//! The Hyprland backend: the compositor calls `session.rs` used to make
//! directly, moved here unchanged behind [`gliff_backend::Backend`].

use std::os::fd::OwnedFd;
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, Result};
use gliff_backend::{
    AwakeCheck, Backend, CaptureConfig, CaptureControl, CaptureSink, ClipboardControl,
    ClipboardSink, CreateError, InputCmd, InputConfig, InputControl, OutputInfo,
};
use hypr_capture::Capturer;
use hypr_input::{Clipboard, Input};
use hypr_wl::Target;

pub struct HyprBackend {
    instance: hypr_ipc::Instance,
    target: Target,
}

impl HyprBackend {
    pub fn discover(target: &Target) -> Result<Self> {
        Ok(Self {
            instance: target.instance().context("find Hyprland instance")?,
            target: target.clone(),
        })
    }
}

/// A timed-out request may still finish in the compositor. An explicit
/// rejection must fail immediately instead of waiting for an unrelated output.
fn creation_may_complete(error: &hypr_ipc::Error) -> bool {
    matches!(error, hypr_ipc::Error::Io(e)
        if matches!(e.kind(), std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock))
}

impl Backend for HyprBackend {
    fn list_outputs(&self) -> Result<Vec<OutputInfo>> {
        Ok(self
            .instance
            .monitors()?
            .into_iter()
            .map(|m| OutputInfo {
                name: m.name,
                width: m.width,
                height: m.height,
                scale: m.scale,
                transform: m.transform,
                x: m.x,
                y: m.y,
                focused: m.focused,
                disabled: m.disabled,
            })
            .collect())
    }

    fn create_virtual_output(&self, name: &str) -> std::result::Result<(), CreateError> {
        self.instance
            .create_headless_output(name)
            .map_err(|e| CreateError {
                may_complete: creation_may_complete(&e),
                source: e.into(),
            })
    }

    fn set_mode(&self, name: &str, width: u32, height: u32, hz: u32, scale: f32) -> Result<()> {
        Ok(self
            .instance
            .set_monitor_mode(name, width, height, hz, scale)?)
    }

    fn remove_output(&self, name: &str) -> Result<()> {
        Ok(self.instance.remove_output(name)?)
    }

    fn align_mode(&self, width: u32, height: u32, scale: f32) -> (u32, u32) {
        hypr_ipc::clean_mode(width, height, scale)
    }

    // Blocking compositor IPC runs on the caller's own thread, off the
    // single-threaded streaming runtime.
    fn keep_awake(&self, output: &str) -> AwakeCheck {
        let instance = self.instance.clone();
        let name = output.to_string();
        Box::new(move |cancelled: &AtomicBool| match instance.monitors() {
            Ok(mons) if mons.iter().any(|m| m.name == name && !m.dpms_status) => {
                if let Err(e) =
                    instance.set_dpms_if(&name, true, || !cancelled.load(Ordering::Acquire))
                {
                    tracing::warn!(error = %e, "could not switch the headless output on");
                }
            }
            Ok(_) => {}
            Err(e) => tracing::debug!(error = %e, "cannot read the output's DPMS state"),
        })
    }

    fn start_capture(
        &self,
        mut config: CaptureConfig,
        sink: CaptureSink,
    ) -> Result<Box<dyn CaptureControl>> {
        config.target = self.target.clone();
        Ok(Box::new(HyprCapture(Capturer::start(config, sink)?)))
    }

    fn start_input(&self, mut config: InputConfig) -> Result<Box<dyn InputControl>> {
        config.target = self.target.clone();
        Ok(Box::new(HyprInput(Input::start(config, Box::new(|_| {}))?)))
    }

    fn start_clipboard(&self, sink: ClipboardSink) -> Result<Box<dyn ClipboardControl>> {
        Ok(Box::new(HyprClipboard(Clipboard::start(
            self.target.clone(),
            sink,
        )?)))
    }
}

// Newtypes: the trait and the `hypr_*` types are both foreign to this crate.
struct HyprCapture(Capturer);
struct HyprInput(Input);
struct HyprClipboard(Clipboard);

impl CaptureControl for HyprCapture {
    fn request_frame(&self) -> Result<()> {
        Ok(self.0.request_frame()?)
    }
    fn recapture(&self) -> Result<()> {
        Ok(self.0.recapture()?)
    }
    fn use_shm(&self) -> Result<()> {
        Ok(self.0.use_shm()?)
    }
}

impl InputControl for HyprInput {
    fn send(&self, cmd: InputCmd) -> Result<()> {
        Ok(self.0.send(cmd)?)
    }
}

impl ClipboardControl for HyprClipboard {
    fn offer(&self, mime_types: Vec<String>) {
        self.0.offer(mime_types)
    }
    fn receive(&self, mime_type: String) -> Result<OwnedFd> {
        Ok(self.0.receive(mime_type)?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_timeout_may_still_complete() {
        assert!(creation_may_complete(&hypr_ipc::Error::Io(
            std::io::ErrorKind::TimedOut.into()
        )));
        assert!(creation_may_complete(&hypr_ipc::Error::Io(
            std::io::ErrorKind::WouldBlock.into()
        )));
        assert!(!creation_may_complete(&hypr_ipc::Error::Io(
            std::io::ErrorKind::PermissionDenied.into()
        )));
        assert!(!creation_may_complete(&hypr_ipc::Error::Command {
            command: "output create".into(),
            response: "refused".into()
        }));
    }
}
