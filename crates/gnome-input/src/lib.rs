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

mod active;
mod keymap;
mod translate;

use std::path::Path;
use std::sync::{Arc, Mutex, Weak};

use anyhow::{anyhow, Result};
use gnome_ipc::{GraphicalSession, InputPort};
pub use hypr_input::{Axis, InputCmd};
pub use keymap::Mode as KeyMode;
pub use translate::{Call, Translator};

/// The server's own keyboard layout, as its compositor serves it to a client:
/// what Mutter reads key codes with. Clone it to share it; every clone sees
/// the same layout, and a change when the user switches layouts.
#[derive(Clone, Default)]
pub struct ServerKeymap(Arc<Mutex<Inner>>);

#[derive(Clone, Default)]
pub(crate) struct Inner {
    /// Counts the keymaps that have arrived.
    generation: u64,
    text: Option<String>,
    /// How many layouts `text` has, and their names (empty when the text does
    /// not name them, or there is only one layout).
    layouts: u32,
    groups: Vec<String>,
    /// Which of the keymap's layouts is in use, when known. Cleared when a
    /// new keymap arrives: the groups may have been reordered.
    active: Option<active::ActiveLayout>,
}

/// How many layouts (groups) a keymap text has; 0 if it does not compile.
fn layout_count(text: &str) -> u32 {
    use xkbcommon::xkb;
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(
        &ctx,
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
    .map_or(0, |k| k.num_layouts())
}

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
        // The watcher lives as long as its compositor connection; it holds the
        // keymap only weakly, so dropping the backend ends the layout poller.
        let sink = keymap.downgrade();
        let watch = Box::new(move |text: String| {
            if let Some(keymap) = ServerKeymap::upgrade(&sink) {
                keymap.set(text);
            }
        });
        if let Err(e) = hypr_input::watch_keymap(target, watch) {
            tracing::warn!(error = %e, "cannot follow the server's keymap; keys go as key codes");
        }
        active::follow(desktop.clone(), &keymap);
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

    /// Which layout is in use, as "group of layouts" (probes print it).
    pub fn active_layout(&self) -> Option<(u32, u32)> {
        self.snapshot().2.map(|a| (a.group, a.of))
    }

    /// A layout given as xkb text, with the one in use (tests and probes).
    pub fn fixed_active(text: &str, group: u32, of: u32) -> Self {
        let keymap = Self::fixed(text);
        let generation = keymap.snapshot().0;
        keymap.set_active(generation, Some(active::ActiveLayout { group, of }));
        keymap
    }

    fn set(&self, text: String) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        inner.generation += 1;
        inner.layouts = layout_count(&text);
        inner.groups = if inner.layouts >= 2 {
            active::group_names(&text, inner.layouts).unwrap_or_default()
        } else {
            Vec::new()
        };
        inner.text = Some(text);
        inner.active = None;
    }

    /// The layout in use, as read for the keymap of `generation`; dropped if
    /// a newer keymap has arrived since.
    fn set_active(&self, generation: u64, active: Option<active::ActiveLayout>) {
        let mut inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if inner.generation == generation {
            inner.active = active;
        }
    }

    /// The keymap's version, its number of layouts and their names, without
    /// its text.
    fn generation_and_groups(&self) -> (u64, u32, Vec<String>) {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (inner.generation, inner.layouts, inner.groups.clone())
    }

    fn snapshot(&self) -> (u64, Option<String>, Option<active::ActiveLayout>) {
        let inner = self.0.lock().unwrap_or_else(|e| e.into_inner());
        (inner.generation, inner.text.clone(), inner.active)
    }

    fn downgrade(&self) -> Weak<Mutex<Inner>> {
        Arc::downgrade(&self.0)
    }

    fn upgrade(weak: &Weak<Mutex<Inner>>) -> Option<Self> {
        weak.upgrade().map(Self)
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
        let (generation, server, active) = self.server.snapshot();
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow!("input state poisoned"))?;
        if let Some(on) = num_lock {
            state.set_num_lock(on);
        }
        // Releases for keys held under the old mode go out even if the
        // command itself fails.
        let mut calls = state.update_server(generation, server, active);
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

#[cfg(test)]
mod tests {
    use super::*;
    use xkbcommon::xkb;

    fn layout(name: &str) -> String {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&ctx, "", "", name, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .expect("compile")
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }

    #[test]
    fn the_layout_in_use_belongs_to_the_keymap_it_was_read_for() {
        let keymap = ServerKeymap::fixed(&layout("us,de"));
        let (generation, layouts, groups) = keymap.generation_and_groups();
        assert_eq!(layouts, 2);
        assert_eq!(groups, ["us", "de"]);
        let group_1 = Some(active::ActiveLayout { group: 1, of: 2 });
        keymap.set_active(generation, group_1);
        assert_eq!(keymap.active_layout(), Some((1, 2)));
        // A new keymap (the groups may have moved) clears it, and a result
        // read for the old one is not taken.
        keymap.set(layout("de,us"));
        assert_eq!(keymap.active_layout(), None);
        keymap.set_active(generation, group_1);
        assert_eq!(keymap.active_layout(), None);
        let (generation, _, groups) = keymap.generation_and_groups();
        assert_eq!(groups, ["de", "us"]);
        keymap.set_active(generation, group_1);
        assert_eq!(keymap.active_layout(), Some((1, 2)));
    }

    #[test]
    fn a_keymap_with_one_layout_has_no_layout_choice_to_follow() {
        let keymap = ServerKeymap::fixed(&layout("us"));
        assert_eq!(keymap.generation_and_groups(), (1, 1, Vec::new()));
        assert_eq!(
            ServerKeymap::unknown().generation_and_groups(),
            (0, 0, Vec::new())
        );
    }
}
