//! The shortcut grab. Windows takes Win, Alt+Tab and the like before a
//! window sees them, so while the viewer has focus a low-level keyboard hook
//! swallows every key and sends it to the remote machine instead.
//! Shift+Escape gives the keyboard back to Windows until the window is
//! clicked, as in the GTK viewer; losing focus gives it back too.
//!
//! The hook runs on its own thread. [`Filter`] holds the decisions and has
//! no Windows in it, so they are tested here; the hook itself is a thin
//! shell around it.

use std::collections::BTreeSet;
use std::sync::{Mutex, MutexGuard, OnceLock};

use gliff_proto::ClientMsg;
use tokio::sync::mpsc::UnboundedSender;

const KEY_ESC: u32 = 1;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_RIGHTSHIFT: u32 = 54;

/// The evdev `KEY_*` code for a Windows scan code (set 1), and whether it
/// came with the `E0` prefix. The first 0x58 codes agree with evdev; the
/// extended ones are a short table.
pub fn evdev_from_scancode(scancode: u32, extended: bool) -> Option<u32> {
    if extended {
        return Some(match scancode {
            0x1C => 96,  // KP Enter
            0x1D => 97,  // Right Ctrl
            0x35 => 98,  // KP Divide
            0x37 => 99,  // Print Screen
            0x38 => 100, // Right Alt
            0x47 => 102, // Home
            0x48 => 103, // Up
            0x49 => 104, // Page Up
            0x4B => 105, // Left
            0x4D => 106, // Right
            0x4F => 107, // End
            0x50 => 108, // Down
            0x51 => 109, // Page Down
            0x52 => 110, // Insert
            0x53 => 111, // Delete
            0x5B => 125, // Left Win
            0x5C => 126, // Right Win
            0x5D => 127, // Menu
            _ => return None,
        });
    }
    match scancode {
        0x01..=0x53 | 0x56..=0x58 => Some(scancode),
        0x73 => Some(89),  // ABNT / Ro
        0x7D => Some(124), // Yen
        _ => None,
    }
}

/// What the hook does with one key event.
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    /// Not ours: let Windows have it.
    Pass,
    /// Ours, with nothing to send (a key held down repeating).
    Swallow,
    /// Send it to the remote and keep it from Windows.
    Forward { keycode: u32, pressed: bool },
    /// The release hotkey: the grab is over.
    Release,
}

#[derive(Default)]
pub struct Filter {
    active: bool,
    /// Keys the remote believes are down because we sent them.
    held: BTreeSet<u32>,
    /// Shift keys physically down, whether or not the grab is on or sent
    /// them: the release hotkey follows the keyboard, not the remote.
    shift: BTreeSet<u32>,
}

impl Filter {
    /// Turn the grab on or off. Turning it off returns the keys still down
    /// on the remote, which the caller releases there.
    pub fn set_active(&mut self, on: bool) -> Vec<u32> {
        self.active = on;
        if on {
            Vec::new()
        } else {
            std::mem::take(&mut self.held).into_iter().collect()
        }
    }

    pub fn on_key(
        &mut self,
        scancode: u32,
        extended: bool,
        pressed: bool,
        injected: bool,
    ) -> Action {
        // A key some program injected (an on-screen keyboard, or us) is not
        // the user's physical key.
        if injected {
            return Action::Pass;
        }
        let Some(keycode) = evdev_from_scancode(scancode, extended) else {
            return Action::Pass;
        };
        if keycode == KEY_LEFTSHIFT || keycode == KEY_RIGHTSHIFT {
            if pressed {
                self.shift.insert(keycode);
            } else {
                self.shift.remove(&keycode);
            }
        }
        if !self.active {
            return Action::Pass;
        }
        if pressed {
            if keycode == KEY_ESC && !self.shift.is_empty() {
                return Action::Release;
            }
            if !self.held.insert(keycode) {
                return Action::Swallow;
            }
        } else if !self.held.remove(&keycode) {
            // Down before the grab began: Windows still counts it as down.
            return Action::Pass;
        }
        Action::Forward { keycode, pressed }
    }
}

struct Shared {
    filter: Filter,
    tx: Option<UnboundedSender<ClientMsg>>,
    on_release: Box<dyn Fn() + Send>,
}

static SHARED: OnceLock<Mutex<Shared>> = OnceLock::new();

fn shared() -> Option<MutexGuard<'static, Shared>> {
    SHARED
        .get()
        .map(|m| m.lock().unwrap_or_else(|e| e.into_inner()))
}

