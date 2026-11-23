//! gliff's [`Backend`] for a GNOME desktop.
//!
//! It is the `gnome-*` crates put behind the trait `gliff-server` uses. Two
//! things differ from Hyprland and shape the code:
//!
//! * A virtual monitor is not a separate object. Mutter makes it only while a
//!   capture is connected, so `create_virtual_output` starts the capture
//!   ([`VirtualOutput`]) and `start_capture` later attaches the server's sink
//!   to it ([`forward`]). Mutter names the monitor itself (`Meta-0`); the
//!   backend reports it under the name the server asked for.
//! * Input needs the session and stream of the capture, so `start_input`
//!   finds them by output name. The server starts the capture first.
//!
//! Capture is a dmabuf for the GPU pipeline when the server wants one and
//! Vulkan on its render node can import what Mutter offers, and shared memory
//! otherwise (the CPU pipeline, `--capture shm`, or after the server's
//! `use_shm`). A virtual output starts capturing before the server says which,
//! so it starts as a dmabuf on the render node the server would pick by
//! default (`GLIFF_RENDER_NODE`, else `renderD128`) and `start_capture` switches
//! it to shared memory when the server needs that. A server run with another
//! `--render-node` on a headless GNOME session may meet a dmabuf its GPU cannot
//! import; its first GPU encode then fails and it falls back to the CPU
//! pipeline by itself.

mod forward;

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use drm_fourcc::DrmFourcc;
use gliff_backend::{
    AwakeCheck, Backend, CaptureConfig, CaptureControl, CaptureMode, CaptureSink, ClipboardControl,
    ClipboardSink, CreateError, InputCmd, InputConfig, InputControl, OutputInfo,
};
use gliff_vk::Gpu;
use gnome_capture::{Capturer, Options, VirtualOutput};
use gnome_ipc::power::{self, IdleInhibitor};
use gnome_ipc::{mutter, CursorMode, GraphicalSession, Monitor, Session, Stream, StreamSource};
use std::path::{Path, PathBuf};
use zbus::blocking::Connection;

use forward::Forward;

/// The size a virtual output starts at; the server resizes it at once.
const FIRST_SIZE: (u32, u32) = (1920, 1080);

/// Whether two render node paths are the same device.
fn same_node(a: &Path, b: &Path) -> bool {
    a == b
        || matches!(
            (std::fs::canonicalize(a), std::fs::canonicalize(b)),
            (Ok(a), Ok(b)) if a == b
        )
}

/// What a stream offers Mutter: the modifiers the GPU can import per format.
type Offer = Vec<(DrmFourcc, Vec<u64>)>;

pub struct GnomeBackend {
    desktop: GraphicalSession,
    conn: Connection,
    outputs: Mutex<HashMap<String, Slot>>,
    /// The render node a virtual output is offered dmabufs for, before the
    /// server says which it uses.
    default_node: PathBuf,
    /// What Vulkan said for each render node asked about.
    offers: Mutex<HashMap<PathBuf, Offer>>,
    /// The desktop's own keyboard layout, for the input to compare the
    /// viewer's with.
    server_keymap: gnome_input::ServerKeymap,
}

enum Slot {
    /// Owned here until `remove_output`.
    Virtual(Arc<Virtual>),
    /// Owned by the capture and input handles; gone when they are.
    Mirror(Weak<Mirror>),
}

struct Virtual {
    out: VirtualOutput,
    forward: Arc<Mutex<Forward>>,
    /// The render node its dmabufs were offered for.
    node: PathBuf,
}

/// A real monitor being captured.
// Fields drop in order: capture, session, then the inhibition.
struct Mirror {
    connector: String,
    serial: String,
    capturer: Capturer,
    session: Session,
    stream: Stream,
    _inhibitor: IdleInhibitor,
}

/// What a capture or input handle keeps alive.
#[derive(Clone)]
enum Held {
    Virtual(Arc<Virtual>),
    Mirror(Arc<Mirror>),
}

impl Held {
    fn capturer(&self) -> &Capturer {
        match self {
            Held::Virtual(v) => v.out.capturer(),
            Held::Mirror(m) => &m.capturer,
        }
    }

