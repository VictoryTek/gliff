//! Capture a Mutter ScreenCast stream as gliff frames.
//!
//! `gnome-ipc` opens the stream and says which PipeWire node carries it; a
//! [`Capturer`] connects to that node and hands the consumer the same
//! [`CaptureEvent`]s the Hyprland capturer does. Frames are copied out of
//! PipeWire into a small ring of memfd buffers, so a PipeWire buffer is
//! returned at once and the consumer can hold a frame as long as it likes.
//! Those copies are CPU-readable. With [`Options::dmabuf`] set the stream is offered
//! as a dmabuf first, and Mutter's buffers are handed on as they are: the GPU
//! imports them, the CPU cannot read them.

mod cursor;
mod output;
mod pacer;
mod pw;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::thread::JoinHandle;

use anyhow::{anyhow, Result};
pub use hypr_capture::{CaptureEvent, CapturedFrame, EventSink, SizeLimit};
pub use output::VirtualOutput;

/// How to capture one stream.
#[derive(Debug, Clone)]
pub struct Options {
    /// The PipeWire node of the stream (`gnome_ipc::Stream::wait_node`).
    pub node: u32,
    /// The user's runtime directory, where the PipeWire socket is.
    pub runtime_dir: PathBuf,
    /// Reported in [`CaptureEvent::Ready`]: the output's name.
    pub name: String,
    /// Frames the consumer may hold at once. One more buffer is kept so the
    /// last frame can be sent again.
    pub buffers: usize,
    /// End the capture with an error when the stream grows past this.
    pub max_size: Option<SizeLimit>,
    /// Ask for this size from the start. Only a virtual monitor follows it.
    /// Without it the stream takes the producer's own size (1920x1080 for a
    /// virtual monitor).
    pub size: Option<(u32, u32)>,
    /// Ask for this scale from the start (a virtual monitor only).
    pub scale: Option<f64>,
    /// The modifiers the consumer can import for each DRM format. With any,
    /// the stream is offered as a dmabuf first and Mutter may pick one of
    /// these; without, or when Mutter picks none, frames come in shared
    /// memory. [`Capturer::use_shm`] drops the offer.
    pub dmabuf: Vec<(drm_fourcc::DrmFourcc, Vec<u64>)>,
    /// Ask for no frame-rate limit. For the stream of an existing monitor
    /// only: Mutter otherwise skips frames that come sooner than the
    /// monitor's refresh interval after the last, which cost a third of the
    /// frames of a 60 Hz monitor. Leave it off for a virtual monitor, whose
    /// refresh rate Mutter takes from this very value (zero gives a 0 Hz
    /// monitor). Needs a Mutter whose monitor offer allows 0 (50.6 does).
    pub unlimited_rate: bool,
}

impl Options {
    pub fn new(node: u32, runtime_dir: impl Into<PathBuf>, name: impl Into<String>) -> Self {
        Self {
            node,
            runtime_dir: runtime_dir.into(),
            name: name.into(),
            buffers: 3,
            max_size: None,
            size: None,
            scale: None,
            dmabuf: Vec::new(),
            unlimited_rate: false,
        }
    }
}

/// What the capture has seen so far.
#[derive(Debug, Default)]
pub struct Stats {
    /// Buffers PipeWire delivered.
    pub buffers: AtomicU64,
    /// Of those, ones with no pixels (cursor-only updates).
    pub empty: AtomicU64,
    /// Frames thrown away because every ring slot was in use or the buffer was
    /// malformed.
    pub dropped: AtomicU64,
    /// Buffers that carried a cursor update (an id of 0 means none).
    pub cursor_meta: AtomicU64,
    /// Times Mutter handed the modifier choice back and we made it (0 when
    /// Mutter fixes the modifier itself).
    pub fixations: AtomicU64,
}

impl Stats {
    pub fn get(counter: &AtomicU64) -> u64 {
        counter.load(Ordering::Relaxed)
    }
}

pub(crate) enum Cmd {
    Request,
    Recapture,
    Release { index: usize, generation: u64 },
    Resize { width: u32, height: u32, scale: f64 },
    UseShm,
    Stop,
}

/// Handle to a running capture. Dropping it ends the capture.
pub struct Capturer {
    tx: pipewire::channel::Sender<Cmd>,
    join: Option<JoinHandle<()>>,
    stats: Arc<Stats>,
}

impl Capturer {
    /// Connect to the stream. Events arrive on `sink` from the capture
    /// thread. Returns once PipeWire is connected.
    pub fn start(options: Options, sink: EventSink) -> Result<Self> {
        pipewire::init();
        let (tx, rx) = pipewire::channel::channel::<Cmd>();
        let stats = Arc::new(Stats::default());
        let (ready_tx, ready_rx) = mpsc::channel();
        let join = std::thread::Builder::new()
            .name("gnome-capture".into())
            .spawn({
                let tx = tx.clone();
                let stats = stats.clone();
                move || pw::run(options, sink, rx, tx, stats, ready_tx)
            })?;
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(Self {
                tx,
                join: Some(join),
                stats,
            }),
            Ok(Err(e)) => {
                let _ = join.join();
                Err(anyhow!(e))
            }
            Err(_) => Err(anyhow!("the capture thread ended before it connected")),
        }
    }

    /// Ask for the next frame. Frames are only sent on demand: the newest
    /// one, or the next to arrive.
    pub fn request_frame(&self) -> Result<()> {
        self.send(Cmd::Request)
    }

    /// Send a full frame now: the newest unsent one, else the last one again.
    pub fn recapture(&self) -> Result<()> {
        self.send(Cmd::Recapture)
    }

    /// Ask the stream for a new size and scale. Only a virtual monitor follows (it is
    /// resized to match); a real monitor keeps its mode. The consumer learns
    /// the result as a new [`CaptureEvent::Ready`], if it comes.
    pub fn resize(&self, width: u32, height: u32, scale: f64) -> Result<()> {
        self.send(Cmd::Resize {
            width,
            height,
            scale,
        })
    }

    /// Stop offering dmabufs and renegotiate to shared memory, for a consumer
    /// that cannot import them. The choice stays across later resizes.
    pub fn use_shm(&self) -> Result<()> {
        self.send(Cmd::UseShm)
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    fn send(&self, cmd: Cmd) -> Result<()> {
        self.tx
            .send(cmd)
            .map_err(|_| anyhow!("the capture thread has ended"))
    }
}

impl Drop for Capturer {
    fn drop(&mut self) {
        let _ = self.tx.send(Cmd::Stop);
        if let Some(j) = self.join.take() {
            let _ = j.join();
        }
    }
}
