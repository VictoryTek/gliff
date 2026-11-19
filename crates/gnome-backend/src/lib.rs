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
//! Capture is shared memory only for now, so [`Backend::forced_capture_mode`]
//! asks the server for the CPU pipeline.

mod forward;

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use gliff_backend::{
    AwakeCheck, Backend, CaptureConfig, CaptureControl, CaptureMode, CaptureSink, ClipboardControl,
    ClipboardSink, CreateError, InputCmd, InputConfig, InputControl, OutputInfo,
};
use gnome_capture::{Capturer, Options, VirtualOutput};
use gnome_ipc::power::{self, IdleInhibitor};
use gnome_ipc::{mutter, CursorMode, GraphicalSession, Monitor, Session, Stream, StreamSource};
use zbus::blocking::Connection;

use forward::Forward;

/// The size a virtual output starts at; the server resizes it at once.
const FIRST_SIZE: (u32, u32) = (1920, 1080);

pub struct GnomeBackend {
    desktop: GraphicalSession,
    conn: Connection,
    outputs: Mutex<HashMap<String, Slot>>,
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
        Ok(Self {
            desktop,
            conn,
            outputs: Mutex::new(HashMap::new()),
        })
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
        let out = VirtualOutput::create(&self.desktop, FIRST_SIZE, 1.0, CursorMode::Metadata, sink)
            .map_err(fail)?;
        tracing::info!(name, connector = out.connector(), "virtual output created");
        self.slots().insert(
            name.to_string(),
            Slot::Virtual(Arc::new(Virtual { out, forward })),
        );
        Ok(())
    }

    /// Only the size is applied; a virtual output stays at scale 1 and 60 Hz.
    /// `VirtualOutput` can ask Mutter for another scale, but it is held back:
    /// on the software-rendered test VM, creating a 2560x1440 monitor at
    /// scale 1.75 crashed GNOME Shell and ended the session, and a scale
    /// change at the same size has no effect. See docs/PLAN.md.
    fn set_mode(&self, name: &str, width: u32, height: u32, _hz: u32, _scale: f32) -> Result<()> {
        let held = self.held(name).ok_or_else(|| anyhow!("no output {name}"))?;
        let Held::Virtual(v) = held else {
            bail!("{name} is a real monitor; its mode is not changed");
        };
        v.out.resize(width, height, 1.0)
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
        if let Some(held) = self.held(&name) {
            if let Held::Virtual(v) = &held {
                v.forward
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach(sink);
                return Ok(Box::new(GnomeCapture(held)));
            }
        }
        if config.mode == CaptureMode::Dmabuf {
            bail!("dmabuf capture is not available on GNOME yet (use --capture auto or shm)");
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
        let input = gnome_input::Input::new(held.session().input(), held.stream_path(), extent);
        Ok(Box::new(GnomeInput { input, _held: held }))
    }

    fn start_clipboard(&self, sink: ClipboardSink) -> Result<Box<dyn ClipboardControl>> {
        Ok(Box::new(GnomeClipboard(gnome_clipboard::Clipboard::start(
            &self.desktop.bus_address,
            sink,
        )?)))
    }

    fn forced_capture_mode(&self) -> Option<CaptureMode> {
        Some(CaptureMode::Shm)
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
    /// Capture is already shared memory.
    fn use_shm(&self) -> Result<()> {
        Ok(())
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
