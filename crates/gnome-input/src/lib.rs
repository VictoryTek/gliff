//! Input for a GNOME desktop: gliff's [`InputCmd`]s become Mutter RemoteDesktop
//! calls.
//!
//! [`translate`] is the pure part: it keeps track of what is held down and
//! turns one command into the calls to make. [`Input`] makes them.
//!
//! Keys are sent as evdev keycodes, which Mutter reads with the *server's*
//! layout, as long as the viewer's layout is that one (or the viewer has not
//! sent one). When the viewer's layout differs from the server's (read from the
//! compositor, see [`ServerKeymap`]), keys are sent as the keysyms the viewer's
//! layout makes (see [`keymap`]), so the characters are the viewer's.

mod keymap;
mod translate;

use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Result};
use gnome_ipc::{GraphicalSession, InputPort};
pub use hypr_input::{Axis, InputCmd};
pub use keymap::Mode as KeyMode;
pub use translate::{Call, Translator};

/// The server's own keyboard layout, as its compositor serves it to a client:
/// what Mutter reads key codes with. Clone it to share it; every clone sees
/// the same layout, and a change when the user switches layouts.
#[derive(Clone, Default)]
pub struct ServerKeymap(Arc<Mutex<(u64, Option<String>)>>);

impl ServerKeymap {
    /// Not known: keys go as key codes.
    pub fn unknown() -> Self {
        Self::default()
    }

    /// A layout given as xkb text (tests and probes).
    pub fn fixed(text: &str) -> Self {
        let keymap = Self::default();
        keymap.set(text.to_owned());
        keymap
    }

    /// Follow the layout of `desktop`'s compositor on a thread of its own.
    /// Stays unknown (with a warning) when its Wayland socket cannot be read.
    pub fn follow(desktop: &GraphicalSession) -> Self {
        let keymap = Self::default();
        let Some(socket) = wayland_socket(&desktop.runtime_dir) else {
            tracing::warn!("no Wayland socket in the runtime directory; keys go as key codes");
            return keymap;
        };
        let target = hypr_input::Target {
            display: Some(socket.to_string_lossy().into_owned()),
            instance: None,
        };
        let sink = keymap.clone();
        if let Err(e) = hypr_input::watch_keymap(target, Box::new(move |text| sink.set(text))) {
            tracing::warn!(error = %e, "cannot follow the server's keymap; keys go as key codes");
        }
        keymap
    }

    /// Whether the layout has arrived.
    pub fn is_known(&self) -> bool {
        self.snapshot().1.is_some()
    }

    /// The layout as xkb text, once it has arrived (probes print it).
    pub fn text(&self) -> Option<String> {
        self.snapshot().1
    }

    fn set(&self, text: String) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.0 += 1;
        inner.1 = Some(text);
    }

    fn snapshot(&self) -> (u64, Option<String>) {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The Wayland socket the compositor listens on: `wayland-0`, `wayland-1`...
fn wayland_socket(runtime_dir: &Path) -> Option<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(runtime_dir)
        .ok()?
        .filter_map(|e| e.ok())
        .filter(|e| {
            let name = e.file_name();
            let name = name.to_string_lossy();
            name.strip_prefix("wayland-")
                .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
        })
        .map(|e| e.path())
        .collect();
    found.sort();
    found.into_iter().next()
}

/// Sends [`InputCmd`]s to a started session.
pub struct Input {
    port: InputPort,
    stream: String,
    state: Mutex<Translator>,
    server: ServerKeymap,
}

impl Input {
    /// `stream` is the D-Bus path of the stream the pointer is positioned in
    /// (`gnome_ipc::Stream::path`); `extent` is its logical size.
    pub fn new(port: InputPort, stream: impl Into<String>, extent: (u32, u32)) -> Self {
        Self {
            port,
            stream: stream.into(),
            state: Mutex::new(Translator::new(extent)),
            server: ServerKeymap::unknown(),
        }
    }

    /// Follow the server's layout, to know when the viewer's differs from it.
    pub fn with_server_keymap(mut self, server: ServerKeymap) -> Self {
        self.server = server;
        self
    }

    /// How keys go now, once a viewer layout has been set (probes print it).
    pub fn key_mode(&self) -> Option<KeyMode> {
        self.state.lock().ok()?.key_mode()
    }

    pub fn send(&self, cmd: InputCmd) -> Result<()> {
        // A new viewer layout starts with the keypad as the server has it.
        let num_lock = match cmd {
            InputCmd::SetKeymap(_) => self.port.lock_state().ok().map(|(_, num)| num),
            _ => None,
        };
        let (generation, server) = self.server.snapshot();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("input state poisoned"))?;
        if let Some(on) = num_lock {
            state.set_num_lock(on);
        }
        // Releases for keys held under the old mode go out even if the
        // command itself fails.
        let mut calls = state.set_server_keymap(generation, server);
        let translated = state.translate(cmd);
        drop(state);
        if let Ok(more) = &translated {
            calls.extend(more.iter().cloned());
        }
        let applied = calls.into_iter().try_for_each(|c| self.apply(c));
        translated?;
        applied
    }

    fn apply(&self, call: Call) -> Result<()> {
        match call {
            Call::Key { code, pressed } => self.port.key(code, pressed),
            Call::Keysym { sym, pressed } => self.port.keysym(sym, pressed),
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