impl Shared {
    fn send_key(&self, keycode: u32, pressed: bool) {
        if let Some(tx) = &self.tx {
            let _ = tx.send(ClientMsg::Key { keycode, pressed });
        }
    }
}

/// Handle one key from the hook; true keeps it from Windows. Never panics:
/// it runs inside a Windows callback.
#[cfg_attr(not(windows), allow(dead_code))]
fn handle(scancode: u32, extended: bool, pressed: bool, injected: bool) -> bool {
    let Some(mut s) = shared() else {
        return false;
    };
    let action = s.filter.on_key(scancode, extended, pressed, injected);
    match action {
        Action::Pass => false,
        Action::Swallow => true,
        Action::Forward { keycode, pressed } => {
            s.send_key(keycode, pressed);
            true
        }
        Action::Release => {
            for keycode in s.filter.set_active(false) {
                s.send_key(keycode, false);
            }
            (s.on_release)();
            true
        }
    }
}

/// The installed hook. Dropping it does not remove the hook; call
/// [`Grab::close`] so the session can end.
pub struct Grab;

impl Grab {
    /// Install the hook. Keys go to `tx`; `on_release` is called, from the
    /// hook thread, when the user presses the release hotkey.
    pub fn install(tx: UnboundedSender<ClientMsg>, on_release: impl Fn() + Send + 'static) -> Self {
        let shared = Mutex::new(Shared {
            filter: Filter::default(),
            tx: Some(tx),
            on_release: Box::new(on_release),
        });
        if SHARED.set(shared).is_ok() {
            #[cfg(windows)]
            hook::spawn();
        }
        Grab
    }

    /// Start or stop grabbing. Stopping releases on the remote every key the
    /// grab sent down.
    pub fn set_active(&self, on: bool) {
        let Some(mut s) = shared() else {
            return;
        };
        tracing::info!(on, "keyboard grab");
        for keycode in s.filter.set_active(on) {
            s.send_key(keycode, false);
        }
    }

    /// Stop grabbing and let go of the input channel, which the session
    /// needs to see closed to end.
    pub fn close(&self) {
        self.set_active(false);
        if let Some(mut s) = shared() {
            s.tx = None;
        }
    }
}

