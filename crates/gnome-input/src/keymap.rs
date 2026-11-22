//! The viewer's keyboard layout, kept on the server so its keys can be sent as
//! the characters they make there.
//!
//! Mutter reads a key code with the *server's* layout and its RemoteDesktop
//! session has no way to install another keymap. When the viewer's layout is
//! the server's, key codes are exact (every symbol, level, dead key and media
//! key as the server's own keyboard has them) and that is what goes. When the
//! two differ (a German viewer on a US desktop would type `y` where it means
//! `z`) the viewer's keymap is compiled here, every key goes through its state
//! (Shift, AltGr, Caps Lock and the rest tracked as the viewer's keyboard
//! would), and the keysym that comes out is what is sent; Mutter finds the key
//! for it on its side and presses whatever Shift it needs (levels 1 and 2
//! only). The server's layout is read from the compositor
//! ([`crate::ServerKeymap`]); until it is known, key codes go.
//!
//! In keysym mode the modifiers that only choose a character stay here: the
//! lock keys and AltGr, and Shift, which would be applied twice (a French
//! Shift+1 is `1`, and a server Shift held down would turn it into `!`). Shift
//! reaches the server only when something other than a character needs it: a
//! key that is not a character (Shift+Left selects), a pointer button or a
//! scroll (Shift+click), and it is let go before the next character. Control,
//! Alt and Super are sent as the keys they are. Only one character key is held
//! on the server at a time, so the Shift Mutter adds for an upper-case letter
//! goes with that letter before the next key is pressed.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Result};
use xkbcommon::xkb;

use crate::translate::Call;

const SHIFT_L: u32 = 0xffe1;
const SHIFT_R: u32 = 0xffe2;
/// Keysyms of the keys that toggle a lock: Caps_Lock, Shift_Lock, Num_Lock,
/// Scroll_Lock.
const LOCKS: [u32; 4] = [0xffe5, 0xffe6, 0xff7f, 0xff14];
/// Keys that choose a level and mean nothing to the server by themselves:
/// ISO_Level3_Shift (AltGr), ISO_Level5_Shift, Mode_switch.
const LEVEL_SHIFTS: [u32; 3] = [0xfe03, 0xfe11, 0xff7e];
/// Control, Alt, Meta, Super and Hyper, left and right.
const MODIFIERS: [u32; 10] = [
    0xffe3, 0xffe4, 0xffe9, 0xffea, 0xffe7, 0xffe8, 0xffeb, 0xffec, 0xffed, 0xffee,
];
/// The keypad's keysyms, KP_Space to KP_9.
const KEYPAD: std::ops::RangeInclusive<u32> = 0xff80..=0xffbf;
/// The dead keysyms, dead_grave to dead_longsolidusoverlay. They type no
/// character of their own but make one with the next key, and Mutter treats
/// them like any key that needs its Shift pressed, so they are character keys
/// here (let go before the next one).
const DEAD: std::ops::RangeInclusive<u32> = 0xfe50..=0xfe93;
/// What KP_Home to KP_Delete (0xff95 to 0xff9f) are without the keypad: Home,
/// Left, Up, Right, Down, Prior, Next, End, Begin, Insert, Delete.
const KEYPAD_NAV: [u32; 11] = [
    0xff50, 0xff51, 0xff52, 0xff53, 0xff54, 0xff55, 0xff56, 0xff57, 0xff58, 0xff63, 0xffff,
];

/// How keys are sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// As key codes: the server's layout is the viewer's, or not known yet.
    Codes,
    /// As the keysyms the viewer's layout makes.
    Keysyms,
}

/// What a held key was sent as, so its release matches whatever the layout
/// says by then.
#[derive(Clone, Copy)]
enum Sent {
    /// A character key: its keysym.
    Text(u32),
    /// A character key that was let go on the server when the next one went
    /// down: nothing to send on release.
    Done,
    /// Control, Alt, Super and the like, and keys that are not characters.
    Keysym(u32),
    /// The key's code goes as it is.
    Code,
    /// A lock, AltGr: only the viewer's state sees it.
    Local,
    /// Shift: only the viewer's state sees it, until something needs it.
    Shift,
}

pub struct ViewerKeys {
    text: String,
    keymap: xkb::Keymap,
    state: xkb::State,
    /// Num Lock starts on, as the server's keyboard has it, so the keypad
    /// types digits until the viewer says otherwise.
    num_lock: bool,
    mode: Mode,
    held: BTreeMap<u32, Sent>,
    /// The Shift keys held on the viewer.
    shifts: BTreeSet<u32>,
    /// A Shift has been pressed on the server for something that needs it.
    server_shift: bool,
    /// The character key down on the server, with its keysym.
    text_down: Option<(u32, u32)>,
}

/// Whether a keysym types a character (not a control code like Return).
fn types_a_character(sym: u32) -> bool {
    let c = xkb::keysym_to_utf32(xkb::Keysym::new(sym));
    c >= 0x20 && c != 0x7f
}

