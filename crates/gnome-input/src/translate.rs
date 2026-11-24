//! gliff input commands to Mutter calls, without D-Bus.

use std::collections::BTreeSet;

use anyhow::{bail, Result};
use gnome_ipc::input::{AXIS_FINISH, AXIS_SOURCE_FINGER};
use hypr_input::{Axis, InputCmd};

use crate::keymap::ViewerKeys;

/// One RemoteDesktop input call.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Call {
    Key {
        code: u32,
        pressed: bool,
    },
    /// A key by keysym, from the viewer's layout.
    Keysym {
        sym: u32,
        pressed: bool,
    },
    Button {
        button: u32,
        pressed: bool,
    },
    Motion {
        x: f64,
        y: f64,
    },
    Axis {
        dx: f64,
        dy: f64,
        flags: u32,
    },
    AxisDiscrete {
        axis: u32,
        steps: i32,
    },
}

/// Tracks held keys and buttons and the pointer's extent.
#[derive(Debug)]
pub struct Translator {
    keys: BTreeSet<u32>,
    /// The viewer's layout, once it has sent one.
    viewer: Option<ViewerKeys>,
    /// Whether the server's Num Lock is on, for a layout that arrives next.
    num_lock: bool,
    /// The server's own keymap, once known, and which version of it this is.
    server: Option<String>,
    server_generation: u64,
    buttons: BTreeSet<u32>,
    extent: (u32, u32),
    stopped: bool,
}

impl Translator {
    pub fn new(extent: (u32, u32)) -> Self {
        Self {
            keys: BTreeSet::new(),
            viewer: None,
            num_lock: true,
            server: None,
            server_generation: 0,
            buttons: BTreeSet::new(),
            extent: (extent.0.max(1), extent.1.max(1)),
            stopped: false,
        }
    }

    /// Tell the translator whether the server's Num Lock is on; a viewer
    /// layout that arrives later starts with the keypad in that state.
    pub fn set_num_lock(&mut self, on: bool) {
        self.num_lock = on;
    }

    /// The server's keymap is `text` (version `generation`; `None` when not
    /// known). Returns the calls that let go of keys held under the old
    /// way of sending them, if it changes.
    pub fn set_server_keymap(&mut self, generation: u64, text: Option<String>) -> Vec<Call> {
        if generation == self.server_generation {
            return Vec::new();
        }
        self.server_generation = generation;
        self.server = text;
        match &mut self.viewer {
            Some(v) => v.set_server(self.server.as_deref()),
            None => Vec::new(),
        }
    }

    /// How keys are being sent, when a viewer layout has arrived.
    pub fn key_mode(&self) -> Option<crate::keymap::Mode> {
        self.viewer.as_ref().map(ViewerKeys::mode)
    }

    pub fn translate(&mut self, cmd: InputCmd) -> Result<Vec<Call>> {
        if self.stopped {
            bail!("input has been stopped");
        }
        Ok(match cmd {
            InputCmd::Key { code, pressed } if self.viewer.is_some() => {
                let pointer_down = !self.buttons.is_empty();
                self.viewer
                    .as_mut()
                    .map(|v| v.key(code, pressed, pointer_down))
                    .unwrap_or_default()
            }
            InputCmd::Key { code, pressed } => {
                if pressed {
                    self.keys.insert(code);
                } else {
                    self.keys.remove(&code);
                }
                vec![Call::Key { code, pressed }]
            }
            InputCmd::Button { button, pressed } => {
                let mut calls = Vec::new();
                if pressed {
                    self.buttons.insert(button);
                    // Shift+click needs the Shift the viewer holds.
                    if let Some(v) = &mut self.viewer {
                        calls.extend(v.before_pointer());
                    }
                } else {
                    self.buttons.remove(&button);
                }
                calls.push(Call::Button { button, pressed });
                calls
            }
            InputCmd::Motion { x, y } => {
                let (w, h) = self.extent;
                vec![Call::Motion {
                    x: x.clamp(0.0, f64::from(w - 1)),
                    y: y.clamp(0.0, f64::from(h - 1)),
                }]
            }
            InputCmd::Axis {
                axis,
                value,
                discrete,
                stop,
            } => {
                let index = match axis {
                    Axis::Vertical => 0,
                    Axis::Horizontal => 1,
                };
                if stop {
                    vec![Call::Axis {
                        dx: 0.0,
                        dy: 0.0,
                        flags: AXIS_FINISH | AXIS_SOURCE_FINGER,
                    }]
                } else {
                    // Shift+scroll needs the Shift the viewer holds.
                    let mut calls = match &mut self.viewer {
                        Some(v) => v.before_pointer(),
                        None => Vec::new(),
                    };
                    if let Some(steps) = discrete {
                        calls.push(Call::AxisDiscrete { axis: index, steps });
                    } else {
                        let (dx, dy) = match axis {
                            Axis::Vertical => (0.0, value),
                            Axis::Horizontal => (value, 0.0),
                        };
                        calls.push(Call::Axis {
                            dx,
                            dy,
                            flags: AXIS_SOURCE_FINGER,
                        });
                    }
                    calls
                }
            }
            InputCmd::SetExtent { width, height } => {
                self.extent = (width.max(1), height.max(1));
                Vec::new()
            }
            InputCmd::ReleaseAll => self.release_all(),
            // Like the Hyprland input: let go of everything, then take no more.
            InputCmd::Stop => {
                self.stopped = true;
                self.release_all()
            }
            // Keys held under the old layout are let go first, then the new
            // layout takes over from a clean state.
            InputCmd::SetKeymap(text) => {
                if self.viewer.as_ref().is_some_and(|v| v.text() == text) {
                    return Ok(Vec::new());
                }
                let next = ViewerKeys::new(&text, self.num_lock, self.server.as_deref())?;
                let calls = self.release_keys();
                self.viewer = Some(next);
                calls
            }
        })
    }
}