#[cfg(windows)]
mod hook {
    use windows_sys::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CallNextHookEx, GetMessageW, SetWindowsHookExW, KBDLLHOOKSTRUCT, MSG, WH_KEYBOARD_LL,
    };

    const HC_ACTION: i32 = 0;
    const LLKHF_EXTENDED: u32 = 0x01;
    const LLKHF_INJECTED: u32 = 0x10;
    const LLKHF_UP: u32 = 0x80;

    /// Install the hook on a thread of its own, which also pumps the
    /// messages a low-level hook needs. A slow window must never hold up
    /// the whole system's typing, so it is not the window thread.
    pub fn spawn() {
        let started = std::thread::Builder::new()
            .name("vexdesk-keyhook".into())
            .spawn(run);
        if let Err(e) = started {
            tracing::warn!(error = %e, "cannot start the keyboard hook thread; shortcuts stay local");
        }
    }

    fn run() {
        // SAFETY: a null module name asks for this executable's handle, and
        // the hook procedure below lives as long as the process. `msg` is a
        // plain C struct that GetMessageW fills in.
        unsafe {
            let hook = SetWindowsHookExW(
                WH_KEYBOARD_LL,
                Some(procedure),
                GetModuleHandleW(std::ptr::null()),
                0,
            );
            if hook.is_null() {
                tracing::warn!("cannot install the keyboard hook; shortcuts stay local");
                return;
            }
            tracing::info!("keyboard hook installed");
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {}
        }
    }

    unsafe extern "system" fn procedure(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
        if code == HC_ACTION {
            // SAFETY: for HC_ACTION, `lparam` points to a KBDLLHOOKSTRUCT
            // that stays valid for the duration of this call.
            let key = unsafe { &*(lparam as *const KBDLLHOOKSTRUCT) };
            let swallowed = super::handle(
                key.scanCode,
                key.flags & LLKHF_EXTENDED != 0,
                key.flags & LLKHF_UP == 0,
                key.flags & LLKHF_INJECTED != 0,
            );
            if swallowed {
                return 1;
            }
        }
        // SAFETY: passing the hook's own arguments on, as the API requires.
        unsafe { CallNextHookEx(std::ptr::null_mut(), code, wparam, lparam) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: u32 = 0x1E;
    const WIN: u32 = 0x5B;
    const TAB: u32 = 0x0F;
    const ALT: u32 = 0x38;
    const SHIFT: u32 = 0x2A;
    const ESC: u32 = 0x01;

    fn on() -> Filter {
        let mut f = Filter::default();
        f.set_active(true);
        f
    }

    #[test]
    fn scan_codes_are_evdev_codes() {
        assert_eq!(evdev_from_scancode(A, false), Some(30));
        assert_eq!(evdev_from_scancode(0x58, false), Some(88));
        assert_eq!(evdev_from_scancode(WIN, true), Some(125));
        assert_eq!(evdev_from_scancode(0x48, true), Some(103));
        assert_eq!(evdev_from_scancode(0x1C, true), Some(96));
        assert_eq!(evdev_from_scancode(0x1C, false), Some(28));
    }

    #[test]
    fn a_scan_code_with_no_key_has_no_code() {
        assert_eq!(evdev_from_scancode(0x54, false), None);
        assert_eq!(evdev_from_scancode(0x7F, false), None);
        assert_eq!(evdev_from_scancode(0x70, true), None);
    }

    #[test]
    fn an_inactive_grab_leaves_every_key_to_windows() {
        let mut f = Filter::default();
        assert_eq!(f.on_key(WIN, true, true, false), Action::Pass);
    }

    #[test]
    fn the_win_key_and_alt_tab_go_to_the_remote() {
        let mut f = on();
        assert_eq!(
            f.on_key(WIN, true, true, false),
            Action::Forward {
                keycode: 125,
                pressed: true
            }
        );
        f.on_key(ALT, false, true, false);
        assert_eq!(
            f.on_key(TAB, false, true, false),
            Action::Forward {
                keycode: 15,
                pressed: true
            }
        );
        assert_eq!(
            f.on_key(WIN, true, false, false),
            Action::Forward {
                keycode: 125,
                pressed: false
            }
        );
    }

    #[test]
    fn a_held_key_is_sent_once() {
        let mut f = on();
        assert!(matches!(
            f.on_key(A, false, true, false),
            Action::Forward { .. }
        ));
        assert_eq!(f.on_key(A, false, true, false), Action::Swallow);
        assert_eq!(f.on_key(A, false, true, false), Action::Swallow);
    }

    #[test]
    fn a_key_released_that_was_not_sent_down_stays_with_windows() {
        let mut f = on();
        assert_eq!(f.on_key(A, false, false, false), Action::Pass);
    }

    #[test]
    fn an_injected_key_is_not_the_users() {
        let mut f = on();
        assert_eq!(f.on_key(A, false, true, true), Action::Pass);
    }

    #[test]
    fn shift_escape_releases_the_grab() {
        let mut f = on();
        f.on_key(SHIFT, false, true, false);
        assert_eq!(f.on_key(ESC, false, true, false), Action::Release);
        // The shift the remote still holds is let go with the grab.
        assert_eq!(f.set_active(false), vec![KEY_LEFTSHIFT]);
    }

    #[test]
    fn a_shift_held_before_the_grab_still_releases_it() {
        let mut f = Filter::default();
        f.on_key(SHIFT, false, true, false);
        f.set_active(true);
        assert_eq!(f.on_key(ESC, false, true, false), Action::Release);
    }

    #[test]
    fn a_shift_held_across_release_and_recapture_still_releases_it() {
        let mut f = on();
        f.on_key(SHIFT, false, true, false);
        assert_eq!(f.on_key(ESC, false, true, false), Action::Release);
        f.set_active(false);
        f.set_active(true);
        assert_eq!(f.on_key(ESC, false, true, false), Action::Release);
    }

    #[test]
    fn a_shift_let_go_while_the_grab_is_off_no_longer_counts() {
        let mut f = Filter::default();
        f.on_key(SHIFT, false, true, false);
        f.on_key(SHIFT, false, false, false);
        f.set_active(true);
        assert!(matches!(
            f.on_key(ESC, false, true, false),
            Action::Forward { .. }
        ));
    }

    #[test]
    fn escape_alone_goes_to_the_remote() {
        let mut f = on();
        assert_eq!(
            f.on_key(ESC, false, true, false),
            Action::Forward {
                keycode: 1,
                pressed: true
            }
        );
    }

    #[test]
    fn turning_the_grab_off_returns_the_held_keys_once() {
        let mut f = on();
        f.on_key(WIN, true, true, false);
        f.on_key(A, false, true, false);
        assert_eq!(f.set_active(false), vec![30, 125]);
        assert!(f.set_active(false).is_empty());
        // And a late release of a key the remote no longer holds is Windows'.
        assert_eq!(f.on_key(A, false, false, false), Action::Pass);
    }
}