/// The keypad's keysyms as the plain ones. Mutter makes KP_1 a shifted key
/// (so the server's Num Lock turns it into End), and a keypad navigation key
/// (KP_Left, with Num Lock off on the viewer) is only Left when the server's
/// Num Lock is off too; `1` and Left are just themselves.
fn plain(sym: u32) -> u32 {
    match sym {
        0xff80 => 0x20,                           // KP_Space
        0xff89 => 0xff09,                         // KP_Tab
        0xff8d => 0xff0d,                         // KP_Enter
        0xfe20 => 0xff09, // ISO_Left_Tab: Mutter adds a Shift of its own for it
        0xff91..=0xff94 => sym - 0xff91 + 0xffbe, // KP_F1..F4
        0xff95..=0xff9f => KEYPAD_NAV[(sym - 0xff95) as usize],
        s if KEYPAD.contains(&s) => match xkb::keysym_to_utf32(xkb::Keysym::new(s)) {
            0 => s,
            c => xkb::utf32_to_keysym(c).raw(),
        },
        s => s,
    }
}

/// Whether `a`'s first layout gives every key the same symbols, on every
/// level, as layout `group` of `b`.
fn same_layout(a: &xkb::Keymap, b: &xkb::Keymap, group: u32) -> bool {
    let lo = a.min_keycode().raw().min(b.min_keycode().raw());
    let hi = a.max_keycode().raw().max(b.max_keycode().raw());
    (lo..=hi).all(|raw| {
        let key = xkb::Keycode::new(raw);
        let levels = a.num_levels_for_key(key, 0);
        levels == b.num_levels_for_key(key, group)
            && (0..levels).all(|l| {
                a.key_get_syms_by_level(key, 0, l) == b.key_get_syms_by_level(key, group, l)
            })
    })
}

#[cfg(test)]
pub(crate) fn same_layout_for_tests(a: &xkb::Keymap, b: &xkb::Keymap) -> bool {
    same_layout(a, b, 0)
}

fn compile(text: &str) -> Option<xkb::Keymap> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_string(
        &ctx,
        text.to_owned(),
        xkb::KEYMAP_FORMAT_TEXT_V1,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    )
}