    fn session(&self) -> &Session {
        match self {
            Held::Virtual(v) => v.out.session(),
            Held::Mirror(m) => &m.session,
        }
    }

    fn stream_path(&self) -> &str {
        match self {
            Held::Virtual(v) => v.out.stream_path(),
            Held::Mirror(m) => m.stream.path(),
        }
    }

    fn monitor(&self) -> Result<Monitor> {
        match self {
            Held::Virtual(v) => v.out.monitor(),
            Held::Mirror(m) => m
                .session
                .monitors()?
                .into_iter()
                .find(|mon| mon.connector == m.connector && mon.serial == m.serial)
                .ok_or_else(|| anyhow!("monitor {} is gone", m.connector)),
        }
    }
}

impl GnomeBackend {
    /// Find the user's graphical session and check it is GNOME.
    pub fn discover() -> Result<Self> {
        let desktop = gnome_ipc::find_session().context("find a graphical session")?;
        let conn = mutter::connect(&desktop.bus_address)?;
        let (rd, sc) = mutter::versions(&conn)?;
        tracing::info!(remote_desktop = rd, screen_cast = sc, "GNOME session found");
        let default_node = std::env::var_os("GLIFF_RENDER_NODE")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(hypr_capture::DEFAULT_RENDER_NODE));
        let server_keymap = gnome_input::ServerKeymap::follow(&desktop);
        Ok(Self {
            desktop,
            conn,
            outputs: Mutex::new(HashMap::new()),
            default_node,
            offers: Mutex::new(HashMap::new()),
            server_keymap,
        })
    }

    /// The modifiers Vulkan on `node` can import, for each format Mutter may
    /// use; empty when there is no such GPU. Asked once per node.
    fn dmabuf_offer(&self, node: &Path) -> Offer {
        let mut offers = self.offers.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(offer) = offers.get(node) {
            return offer.clone();
        }
        let offer = match Gpu::open(Some(node)) {
            Ok(gpu) => [
                DrmFourcc::Xrgb8888,
                DrmFourcc::Argb8888,
                DrmFourcc::Xbgr8888,
                DrmFourcc::Abgr8888,
            ]
            .into_iter()
            .map(|f| (f, gpu.dmabuf_modifiers(f)))
            .filter(|(_, mods)| !mods.is_empty())
            .collect(),
            Err(e) => {
                tracing::info!(node = %node.display(), error = %e, "no GPU to import dmabufs; capturing to shared memory");
                Vec::new()
            }
        };
        offers.insert(node.to_path_buf(), offer.clone());
        offer
    }

    fn slots(&self) -> std::sync::MutexGuard<'_, HashMap<String, Slot>> {
        // A panic while holding the map leaves it usable.
        self.outputs.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn held(&self, name: &str) -> Option<Held> {
        match self.slots().get(name)? {
            Slot::Virtual(v) => Some(Held::Virtual(v.clone())),
            Slot::Mirror(m) => m.upgrade().map(Held::Mirror),
        }
    }
}

impl Backend for GnomeBackend {
    fn list_outputs(&self) -> Result<Vec<OutputInfo>> {
        // Our virtual monitors go by the names the server gave them.
        let ours: HashMap<(String, String), String> = self
            .slots()
            .iter()
            .filter_map(|(name, slot)| match slot {
                Slot::Virtual(v) => Some((
                    (v.out.connector().to_string(), v.out.serial().to_string()),
                    name.clone(),
                )),
                Slot::Mirror(_) => None,
            })
            .collect();
        Ok(mutter::monitors(&self.conn)?
            .into_iter()
            .map(|m| OutputInfo {
                name: ours
                    .get(&(m.connector.clone(), m.serial.clone()))
                    .cloned()
                    .unwrap_or(m.connector),
                width: m.width,
                height: m.height,
                scale: m.scale as f32,
                transform: 0,
                x: m.x,
                y: m.y,
                focused: m.primary,
                disabled: false,
            })
            .collect())
    }