impl Translator {
    /// Let go of every held key, whether it went as a code or as a keysym.
    fn release_keys(&mut self) -> Vec<Call> {
        let mut calls: Vec<Call> = std::mem::take(&mut self.keys)
            .into_iter()
            .map(|code| Call::Key {
                code,
                pressed: false,
            })
            .collect();
        if let Some(viewer) = &mut self.viewer {
            calls.extend(viewer.release_all());
        }
        calls
    }

    fn release_all(&mut self) -> Vec<Call> {
        let mut calls = self.release_keys();
        calls.extend(
            std::mem::take(&mut self.buttons)
                .into_iter()
                .map(|button| Call::Button {
                    button,
                    pressed: false,
                }),
        );
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t() -> Translator {
        Translator::new((1920, 1080))
    }

    #[test]
    fn keys_and_buttons_pass_through() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Key {
                code: 30,
                pressed: true
            })
            .unwrap(),
            [Call::Key {
                code: 30,
                pressed: true
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Button {
                button: 0x110,
                pressed: true
            })
            .unwrap(),
            [Call::Button {
                button: 0x110,
                pressed: true
            }]
        );
    }

    #[test]
    fn release_all_lets_go_of_what_is_held_once() {
        let mut t = t();
        t.translate(InputCmd::Key {
            code: 42,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Key {
            code: 30,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Key {
            code: 30,
            pressed: false,
        })
        .unwrap();
        t.translate(InputCmd::Button {
            button: 0x110,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::ReleaseAll).unwrap(),
            [
                Call::Key {
                    code: 42,
                    pressed: false
                },
                Call::Button {
                    button: 0x110,
                    pressed: false
                },
            ]
        );
        assert!(t.translate(InputCmd::ReleaseAll).unwrap().is_empty());
    }

    #[test]
    fn stop_releases_what_is_held_and_refuses_more_input() {
        let mut t = t();
        t.translate(InputCmd::Key {
            code: 42,
            pressed: true,
        })
        .unwrap();
        t.translate(InputCmd::Button {
            button: 0x110,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::Stop).unwrap(),
            [
                Call::Key {
                    code: 42,
                    pressed: false
                },
                Call::Button {
                    button: 0x110,
                    pressed: false
                },
            ]
        );
        assert!(t
            .translate(InputCmd::Key {
                code: 30,
                pressed: true
            })
            .is_err());
        assert!(t.translate(InputCmd::Motion { x: 1.0, y: 1.0 }).is_err());
    }

    #[test]
    fn motion_is_clamped_to_the_extent_and_follows_resizes() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Motion { x: -5.0, y: 5000.0 })
                .unwrap(),
            [Call::Motion { x: 0.0, y: 1079.0 }]
        );
        t.translate(InputCmd::SetExtent {
            width: 800,
            height: 600,
        })
        .unwrap();
        assert_eq!(
            t.translate(InputCmd::Motion { x: 900.5, y: 10.25 })
                .unwrap(),
            [Call::Motion { x: 799.0, y: 10.25 }]
        );
    }

    #[test]
    fn wheel_steps_are_discrete_on_the_right_axis() {
        let mut t = t();
        let wheel = |axis, steps| InputCmd::Axis {
            axis,
            value: 15.0,
            discrete: Some(steps),
            stop: false,
        };
        assert_eq!(
            t.translate(wheel(Axis::Vertical, 2)).unwrap(),
            [Call::AxisDiscrete { axis: 0, steps: 2 }]
        );
        assert_eq!(
            t.translate(wheel(Axis::Horizontal, -1)).unwrap(),
            [Call::AxisDiscrete { axis: 1, steps: -1 }]
        );
    }

    #[test]
    fn smooth_scrolling_uses_the_value_and_stop_finishes_it() {
        let mut t = t();
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Vertical,
                value: 4.5,
                discrete: None,
                stop: false
            })
            .unwrap(),
            [Call::Axis {
                dx: 0.0,
                dy: 4.5,
                flags: AXIS_SOURCE_FINGER
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Horizontal,
                value: -2.0,
                discrete: None,
                stop: false
            })
            .unwrap(),
            [Call::Axis {
                dx: -2.0,
                dy: 0.0,
                flags: AXIS_SOURCE_FINGER
            }]
        );
        assert_eq!(
            t.translate(InputCmd::Axis {
                axis: Axis::Vertical,
                value: 0.0,
                discrete: None,
                stop: true
            })
            .unwrap(),
            [Call::Axis {
                dx: 0.0,
                dy: 0.0,
                flags: AXIS_FINISH | AXIS_SOURCE_FINGER
            }]
        );
    }

    /// The text of a layout, as the viewer would send it.
    fn layout(name: &str) -> String {
        use xkbcommon::xkb;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        xkb::Keymap::new_from_names(&ctx, "", "", name, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .expect("compile")
            .get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)
    }

    /// A translator for a viewer on `viewer` and a server on `server`.
    fn with_server(viewer: &str, server: &str) -> Translator {
        let mut t = t();
        t.set_server_keymap(1, Some(layout(server)));
        assert!(t
            .translate(InputCmd::SetKeymap(layout(viewer)))
            .unwrap()
            .is_empty());
        t
    }

    /// A viewer on `viewer` and a server on another layout, so keys go as
    /// keysyms.
    fn with(viewer: &str) -> Translator {
        with_server(viewer, if viewer == "us" { "de" } else { "us" })
    }

    fn key(t: &mut Translator, code: u32, pressed: bool) -> Vec<Call> {
        t.translate(InputCmd::Key { code, pressed }).unwrap()
    }

    fn sym(sym: u32, pressed: bool) -> Call {
        Call::Keysym { sym, pressed }
    }

    const KEY_1: u32 = 2;
    const KEY_T: u32 = 20;
    const KEY_LEFTCTRL: u32 = 29;
    const KEY_LEFT: u32 = 105;
    const SHIFT_L: u32 = 0xffe1;
    const KEYSYM_LEFT: u32 = 0xff51;
    const KEYSYM_CONTROL_L: u32 = 0xffe3;
    const KEY_Y: u32 = 21;
    const KEY_Z: u32 = 44;
    const KEY_Q: u32 = 16;
    const KEY_A: u32 = 30;
    const KEY_LEFTSHIFT: u32 = 42;
    const KEY_RIGHTALT: u32 = 100;
    const KEY_CAPSLOCK: u32 = 58;

    #[test]
    fn the_translator_can_move_between_threads() {
        // `Input` is held by the server's session, which needs `Send`.
        fn send<T: Send>() {}
        send::<Translator>();
    }

    #[test]
    fn the_same_layout_on_both_ends_keeps_every_key_a_code() {
        let mut de = with_server("de", "de");
        assert_eq!(de.key_mode(), Some(crate::keymap::Mode::Codes));
        assert_eq!(
            key(&mut de, KEY_Y, true),
            [Call::Key {
                code: KEY_Y,
                pressed: true
            }]
        );
        // Shift, Caps Lock and the keypad are the server's own too.
        for code in [KEY_LEFTSHIFT, KEY_CAPSLOCK, 79] {
            assert_eq!(
                key(&mut de, code, true),
                [Call::Key {
                    code,
                    pressed: true
                }]
            );
        }
        assert_eq!(
            key(&mut de, KEY_Y, false),
            [Call::Key {
                code: KEY_Y,
                pressed: false
            }]
        );
    }

    #[test]
    fn a_different_layout_sends_keysyms_and_an_unknown_server_sends_codes() {
        assert_eq!(
            with_server("de", "us").key_mode(),
            Some(crate::keymap::Mode::Keysyms)
        );
        let mut t = t();
        t.translate(InputCmd::SetKeymap(layout("de"))).unwrap();
        assert_eq!(t.key_mode(), Some(crate::keymap::Mode::Codes));
        assert_eq!(
            key(&mut t, KEY_Y, true),
            [Call::Key {
                code: KEY_Y,
                pressed: true
            }]
        );
        // The server's layout arrives: keys held as codes are let go, and
        // the key's own release then sends nothing.
        let calls = t.set_server_keymap(1, Some(layout("us")));
        assert_eq!(
            calls,
            [Call::Key {
                code: KEY_Y,
                pressed: false
            }]
        );
        assert_eq!(t.key_mode(), Some(crate::keymap::Mode::Keysyms));
        assert!(key(&mut t, KEY_Y, false).is_empty());
        assert_eq!(key(&mut t, KEY_Y, true), [sym(0x7a, true)]);
        // The same version again is no change.
        assert!(t.set_server_keymap(1, Some(layout("de"))).is_empty());
        assert_eq!(t.key_mode(), Some(crate::keymap::Mode::Keysyms));
    }

    #[test]
    fn a_shift_that_went_to_the_server_does_not_shift_the_next_character() {
        // French: hold Shift, tap Left (Shift reaches the server), then tap
        // 1 with Shift still held: it is "1", so the server's Shift must be up.
        let mut fr = with("fr");
        key(&mut fr, KEY_LEFTSHIFT, true);
        assert_eq!(
            key(&mut fr, KEY_LEFT, true),
            [sym(SHIFT_L, true), sym(KEYSYM_LEFT, true)]
        );
        key(&mut fr, KEY_LEFT, false);
        assert_eq!(
            key(&mut fr, KEY_1, true),
            [sym(SHIFT_L, false), sym(0x31, true)]
        );
        key(&mut fr, KEY_1, false);
        // And Left needs it again.
        assert_eq!(
            key(&mut fr, KEY_LEFT, true),
            [sym(SHIFT_L, true), sym(KEYSYM_LEFT, true)]
        );
    }

    #[test]
    fn only_one_character_key_is_down_on_the_server_at_a_time() {
        // Shift+Y, Shift up, then A while Y is still down: the server lets
        // go of Z (and the Shift Mutter made for it) before it presses a.
        let mut de = with("de");
        key(&mut de, KEY_LEFTSHIFT, true);
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x5a, true)]);
        key(&mut de, KEY_LEFTSHIFT, false);
        assert_eq!(
            key(&mut de, KEY_A, true),
            [sym(0x5a, false), sym(0x61, true)]
        );
        assert!(key(&mut de, KEY_Y, false).is_empty(), "already let go");
        assert_eq!(key(&mut de, KEY_A, false), [sym(0x61, false)]);
        // A repeating key is not "another" key.
        assert_eq!(key(&mut de, KEY_A, true), [sym(0x61, true)]);
        assert_eq!(key(&mut de, KEY_A, true), [sym(0x61, true)]);
    }

    #[test]
    fn a_held_shift_reaches_the_server_for_the_wheel_and_for_a_press_during_a_drag() {
        let mut us = with("us");
        key(&mut us, KEY_LEFTSHIFT, true);
        let calls = us
            .translate(InputCmd::Axis {
                axis: Axis::Horizontal,
                value: 10.0,
                discrete: Some(1),
                stop: false,
            })
            .unwrap();
        assert_eq!(
            calls,
            [sym(SHIFT_L, true), Call::AxisDiscrete { axis: 1, steps: 1 }]
        );
        key(&mut us, KEY_LEFTSHIFT, false);
        // Button down first, Shift pressed during the drag: it goes at once.
        us.translate(InputCmd::Button {
            button: 0x110,
            pressed: true,
        })
        .unwrap();
        assert_eq!(
            key(&mut us, KEY_LEFTSHIFT, true),
            [sym(SHIFT_L, true)],
            "Shift pressed with a button held"
        );
        assert_eq!(key(&mut us, KEY_LEFTSHIFT, false), [sym(SHIFT_L, false)]);
    }

    #[test]
    fn a_key_pressed_twice_is_released_once() {
        let mut us = with("us");
        key(&mut us, KEY_LEFTSHIFT, true);
        key(&mut us, KEY_LEFTSHIFT, true);
        key(&mut us, KEY_LEFTSHIFT, false);
        // The viewer's Shift is up; the letter after it is lower case.
        assert_eq!(key(&mut us, KEY_A, true), [sym(0x61, true)]);
    }

    #[test]
    fn layouts_are_compared_key_by_key_on_every_level() {
        let keymap = |name: &str| {
            use xkbcommon::xkb;
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            xkb::Keymap::new_from_string(
                &ctx,
                layout(name),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("compile")
        };
        assert!(crate::keymap::same_layout_for_tests(
            &keymap("us"),
            &keymap("us")
        ));
        assert!(!crate::keymap::same_layout_for_tests(
            &keymap("us"),
            &keymap("de")
        ));
        assert!(!crate::keymap::same_layout_for_tests(
            &keymap("de"),
            &keymap("fr")
        ));
    }

    #[test]
    fn layouts_that_differ_only_in_function_and_vendor_keys_are_the_same() {
        // Seen on hardware: the same `us` layout read by two machines with
        // different xkeyboard-config versions differed in F23 and five vendor
        // keys, and the viewer's layout was taken for another one.
        use xkbcommon::xkb;
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let compile = |text: String| {
            xkb::Keymap::new_from_string(
                &ctx,
                text,
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("compile")
        };
        // A layout with one more definition at the end of the symbols (a
        // later definition of a key wins).
        let with_key = |name: &str, definition: &str| {
            let text = layout(name);
            let at = text.find("xkb_symbols").expect("symbols");
            let close = at + text[at..].find("\n};").expect("close") + 1;
            compile(format!(
                "{}\t{definition}\n{}",
                &text[..close],
                &text[close..]
            ))
        };
        let with_options = |name: &str, options: &str| {
            xkb::Keymap::new_from_names(
                &ctx,
                "",
                "",
                name,
                "",
                Some(options.to_owned()),
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
            .expect("compile")
        };
        let us = compile(layout("us"));
        let fr = compile(layout("fr"));
        let same = crate::keymap::same_layout_for_tests;
        assert!(same(
            &us,
            &with_key("us", "key <FK23> { [ XF86Tools, XF86Assistant ] };")
        ));
        // Where one database has a vendor key and the other none.
        assert!(same(&us, &with_key("us", "key <I602> { [ XF86Tools ] };")));
        // Keys that type, or change what is typed, still count: a letter, a
        // dead key (it makes a different character), Compose, a modifier.
        assert!(!same(&us, &with_key("us", "key <AC01> { [ b, B ] };")));
        assert!(!same(
            &fr,
            &with_key("fr", "key <AD11> { [ dead_acute, dead_diaeresis ] };")
        ));
        // An F key of the main block remapped is a difference.
        assert!(!same(&us, &with_key("us", "key <FK01> { [ F2 ] };")));
        assert!(!same(&us, &with_options("us", "compose:menu")));
        assert!(!same(&us, &with_options("us", "ctrl:swapcaps")));
    }

    #[test]
    fn without_a_viewer_layout_keys_go_as_codes() {
        assert_eq!(
            key(&mut t(), KEY_A, true),
            [Call::Key {
                code: KEY_A,
                pressed: true
            }]
        );
    }

    #[test]
    fn a_viewer_layout_turns_key_codes_into_the_characters_it_makes() {
        // The key that is Y on a US keyboard is Z on a German one, and on a
        // French one the first letter key is A.
        let mut de = with("de");
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x7a, true)]);
        assert_eq!(key(&mut de, KEY_Y, false), [sym(0x7a, false)]);
        assert_eq!(key(&mut de, KEY_Z, true), [sym(0x79, true)]);
        let mut fr = with("fr");
        assert_eq!(key(&mut fr, KEY_Q, true), [sym(0x61, true)]);
        let mut us = with("us");
        assert_eq!(key(&mut us, KEY_A, true), [sym(0x61, true)]);
    }

    #[test]
    fn shift_and_altgr_choose_the_character_and_are_not_sent() {
        let mut de = with("de");
        assert!(key(&mut de, KEY_LEFTSHIFT, true).is_empty());
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x5a, true)], "Z");
        assert_eq!(key(&mut de, KEY_Y, false), [sym(0x5a, false)]);
        assert!(key(&mut de, KEY_LEFTSHIFT, false).is_empty());
        // AltGr+Q is @ on a German layout.
        assert!(key(&mut de, KEY_RIGHTALT, true).is_empty());
        assert_eq!(key(&mut de, KEY_Q, true), [sym(0x40, true)], "@");
    }

    #[test]
    fn a_shifted_digit_that_is_a_digit_on_this_layout_is_not_shifted_twice() {
        // French Shift+1 is "1": a Shift on the server would make it "!".
        let mut fr = with("fr");
        assert!(key(&mut fr, KEY_LEFTSHIFT, true).is_empty());
        assert_eq!(key(&mut fr, KEY_1, true), [sym(0x31, true)]);
    }

    #[test]
    fn shift_reaches_the_server_for_a_key_that_is_not_a_character() {
        let mut us = with("us");
        assert!(key(&mut us, KEY_LEFTSHIFT, true).is_empty());
        // Shift+Left selects: the server needs the Shift, once.
        assert_eq!(
            key(&mut us, KEY_LEFT, true),
            [sym(SHIFT_L, true), sym(KEYSYM_LEFT, true)]
        );
        assert_eq!(key(&mut us, KEY_LEFT, false), [sym(KEYSYM_LEFT, false)]);
        assert_eq!(key(&mut us, KEY_LEFT, true), [sym(KEYSYM_LEFT, true)]);
        key(&mut us, KEY_LEFT, false);
        // And it lets go when the viewer does.
        assert_eq!(key(&mut us, KEY_LEFTSHIFT, false), [sym(SHIFT_L, false)]);
        // A Shift that never reached the server sends no release.
        key(&mut us, KEY_LEFTSHIFT, true);
        assert!(key(&mut us, KEY_LEFTSHIFT, false).is_empty());
    }

    #[test]
    fn control_goes_as_itself_and_does_not_drag_a_held_shift_along() {
        let mut us = with("us");
        assert!(key(&mut us, KEY_LEFTSHIFT, true).is_empty());
        assert_eq!(
            key(&mut us, KEY_LEFTCTRL, true),
            [sym(KEYSYM_CONTROL_L, true)]
        );
        // Ctrl+Shift+T: a character, so Mutter adds the Shift it needs.
        assert_eq!(key(&mut us, KEY_T, true), [sym(0x54, true)]);
    }

    #[test]
    fn a_held_shift_reaches_the_server_for_a_click() {
        let mut us = with("us");
        key(&mut us, KEY_LEFTSHIFT, true);
        let calls = us
            .translate(InputCmd::Button {
                button: 0x110,
                pressed: true,
            })
            .unwrap();
        assert_eq!(
            calls,
            [
                sym(SHIFT_L, true),
                Call::Button {
                    button: 0x110,
                    pressed: true
                }
            ]
        );
        // Only once.
        let again = us
            .translate(InputCmd::Button {
                button: 0x111,
                pressed: true,
            })
            .unwrap();
        assert_eq!(again.len(), 1);
    }

    #[test]
    fn a_release_matches_its_press_even_when_the_modifiers_changed_between() {
        let mut de = with("de");
        key(&mut de, KEY_LEFTSHIFT, true);
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x5a, true)]);
        key(&mut de, KEY_LEFTSHIFT, false);
        // Shift is up, but the key was pressed as Z.
        assert_eq!(key(&mut de, KEY_Y, false), [sym(0x5a, false)]);
    }

    #[test]
    fn a_repeating_key_keeps_its_symbol() {
        let mut de = with("de");
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x7a, true)]);
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x7a, true)]);
        assert_eq!(key(&mut de, KEY_Y, false), [sym(0x7a, false)]);
    }

    #[test]
    fn the_keypad_types_digits_while_the_server_has_num_lock_on() {
        const KEY_KP1: u32 = 79;
        // Num Lock on: the keypad's 1 is the plain character 1 (Mutter would
        // make KP_1 a shifted key, which Num Lock turns into End).
        let mut on = t();
        on.set_server_keymap(1, Some(layout("de")));
        on.set_num_lock(true);
        on.translate(InputCmd::SetKeymap(layout("us"))).unwrap();
        assert_eq!(key(&mut on, KEY_KP1, true), [sym(0x31, true)], "1");
        let mut off = t();
        off.set_server_keymap(1, Some(layout("de")));
        off.set_num_lock(false);
        off.translate(InputCmd::SetKeymap(layout("us"))).unwrap();
        assert_eq!(key(&mut off, KEY_KP1, true), [sym(0xff57, true)], "End");
        // The viewer's own Num Lock key toggles it, and is not sent.
        let mut us = with("us");
        assert!(key(&mut us, 69, true).is_empty());
        key(&mut us, 69, false);
        assert_eq!(key(&mut us, KEY_KP1, true), [sym(0xff57, true)], "now off");
        // A release_all keeps the starting state.
        us.translate(InputCmd::ReleaseAll).unwrap();
        assert_eq!(key(&mut us, KEY_KP1, true), [sym(0x31, true)], "back on");
    }

    #[test]
    fn caps_lock_acts_on_the_letters_here_and_is_not_sent() {
        let mut us = with("us");
        assert!(key(&mut us, KEY_CAPSLOCK, true).is_empty());
        assert!(key(&mut us, KEY_CAPSLOCK, false).is_empty());
        assert_eq!(key(&mut us, KEY_A, true), [sym(0x41, true)], "A");
        key(&mut us, KEY_A, false);
        key(&mut us, KEY_CAPSLOCK, true);
        key(&mut us, KEY_CAPSLOCK, false);
        assert_eq!(key(&mut us, KEY_A, true), [sym(0x61, true)], "a again");
    }

    #[test]
    fn a_key_the_layout_has_no_symbol_for_goes_as_its_code() {
        let mut us = with("us");
        assert_eq!(
            key(&mut us, 250, true),
            [Call::Key {
                code: 250,
                pressed: true
            }]
        );
        assert_eq!(
            key(&mut us, 250, false),
            [Call::Key {
                code: 250,
                pressed: false
            }]
        );
    }

    #[test]
    fn a_new_layout_lets_go_of_what_the_old_one_held() {
        let mut t = t();
        t.set_server_keymap(1, Some(layout("us")));
        key(&mut t, 29, true); // a code, before any layout
        let calls = t.translate(InputCmd::SetKeymap(layout("de"))).unwrap();
        assert_eq!(
            calls,
            [Call::Key {
                code: 29,
                pressed: false
            }]
        );
        key(&mut t, KEY_Y, true);
        let calls = t.translate(InputCmd::SetKeymap(layout("fr"))).unwrap();
        assert_eq!(calls.len(), 1, "{calls:?}");
        assert_eq!(calls[0], sym(0x7a, false));
        // The key still down on the viewer is let go later and sends nothing.
        assert!(key(&mut t, KEY_Y, false).is_empty());
        // The same layout again changes nothing.
        let same = layout("fr");
        assert!(t.translate(InputCmd::SetKeymap(same)).unwrap().is_empty());
    }

    #[test]
    fn release_all_and_stop_let_go_of_keysyms_too() {
        let mut de = with("de");
        key(&mut de, KEY_LEFTCTRL, true);
        key(&mut de, KEY_LEFTSHIFT, true);
        key(&mut de, KEY_Y, true);
        // Left lets go of Z and takes the viewer's Shift to the server.
        let left = key(&mut de, KEY_LEFT, true);
        assert!(left.contains(&sym(0x5a, false)), "{left:?}");
        let calls = de.translate(InputCmd::ReleaseAll).unwrap();
        assert!(calls.contains(&sym(KEYSYM_CONTROL_L, false)));
        assert!(calls.contains(&sym(KEYSYM_LEFT, false)));
        assert!(calls.contains(&sym(SHIFT_L, false)), "{calls:?}");
        assert!(de.translate(InputCmd::ReleaseAll).unwrap().is_empty());
        // After a release the viewer's Shift is gone from its state too.
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x7a, true)]);
        let calls = de.translate(InputCmd::Stop).unwrap();
        assert_eq!(calls, [sym(0x7a, false)]);
    }

    #[test]
    fn keypad_navigation_is_plain_navigation_whatever_the_servers_num_lock() {
        // KP_Left would be Left only with the server's Num Lock off.
        let mut us = with("us");
        key(&mut us, 69, true); // the viewer's Num Lock: off
        key(&mut us, 69, false);
        assert_eq!(key(&mut us, 75, true), [sym(0xff51, true)], "Left");
        assert_eq!(key(&mut us, 83, true), [sym(0xffff, true)], "Delete");
        assert_eq!(key(&mut us, 96, true), [sym(0xff0d, true)], "KP_Enter");
    }

    #[test]
    fn a_held_character_is_let_go_before_a_key_that_is_not_one_or_a_pointer_event() {
        // Mutter's Shift for Z stays down while the key does: Left or a click
        // right then would be Shift+Left, a selection.
        let mut de = with("de");
        key(&mut de, KEY_LEFTSHIFT, true);
        assert_eq!(key(&mut de, KEY_Y, true), [sym(0x5a, true)]);
        key(&mut de, KEY_LEFTSHIFT, false);
        assert_eq!(
            key(&mut de, KEY_LEFT, true),
            [sym(0x5a, false), sym(KEYSYM_LEFT, true)]
        );
        assert!(key(&mut de, KEY_Y, false).is_empty(), "already let go");
        key(&mut de, KEY_LEFT, false);
        // The same before a button.
        key(&mut de, KEY_A, true);
        assert_eq!(
            de.translate(InputCmd::Button {
                button: 0x110,
                pressed: true
            })
            .unwrap(),
            [
                sym(0x61, false),
                Call::Button {
                    button: 0x110,
                    pressed: true
                }
            ]
        );
    }

    #[test]
    fn shift_tab_is_tab_with_our_shift_and_leaves_nothing_behind() {
        // ISO_Left_Tab would get a Shift from Mutter that outlives ours.
        const KEY_TAB: u32 = 15;
        let mut us = with("us");
        key(&mut us, KEY_LEFTSHIFT, true);
        assert_eq!(
            key(&mut us, KEY_TAB, true),
            [sym(SHIFT_L, true), sym(0xff09, true)]
        );
        key(&mut us, KEY_LEFTSHIFT, false);
        // Tab is still down; the next character first lets go of nothing
        // Mutter shifted for it.
        let a = key(&mut us, KEY_A, true);
        assert_eq!(a.last(), Some(&sym(0x61, true)));
        assert!(!a.contains(&sym(0xff09, true)));
    }

    #[test]
    fn a_held_dead_key_is_let_go_before_the_next_character() {
        // US International: Shift+apostrophe is dead_diaeresis; on a French
        // server it sits on the shifted level, so Mutter's Shift for it must
        // not reach the next letter.
        const KEY_APOSTROPHE: u32 = 40;
        let mut t = with_server("us(intl)", "fr");
        key(&mut t, KEY_LEFTSHIFT, true);
        let dead = key(&mut t, KEY_APOSTROPHE, true);
        assert_eq!(dead, [sym(0xfe57, true)], "dead_diaeresis");
        key(&mut t, KEY_LEFTSHIFT, false);
        assert_eq!(
            key(&mut t, KEY_A, true),
            [sym(0xfe57, false), sym(0x61, true)]
        );
        assert!(key(&mut t, KEY_APOSTROPHE, false).is_empty());
    }

    #[test]
    fn a_server_with_several_layouts_keeps_key_codes_for_a_viewer_layout_it_has() {
        // Which layout is active there is not known, so a viewer on any of
        // them keeps key codes.
        for viewer in ["us", "de"] {
            let mut t = t();
            t.set_server_keymap(1, Some(layout("us,de")));
            t.translate(InputCmd::SetKeymap(layout(viewer))).unwrap();
            assert_eq!(t.key_mode(), Some(crate::KeyMode::Codes), "{viewer}");
            assert_eq!(
                key(&mut t, KEY_A, true),
                [Call::Key {
                    code: KEY_A,
                    pressed: true
                }]
            );
        }
    }

    #[test]
    fn a_viewer_layout_the_server_does_not_have_sends_symbols_whatever_is_active() {
        let mut t = t();
        t.set_server_keymap(1, Some(layout("us,de")));
        t.translate(InputCmd::SetKeymap(layout("fr"))).unwrap();
        assert_eq!(t.key_mode(), Some(crate::KeyMode::Keysyms));
        // The key at the position of A on a French keyboard types q.
        assert_eq!(key(&mut t, KEY_A, true), [sym(0x71, true)]);
    }

    #[test]
    fn a_keymap_that_does_not_compile_is_an_error_not_silently_ignored() {
        let err = t()
            .translate(InputCmd::SetKeymap("xkb_keymap {}".into()))
            .unwrap_err();
        assert!(err.to_string().contains("keymap"));
    }
}