impl ViewerKeys {
    /// `server` is the server's keymap text when known.
    pub fn new(text: &str, num_lock: bool, server: Option<&str>) -> Result<Self> {
        let keymap =
            compile(text).ok_or_else(|| anyhow!("cannot compile the viewer's keymap text"))?;
        let state = Self::fresh_state(&keymap, num_lock);
        let mode = Self::mode_for(&keymap, server);
        Ok(Self {
            text: text.to_owned(),
            keymap,
            state,
            num_lock,
            mode,
            held: BTreeMap::new(),
            shifts: BTreeSet::new(),
            server_shift: false,
            text_down: None,
        })
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    fn mode_for(viewer: &xkb::Keymap, server: Option<&str>) -> Mode {
        match server.and_then(compile) {
            // Which of the server's layouts is active is not known here. The
            // viewer's layout can only be trusted as the same when it is one
            // of them (key codes, as before); when it is none of them, key
            // codes are certainly wrong and symbols are right in any layout.
            Some(server) if (0..server.num_layouts()).any(|g| same_layout(viewer, &server, g)) => {
                Mode::Codes
            }
            Some(_) => Mode::Keysyms,
            None => Mode::Codes,
        }
    }

    /// The server's keymap is now `server`: keys held under the old mode are
    /// let go if the mode changes.
    pub fn set_server(&mut self, server: Option<&str>) -> Vec<Call> {
        let mode = Self::mode_for(&self.keymap, server);
        if mode == self.mode {
            return Vec::new();
        }
        let calls = self.release_all();
        self.mode = mode;
        calls
    }

    /// A state with nothing held and, if asked, Num Lock locked.
    fn fresh_state(keymap: &xkb::Keymap, num_lock: bool) -> xkb::State {
        let mut state = xkb::State::new(keymap);
        let num = keymap.mod_get_index(xkb::MOD_NAME_NUM);
        if num_lock && num != xkb::MOD_INVALID {
            state.update_mask(0, 0, 1 << num, 0, 0, 0);
        }
        state
    }

    fn shift_call(&mut self, pressed: bool) -> Option<Call> {
        if pressed == self.server_shift {
            return None;
        }
        self.server_shift = pressed;
        Some(Call::Keysym {
            sym: SHIFT_L,
            pressed,
        })
    }

    /// The calls for one key event. `pointer_down` says a pointer button is
    /// held, which makes a Shift pressed now matter at once.
    pub fn key(&mut self, code: u32, pressed: bool, pointer_down: bool) -> Vec<Call> {
        let keycode = xkb::Keycode::new(code.saturating_add(8));
        let mut calls = Vec::new();
        if pressed {
            let first = !self.held.contains_key(&code);
            // A key that repeats keeps the symbol it started with.
            let sent = match self.held.get(&code) {
                Some(s) => *s,
                None => self.classify(self.state.key_get_one_sym(keycode).raw()),
            };
            self.held.insert(code, sent);
            if first {
                self.state.update_key(keycode, xkb::KeyDirection::Down);
            }
            match sent {
                Sent::Shift => {
                    self.shifts.insert(code);
                    if pointer_down {
                        calls.extend(self.shift_call(true));
                    }
                }
                Sent::Text(sym) => {
                    // One character key at a time, and no Shift of ours
                    // under it.
                    if let Some((other, other_sym)) = self.text_down.take() {
                        if other != code {
                            calls.push(Call::Keysym {
                                sym: other_sym,
                                pressed: false,
                            });
                            self.held.insert(other, Sent::Done);
                        }
                    }
                    calls.extend(self.shift_call(false));
                    calls.push(Call::Keysym { sym, pressed });
                    self.text_down = Some((code, sym));
                }
                Sent::Keysym(sym) => {
                    // The Shift Mutter added for a character must not reach
                    // this key, and a key that is not a character is where
                    // a Shift held on the viewer matters.
                    if first {
                        calls.extend(self.release_text());
                    }
                    if !MODIFIERS.contains(&sym) && !self.shifts.is_empty() {
                        calls.extend(self.shift_call(true));
                    }
                    calls.push(Call::Keysym { sym, pressed });
                }
                Sent::Code => {
                    if first {
                        calls.extend(self.release_text());
                    }
                    calls.push(Call::Key { code, pressed });
                }
                Sent::Local | Sent::Done => {}
            }
        } else if let Some(sent) = self.held.remove(&code) {
            self.state.update_key(keycode, xkb::KeyDirection::Up);
            match sent {
                Sent::Text(sym) => {
                    if self.text_down.is_some_and(|(c, _)| c == code) {
                        self.text_down = None;
                    }
                    calls.push(Call::Keysym { sym, pressed });
                }
                Sent::Keysym(sym) => calls.push(Call::Keysym { sym, pressed }),
                Sent::Code => calls.push(Call::Key { code, pressed }),
                Sent::Shift => {
                    self.shifts.remove(&code);
                    if self.shifts.is_empty() {
                        calls.extend(self.shift_call(false));
                    }
                }
                Sent::Local | Sent::Done => {}
            }
        }
        calls
    }

    fn classify(&self, sym: u32) -> Sent {
        if self.mode == Mode::Codes || sym == 0 {
            return Sent::Code;
        }
        match plain(sym) {
            SHIFT_L | SHIFT_R => Sent::Shift,
            s if LOCKS.contains(&s) || LEVEL_SHIFTS.contains(&s) => Sent::Local,
            s if types_a_character(s) || DEAD.contains(&s) => Sent::Text(s),
            s => Sent::Keysym(s),
        }
    }

    /// The calls to make before a pointer button goes down or the wheel
    /// turns: a held Shift reaches the server for Shift+click and
    /// Shift+scroll.
    pub fn before_pointer(&mut self) -> Vec<Call> {
        if self.mode != Mode::Keysyms {
            return Vec::new();
        }
        let mut calls = self.release_text();
        if !self.shifts.is_empty() {
            calls.extend(self.shift_call(true));
        }
        calls
    }

    /// Let go of the character key that is down on the server, if any, so the
    /// Shift Mutter pressed for it goes too. The viewer's key stays held.
    fn release_text(&mut self) -> Vec<Call> {
        match self.text_down.take() {
            Some((code, sym)) => {
                self.held.insert(code, Sent::Done);
                vec![Call::Keysym {
                    sym,
                    pressed: false,
                }]
            }
            None => Vec::new(),
        }
    }

    /// Let go of everything held, and forget the viewer's modifier state.
    pub fn release_all(&mut self) -> Vec<Call> {
        let mut calls: Vec<Call> = std::mem::take(&mut self.held)
            .into_iter()
            .filter_map(|(code, sent)| match sent {
                Sent::Text(sym) | Sent::Keysym(sym) => Some(Call::Keysym {
                    sym,
                    pressed: false,
                }),
                Sent::Code => Some(Call::Key {
                    code,
                    pressed: false,
                }),
                Sent::Local | Sent::Shift | Sent::Done => None,
            })
            .collect();
        self.shifts.clear();
        self.text_down = None;
        calls.extend(self.shift_call(false));
        self.state = Self::fresh_state(&self.keymap, self.num_lock);
        calls
    }
}

// SAFETY: the xkb context, keymap and state are made in `new` and never leave
// this struct, whose methods take `&mut self`; `Input` keeps it behind a
// `Mutex`, so only one thread touches them at a time. libxkbcommon objects
// have no thread affinity; they are only not safe to use from two threads at
// once, which the lock prevents. (The Rust types are `!Send` only because
// they hold raw pointers.)
unsafe impl Send for ViewerKeys {}

impl std::fmt::Debug for ViewerKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ViewerKeys")
            .field("mode", &self.mode)
            .field("held", &self.held.len())
            .finish_non_exhaustive()
    }
}