    fn create_virtual_output(&self, name: &str) -> std::result::Result<(), CreateError> {
        let fail = |source: anyhow::Error| CreateError {
            source,
            may_complete: false,
        };
        if self.slots().contains_key(name) {
            return Err(fail(anyhow!("output {name} already exists")));
        }
        let forward = Arc::new(Mutex::new(Forward::default()));
        let sink: CaptureSink = {
            let forward = forward.clone();
            Box::new(move |event| {
                forward
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .event(event)
            })
        };
        let offer = self.dmabuf_offer(&self.default_node);
        let out = VirtualOutput::create(
            &self.desktop,
            FIRST_SIZE,
            1.0,
            CursorMode::Metadata,
            offer,
            sink,
        )
        .map_err(fail)?;
        tracing::info!(name, connector = out.connector(), "virtual output created");
        self.slots().insert(
            name.to_string(),
            Slot::Virtual(Arc::new(Virtual {
                out,
                forward,
                node: self.default_node.clone(),
            })),
        );
        Ok(())
    }

    /// The size and the scale are applied, at 60 Hz. Mutter takes the nearest
    /// scale the mode supports (none that would leave under 800x480 logical
    /// pixels), so the session reads the scale back from `list_outputs`. A new
    /// scale at the same size does nothing on Mutter; the session then keeps
    /// the scale it had. See docs/PLAN.md.
    fn set_mode(&self, name: &str, width: u32, height: u32, _hz: u32, scale: f32) -> Result<()> {
        let held = self.held(name).ok_or_else(|| anyhow!("no output {name}"))?;
        let Held::Virtual(v) = held else {
            bail!("{name} is a real monitor; its mode is not changed");
        };
        v.out.resize(width, height, f64::from(scale))
    }

    fn remove_output(&self, name: &str) -> Result<()> {
        let slot = self
            .slots()
            .remove(name)
            .ok_or_else(|| anyhow!("no output {name}"))?;
        match slot {
            // Wait for the monitor to go when nobody else holds it. When a
            // capture or input handle still does, it goes with the handle.
            Slot::Virtual(v) => match Arc::try_unwrap(v) {
                Ok(v) => v.out.close(),
                Err(_) => Ok(()),
            },
            Slot::Mirror(_) => Ok(()),
        }
    }

    fn align_mode(&self, width: u32, height: u32, _scale: f32) -> (u32, u32) {
        (width, height)
    }

    fn keep_awake(&self, _output: &str) -> AwakeCheck {
        // The output already holds an idle inhibitor; this catches a screen
        // that blanks anyway (the user pressed a key to blank it, say).
        let address = self.desktop.bus_address.clone();
        let mut conn: Option<Connection> = None;
        Box::new(move |_cancelled: &AtomicBool| {
            if conn.is_none() {
                conn = mutter::connect(&address).ok();
            }
            let Some(c) = &conn else { return };
            match power::wake(c) {
                Ok(true) => tracing::info!("the screen had blanked; woke it"),
                Ok(false) => {}
                Err(e) => {
                    tracing::debug!(error = %format!("{e:#}"), "cannot check the screen");
                    conn = None;
                }
            }
        })
    }

