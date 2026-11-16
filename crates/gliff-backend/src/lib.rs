//! The seam between `gliff-server` and the desktop it serves.
//!
//! The server talks to the compositor in a handful of places: list and
//! create outputs, set a mode, keep an output awake, capture it, inject
//! input and bridge the clipboard. A [`Backend`] is one way of doing all of
//! that (Hyprland today). The video pipeline, the protocol and the clipboard
//! transfer logic never see which one is in use.
//!
//! Frames, input commands and clipboard events keep the types `hypr-capture`
//! and `hypr-input` define, so every backend produces the same frames.

use std::os::fd::OwnedFd;
use std::sync::atomic::AtomicBool;

pub use anyhow::Result;
pub use hypr_capture::{
    CaptureConfig, CaptureEvent, CaptureMode, CapturedFrame, EventSink as CaptureSink, SizeLimit,
};
pub use hypr_input::{Axis, ClipboardEvent, ClipboardSink, InputCmd, InputConfig};

/// One screen of the served desktop.
#[derive(Debug, Clone)]
pub struct OutputInfo {
    pub name: String,
    /// Physical size, before `transform` is applied.
    pub width: u32,
    pub height: u32,
    pub scale: f32,
    /// A `wl_output` transform; odd values swap width and height.
    pub transform: i32,
    pub x: i32,
    pub y: i32,
    pub focused: bool,
    pub disabled: bool,
}

/// A failed request to create a virtual output.
#[derive(Debug)]
pub struct CreateError {
    pub source: anyhow::Error,
    /// A timed-out request may still finish in the compositor, so the caller
    /// waits for the named output. An explicit rejection must not wait.
    pub may_complete: bool,
}

impl std::fmt::Display for CreateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.source.fmt(f)
    }
}

/// Run periodically on a thread of its own to keep an output switched on.
/// It gets a flag that turns true when the session is ending.
pub type AwakeCheck = Box<dyn FnMut(&AtomicBool) + Send>;

/// Handle to a running capture. Dropping it ends the capture.
pub trait CaptureControl: Send {
    /// Ask for the next frame. Frames are only captured on demand.
    fn request_frame(&self) -> Result<()>;
    /// Abandon the in-flight capture and start a fresh full-damage one.
    fn recapture(&self) -> Result<()>;
    /// Capture subsequent frames into shared memory for CPU encoding.
    fn use_shm(&self) -> Result<()>;
}

/// Handle to the input injector. Dropping it ends it.
pub trait InputControl: Send {
    fn send(&self, cmd: InputCmd) -> Result<()>;
}

/// Handle to the compositor's clipboard. Dropping it ends it.
pub trait ClipboardControl: Send {
    /// Become the selection, offering `mime_types` (empty withdraws it).
    fn offer(&self, mime_types: Vec<String>);
    /// Ask the selection's owner for `mime_type`; the pipe yields the bytes.
    fn receive(&self, mime_type: String) -> Result<OwnedFd>;
}

/// One way of capturing, driving and querying a desktop.
pub trait Backend: Send + Sync {
    fn list_outputs(&self) -> Result<Vec<OutputInfo>>;

    /// Create a private output called `name`. It is removed with
    /// [`Backend::remove_output`].
    fn create_virtual_output(&self, name: &str) -> std::result::Result<(), CreateError>;

    fn set_mode(&self, name: &str, width: u32, height: u32, hz: u32, scale: f32) -> Result<()>;

    fn remove_output(&self, name: &str) -> Result<()>;

    /// Round a requested mode so `scale` divides it into whole logical
    /// pixels, the way this compositor needs.
    fn align_mode(&self, width: u32, height: u32, scale: f32) -> (u32, u32);

    /// The periodic check that keeps `output` awake.
    fn keep_awake(&self, output: &str) -> AwakeCheck;

    /// `config.output` names the output; the backend ignores any other
    /// compositor selection in the config.
    fn start_capture(
        &self,
        config: CaptureConfig,
        sink: CaptureSink,
    ) -> Result<Box<dyn CaptureControl>>;

    fn start_input(&self, config: InputConfig) -> Result<Box<dyn InputControl>>;

    fn start_clipboard(&self, sink: ClipboardSink) -> Result<Box<dyn ClipboardControl>>;
}
