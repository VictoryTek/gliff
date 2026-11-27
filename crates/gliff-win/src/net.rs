//! The network and decode worker: run `gliff-server --stdio` over ssh, speak
//! the protocol, decode the single 4:2:0 stream on the CPU and hand finished
//! pictures to the window. Input flows the other way as `ClientMsg`s.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use gliff_proto::{
    features, is_incompatible, version_mismatch, ChromaMode, ClientCaps, ClientMsg, Codec,
    ServerMsg, PROTOCOL_VERSION,
};
use gliff_sw::{BgraFrame, Decoder};
use gliff_transport::{spawn_ssh, Framed, SshTarget};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver};
use winit::event_loop::EventLoopProxy;

/// What the window may be asked to decode: the largest stream we advertise.
const MAX_STREAM: (u32, u32) = (3840, 2160);
/// The CPU decoder holds the first picture of a High-profile stream until
/// the next access unit; a still screen sends none, so drain it after this.
const IDLE_DRAIN: Duration = Duration::from_millis(150);
/// How long the last messages (Bye) get to leave before the worker ends.
const FLUSH_WAIT: Duration = Duration::from_secs(1);

/// The geometry a picture was encoded under, which the window needs to map
/// the pointer. It travels with the picture so the two never disagree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    /// Stream size in physical pixels.
    pub stream: (u32, u32),
    /// The size the stream is stretched to before it is fitted to the window.
    pub view: (u32, u32),
    /// Effective stream scale x1000.
    pub scale_milli: u32,
}

pub struct Picture {
    pub bgra: BgraFrame,
    pub geometry: Geometry,
}

/// The newest picture not yet shown. A slow window skips pictures, it never
/// queues them.
pub type Slot = Arc<Mutex<Option<Picture>>>;

pub enum UiEvent {
    /// A new picture waits in the slot.
    Frame,
    Connected {
        headless: bool,
    },
    /// The release hotkey was pressed: the keyboard is Windows' again.
    GrabReleased,
    /// The worker ended, with the reason if it failed.
    Closed(Option<String>),
}

/// Everything the worker reports to the window.
#[derive(Clone)]
pub struct Ui {
    proxy: EventLoopProxy<UiEvent>,
    slot: Slot,
    /// Set while a `UiEvent::Frame` is on its way, so a fast stream cannot
    /// flood the event loop.
    notified: Arc<AtomicBool>,
}

impl Ui {
    pub fn new(proxy: EventLoopProxy<UiEvent>) -> (Self, Slot, Arc<AtomicBool>) {
        let slot = Slot::default();
        let notified = Arc::new(AtomicBool::new(false));
        let ui = Self {
            proxy,
            slot: slot.clone(),
            notified: notified.clone(),
        };
        (ui, slot, notified)
    }

    fn publish(&self, picture: Picture) {
        *self.slot.lock().unwrap_or_else(|e| e.into_inner()) = Some(picture);
        if !self.notified.swap(true, Ordering::AcqRel) {
            let _ = self.proxy.send_event(UiEvent::Frame);
        }
    }

    fn send(&self, event: UiEvent) {
        let _ = self.proxy.send_event(event);
    }
}

/// Run the worker on its own thread. It ends when the connection does, or
/// when `input` is closed.
pub fn spawn(
    target: SshTarget,
    ui: Ui,
    input: UnboundedReceiver<ClientMsg>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("gliff-net".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    ui.send(UiEvent::Closed(Some(format!("runtime: {e}"))));
                    return;
                }
            };
            let result = rt.block_on(async {
                let ssh = spawn_ssh(&target).context("cannot run ssh")?;
                // `ssh.child` stays alive until the session ends; dropping it
                // kills ssh.
                let _child = ssh.child;
                session(ssh.stdout, ssh.stdin, &ui, input).await
            });
            ui.send(UiEvent::Closed(result.err().map(|e| format!("{e:#}"))));
        })
}

struct Config {
    geometry: Geometry,
    chroma: ChromaMode,
}

