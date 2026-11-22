//! Input injection into a started RemoteDesktop session.

use anyhow::{Context, Result};
use zbus::blocking::Proxy;

/// `NotifyPointerAxis` flags.
pub const AXIS_FINISH: u32 = 1;
pub const AXIS_SOURCE_WHEEL: u32 = 2;
pub const AXIS_SOURCE_FINGER: u32 = 4;

/// The input calls of a RemoteDesktop session, from [`crate::Session::input`].
///
/// Cheap to clone. Calls fail once the session has ended. Mutter only takes
/// input after the session has started.
#[derive(Clone)]
pub struct InputPort {
    pub(crate) rd: Proxy<'static>,
}

impl InputPort {
    /// A key by evdev keycode, as the session's own keymap reads it.
    pub fn key(&self, code: u32, pressed: bool) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyKeyboardKeycode", &(code, pressed))
            .context("NotifyKeyboardKeycode")
    }

    /// A key by keysym: Mutter finds the key for it in the session's keymap,
    /// or makes one, whatever the layout is.
    pub fn keysym(&self, sym: u32, pressed: bool) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyKeyboardKeysym", &(sym, pressed))
            .context("NotifyKeyboardKeysym")
    }

    /// A pointer button by evdev `BTN_*` code.
    pub fn button(&self, button: u32, pressed: bool) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyPointerButton", &(button as i32, pressed))
            .context("NotifyPointerButton")
    }

    /// Smooth scrolling; 10.0 is one wheel step. `flags` are `AXIS_*`.
    pub fn axis(&self, dx: f64, dy: f64, flags: u32) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyPointerAxis", &(dx, dy, flags))
            .context("NotifyPointerAxis")
    }

    /// Wheel steps: `axis` 0 is vertical, 1 horizontal.
    pub fn axis_discrete(&self, axis: u32, steps: i32) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyPointerAxisDiscrete", &(axis, steps))
            .context("NotifyPointerAxisDiscrete")
    }

    /// Move the pointer to `x`, `y` within the stream `stream` (its D-Bus path).
    pub fn motion_absolute(&self, stream: &str, x: f64, y: f64) -> Result<()> {
        self.rd
            .call::<_, _, ()>("NotifyPointerMotionAbsolute", &(stream, x, y))
            .context("NotifyPointerMotionAbsolute")
    }

    /// Caps Lock and Num Lock as the session's keyboard sees them.
    pub fn lock_state(&self) -> Result<(bool, bool)> {
        let caps = self.rd.get_property::<bool>("CapsLockState")?;
        let num = self.rd.get_property::<bool>("NumLockState")?;
        Ok((caps, num))
    }
}