    fn start_capture(
        &self,
        config: CaptureConfig,
        sink: CaptureSink,
    ) -> Result<Box<dyn CaptureControl>> {
        let name = config.output.clone();
        // The CPU pipeline cannot read a dmabuf; the server says so with
        // `--capture shm` or by allowing the shared-memory fallback.
        let needs_shm = config.mode == CaptureMode::Shm || config.shm_fallback;
        if let Some(held) = self.held(&name) {
            if let Held::Virtual(v) = &held {
                v.forward
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach(sink);
                // A dmabuf made for another GPU than the server encodes on
                // can lose the encoder's device when it is imported (seen
                // between an NVIDIA and an AMD GPU), so that case is shared
                // memory too.
                let other_gpu = !same_node(&v.node, &config.render_node);
                if needs_shm || other_gpu {
                    held.capturer().use_shm()?;
                }
                return Ok(Box::new(GnomeCapture(held)));
            }
        }
        let offer = if needs_shm {
            Offer::new()
        } else {
            self.dmabuf_offer(&config.render_node)
        };
        if config.mode == CaptureMode::Dmabuf && !needs_shm && offer.is_empty() {
            bail!(
                "dmabuf capture needs a GPU Vulkan can import from; none on {}",
                config.render_node.display()
            );
        }
        let monitors = mutter::monitors(&self.conn)?;
        let monitor = monitors
            .iter()
            .find(|m| m.connector == name)
            .ok_or_else(|| anyhow!("no monitor {name}"))?;
        // A blanked screen refuses new sessions; an idle one would end it.
        power::wake(&self.conn).context("wake the screen")?;
        let inhibitor = IdleInhibitor::new(&self.desktop.bus_address, "Remote screen in use")?;
        let session = Session::create(&self.desktop.bus_address)?;
        let cursor = if config.cursor {
            CursorMode::Metadata
        } else {
            CursorMode::Embedded
        };
        let stream = session.record(&StreamSource::Monitor(monitor.connector.clone()), cursor)?;
        session.start()?;
        let node = stream.wait_node(Duration::from_secs(5))?;
        let mut options = Options::new(node, &self.desktop.runtime_dir, name.clone());
        options.buffers = config.buffers;
        options.max_size = config.max_size;
        options.dmabuf = offer;
        options.unlimited_rate = true;
        let capturer = Capturer::start(options, sink)?;
        let mirror = Arc::new(Mirror {
            connector: monitor.connector.clone(),
            serial: monitor.serial.clone(),
            capturer,
            session,
            stream,
            _inhibitor: inhibitor,
        });
        self.slots()
            .insert(name, Slot::Mirror(Arc::downgrade(&mirror)));
        Ok(Box::new(GnomeCapture(Held::Mirror(mirror))))
    }

    fn start_input(&self, config: InputConfig) -> Result<Box<dyn InputControl>> {
        let held = self.held(&config.output).ok_or_else(|| {
            anyhow!(
                "no capture on {} to attach input to (start the capture first)",
                config.output
            )
        })?;
        let m = held.monitor()?;
        let scale = m.scale.max(0.5);
        let extent = (
            (f64::from(m.width) / scale).round().max(1.0) as u32,
            (f64::from(m.height) / scale).round().max(1.0) as u32,
        );
        let input = gnome_input::Input::new(held.session().input(), held.stream_path(), extent)
            .with_server_keymap(self.server_keymap.clone());
        Ok(Box::new(GnomeInput { input, _held: held }))
    }

    fn start_clipboard(&self, sink: ClipboardSink) -> Result<Box<dyn ClipboardControl>> {
        Ok(Box::new(GnomeClipboard(gnome_clipboard::Clipboard::start(
            &self.desktop.bus_address,
            sink,
        )?)))
    }
}

struct GnomeCapture(Held);

impl CaptureControl for GnomeCapture {
    fn request_frame(&self) -> Result<()> {
        self.0.capturer().request_frame()
    }
    fn recapture(&self) -> Result<()> {
        self.0.capturer().recapture()
    }
    fn use_shm(&self) -> Result<()> {
        self.0.capturer().use_shm()
    }
}

// Field order: the input is released before the output it points into.
struct GnomeInput {
    input: gnome_input::Input,
    _held: Held,
}

impl InputControl for GnomeInput {
    fn send(&self, cmd: InputCmd) -> Result<()> {
        self.input.send(cmd)
    }
}

struct GnomeClipboard(gnome_clipboard::Clipboard);

impl ClipboardControl for GnomeClipboard {
    fn offer(&self, mime_types: Vec<String>) {
        self.0.offer(mime_types)
    }
    fn receive(&self, mime_type: String) -> Result<OwnedFd> {
        self.0.receive(mime_type)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_same_render_node_by_another_path_is_the_same_node() {
        let dir = std::env::temp_dir().join(format!("gliff-node-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (a, b, link) = (dir.join("a"), dir.join("b"), dir.join("link"));
        std::fs::write(&a, "").unwrap();
        std::fs::write(&b, "").unwrap();
        std::os::unix::fs::symlink(&a, &link).unwrap();
        assert!(same_node(&a, &a));
        assert!(same_node(&a, &link));
        assert!(!same_node(&a, &b));
        // A path that does not exist is only equal to itself.
        assert!(same_node(Path::new("/nope"), Path::new("/nope")));
        assert!(!same_node(Path::new("/nope"), &a));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