impl Config {
    fn new(
        stream: (u32, u32),
        chroma: ChromaMode,
        scale_milli: u32,
        view: (u32, u32),
    ) -> Result<Self> {
        let sane = |(w, h): (u32, u32)| w > 0 && h > 0 && w <= MAX_STREAM.0 && h <= MAX_STREAM.1;
        if !sane(stream) || !sane(view) || scale_milli == 0 {
            bail!("server sent a stream of {stream:?} (view {view:?}, scale {scale_milli})");
        }
        if chroma != ChromaMode::Single420 {
            bail!("server sent {chroma:?}, but this viewer decodes only Single420");
        }
        Ok(Self {
            geometry: Geometry {
                stream,
                view,
                scale_milli,
            },
            chroma,
        })
    }

    /// Whether the coded stream changed, which needs a new decoder; a change
    /// of scale or view alone does not.
    fn recodes(&self, next: &Config) -> bool {
        self.geometry.stream != next.geometry.stream || self.chroma != next.chroma
    }
}

async fn session<R, W>(rd: R, wr: W, ui: &Ui, mut input: UnboundedReceiver<ClientMsg>) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin + Send + 'static,
{
    let mut reader = Framed::new(rd);
    let mut writer = Framed::new(wr);

    writer
        .write_msg(&ClientMsg::Hello {
            version: PROTOCOL_VERSION,
            // No XKB text from Windows: the server falls back to its own
            // default layout, and keys are named by position.
            keymap: String::new(),
            caps: ClientCaps {
                codecs: vec![Codec::H264],
                max_width: MAX_STREAM.0,
                max_height: MAX_STREAM.1,
                chroma: vec![ChromaMode::Single420],
                features: features(),
            },
        })
        .await?;

    // The server may Ping between HelloAck and StreamConfig; answer at once.
    // Closing the window cancels the wait (ssh may be at a password prompt);
    // input typed before the connection is up is dropped.
    let handshake = async {
        let mut got_ack = false;
        let mut headless = false;
        loop {
            match reader.read_msg::<ServerMsg>().await? {
                ServerMsg::HelloAck {
                    version, session, ..
                } => {
                    if version != PROTOCOL_VERSION {
                        bail!("{}", version_mismatch(PROTOCOL_VERSION, version));
                    }
                    headless = session.headless;
                    got_ack = true;
                }
                ServerMsg::Ping { t } => writer.write_msg(&ClientMsg::Pong { t }).await?,
                ServerMsg::StreamConfig {
                    width,
                    height,
                    chroma,
                    scale_milli,
                    view_width,
                    view_height,
                    ..
                } if got_ack => {
                    let cfg = Config::new(
                        (width, height),
                        chroma,
                        scale_milli,
                        (view_width, view_height),
                    )?;
                    return Ok::<_, anyhow::Error>((cfg, headless));
                }
                ServerMsg::Error { message, .. } => bail!("server: {message}"),
                other => bail!("unexpected message before StreamConfig: {other:?}"),
            }
        }
    };
    let (mut cfg, headless) = tokio::select! {
        done = handshake => done?,
        _ = async { while input.recv().await.is_some() {} } => return Ok(()),
    };
    let (w, h) = cfg.geometry.stream;
    let mut decoder = Decoder::new(false, w, h).context("video decoder")?;
    tracing::info!(?cfg.geometry, headless, "connected");
    ui.send(UiEvent::Connected { headless });

    // Writes run on their own task so reading never waits on a write.
    let (out_tx, mut out_rx) = unbounded_channel::<ClientMsg>();
    let mut writes = tokio::spawn(async move {
        while let Some(m) = out_rx.recv().await {
            writer.write_msg(&m).await?;
        }
        Ok::<(), gliff_transport::Error>(())
    });
    // The window's input joins the same channel. When the window closes its
    // end, the session is over.
    let (ui_gone_tx, mut ui_gone) = tokio::sync::oneshot::channel::<()>();
    {
        let out_tx = out_tx.clone();
        tokio::spawn(async move {
            while let Some(m) = input.recv().await {
                if out_tx.send(m).is_err() {
                    return;
                }
            }
            let _ = ui_gone_tx.send(());
        });
    }

    let mut drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
    let mut logged_first = false;
    // Geometry of the access units inside the decoder, oldest first: the CPU
    // decoder returns the PREVIOUS unit's picture, and a scale-only config
    // can land in between, so a picture takes the geometry it was sent under.
    let mut in_decoder: VecDeque<Geometry> = VecDeque::new();
    let publish = |bgra: BgraFrame, geometry: Geometry, logged: &mut bool| {
        if !*logged {
            tracing::info!("first frame decoded");
            *logged = true;
        }
        ui.publish(Picture { bgra, geometry });
    };
    loop {
        let read = tokio::select! {
            read = reader.read_msg::<ServerMsg>() => read,
            _ = &mut ui_gone => {
                // Let the Bye the window queued leave before ssh is killed.
                drop(out_tx);
                let _ = tokio::time::timeout(FLUSH_WAIT, &mut writes).await;
                return Ok(());
            }
            written = &mut writes => {
                written.context("writer task")?.context("write to server")?;
                return Ok(());
            }
            _ = tokio::time::sleep_until(drain_at), if decoder.has_pending() => {
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                match decoder.flush() {
                    Ok(Some(frame)) => {
                        let geometry = in_decoder.pop_front().unwrap_or(cfg.geometry);
                        publish(frame, geometry, &mut logged_first);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        in_decoder.clear();
                        tracing::warn!(error = %e, "idle drain failed; requesting keyframe");
                        let _ = out_tx.send(ClientMsg::RequestKeyframe);
                    }
                }
                continue;
            }
        };
        let msg = match read {
            Ok(m) => m,
            Err(gliff_transport::Error::Closed) => return Ok(()),
            Err(e) => return Err(e.into()),
        };
        match msg {
            ServerMsg::VideoFrame {
                frame_id,
                data_len,
                aux_len,
                ..
            } => {
                let main = reader.read_payload(data_len).await?;
                let aux = if aux_len > 0 {
                    reader.read_payload(aux_len).await?
                } else {
                    Bytes::new()
                };
                drain_at = tokio::time::Instant::now() + IDLE_DRAIN;
                in_decoder.push_back(cfg.geometry);
                let decoded = decoder.decode(&main, &aux);
                // Ack at once so the server keeps pacing.
                let _ = out_tx.send(ClientMsg::FrameAck {
                    frame_id,
                    decoded_at_ms: now_ms(),
                });
                match decoded {
                    Ok(Some(frame)) => {
                        let geometry = in_decoder.pop_front().unwrap_or(cfg.geometry);
                        publish(frame, geometry, &mut logged_first);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        in_decoder.clear();
                        tracing::warn!(error = %e, "decode error; requesting keyframe");
                        let _ = out_tx.send(ClientMsg::RequestKeyframe);
                    }
                }
            }
            ServerMsg::StreamConfig {
                width,
                height,
                chroma,
                scale_milli,
                view_width,
                view_height,
                ..
            } => {
                let next = Config::new(
                    (width, height),
                    chroma,
                    scale_milli,
                    (view_width, view_height),
                )?;
                if cfg.recodes(&next) {
                    decoder = Decoder::new(false, width, height).context("video decoder")?;
                    in_decoder.clear();
                }
                cfg = next;
            }
            ServerMsg::Ping { t } => {
                let _ = out_tx.send(ClientMsg::Pong { t });
            }
            ServerMsg::Error { code, message, .. } => {
                if is_incompatible(code) {
                    bail!("server: {message}");
                }
                bail!("server error {code}: {message}");
            }
            // Cursor and clipboard messages are not handled yet; their
            // payloads are skipped by the next read.
            _ => {}
        }
    }
}

fn now_ms() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stream_of_another_kind_is_refused() {
        assert!(Config::new((1920, 1080), ChromaMode::Native444, 1000, (1920, 1080)).is_err());
        assert!(Config::new((1920, 1080), ChromaMode::Dual420, 1000, (1920, 1080)).is_err());
    }

    #[test]
    fn a_stream_with_no_size_or_scale_is_refused() {
        assert!(Config::new((0, 1080), ChromaMode::Single420, 1000, (1920, 1080)).is_err());
        assert!(Config::new((1920, 1080), ChromaMode::Single420, 0, (1920, 1080)).is_err());
        assert!(Config::new((8000, 1080), ChromaMode::Single420, 1000, (1920, 1080)).is_err());
    }

    #[test]
    fn only_a_new_coded_stream_needs_a_new_decoder() {
        let a = Config::new((1920, 1080), ChromaMode::Single420, 1000, (1920, 1080)).unwrap();
        let rescaled = Config::new((1920, 1080), ChromaMode::Single420, 2000, (960, 540)).unwrap();
        let resized = Config::new((1280, 720), ChromaMode::Single420, 1000, (1280, 720)).unwrap();
        assert!(!a.recodes(&rescaled));
        assert!(a.recodes(&resized));
    }
}
