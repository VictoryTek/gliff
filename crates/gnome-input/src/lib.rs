//! Input for a GNOME desktop: gliff's [`InputCmd`]s become Mutter RemoteDesktop
//! calls.
//!
//! [`translate`] is the pure part: it keeps track of what is held down and
//! turns one command into the calls to make. [`Input`] makes them.
//!
//! Keys are sent as evdev keycodes, which Mutter reads with the *server's*
//! keymap, so a viewer with a different layout types the wrong characters
//! until keymap forwarding exists (`SetKeymap` is refused for now).

mod translate;

use std::sync::Mutex;

use anyhow::{anyhow, Result};
use gnome_ipc::InputPort;
pub use hypr_input::{Axis, InputCmd};
pub use translate::{Call, Translator};

/// Sends [`InputCmd`]s to a started session.
pub struct Input {
    port: InputPort,
    stream: String,
    state: Mutex<Translator>,
}

impl Input {
    /// `stream` is the D-Bus path of the stream the pointer is positioned in
    /// (`gnome_ipc::Stream::path`); `extent` is its logical size.
    pub fn new(port: InputPort, stream: impl Into<String>, extent: (u32, u32)) -> Self {
        Self {
            port,
            stream: stream.into(),
            state: Mutex::new(Translator::new(extent)),
        }
    }

    pub fn send(&self, cmd: InputCmd) -> Result<()> {
        let calls = self
            .state
            .lock()
            .map_err(|_| anyhow!("input state poisoned"))?
            .translate(cmd)?;
        calls.into_iter().try_for_each(|c| self.apply(c))
    }

    fn apply(&self, call: Call) -> Result<()> {
        match call {
            Call::Key { code, pressed } => self.port.key(code, pressed),
            Call::Button { button, pressed } => self.port.button(button, pressed),
            Call::Motion { x, y } => self.port.motion_absolute(&self.stream, x, y),
            Call::Axis { dx, dy, flags } => self.port.axis(dx, dy, flags),
            Call::AxisDiscrete { axis, steps } => self.port.axis_discrete(axis, steps),
        }
    }
}

impl Drop for Input {
    /// Do not leave a key or button held when the input goes away.
    fn drop(&mut self) {
        let _ = self.send(InputCmd::ReleaseAll);
    }
}
