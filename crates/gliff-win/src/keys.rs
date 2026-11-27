//! Physical key and button numbers on the wire. The server takes Linux
//! evdev codes (`KEY_*`, `BTN_*`), so the layout the user types on is the
//! keymap's business and a key is named by where it sits on the keyboard.

use winit::event::MouseButton;
use winit::keyboard::KeyCode;

/// The evdev `KEY_*` code of a physical key, or `None` for a key the server
/// has no code for.
pub fn evdev_key(key: KeyCode) -> Option<u32> {
    use KeyCode::*;
    Some(match key {
        Escape => 1,
        Digit1 => 2,
        Digit2 => 3,
        Digit3 => 4,
        Digit4 => 5,
        Digit5 => 6,
        Digit6 => 7,
        Digit7 => 8,
        Digit8 => 9,
        Digit9 => 10,
        Digit0 => 11,
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        KeyQ => 16,
        KeyW => 17,
        KeyE => 18,
        KeyR => 19,
        KeyT => 20,
        KeyY => 21,
        KeyU => 22,
        KeyI => 23,
        KeyO => 24,
        KeyP => 25,
        BracketLeft => 26,
        BracketRight => 27,
        Enter => 28,
        ControlLeft => 29,
        KeyA => 30,
        KeyS => 31,
        KeyD => 32,
        KeyF => 33,
        KeyG => 34,
        KeyH => 35,
        KeyJ => 36,
        KeyK => 37,
        KeyL => 38,
        Semicolon => 39,
        Quote => 40,
        Backquote => 41,
        ShiftLeft => 42,
        Backslash => 43,
        KeyZ => 44,
        KeyX => 45,
        KeyC => 46,
        KeyV => 47,
        KeyB => 48,
        KeyN => 49,
        KeyM => 50,
        Comma => 51,
        Period => 52,
        Slash => 53,
        ShiftRight => 54,
        NumpadMultiply => 55,
        AltLeft => 56,
        Space => 57,
        CapsLock => 58,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        NumLock => 69,
        ScrollLock => 70,
        Numpad7 => 71,
        Numpad8 => 72,
        Numpad9 => 73,
        NumpadSubtract => 74,
        Numpad4 => 75,
        Numpad5 => 76,
        Numpad6 => 77,
        NumpadAdd => 78,
        Numpad1 => 79,
        Numpad2 => 80,
        Numpad3 => 81,
        Numpad0 => 82,
        NumpadDecimal => 83,
        IntlBackslash => 86,
        F11 => 87,
        F12 => 88,
        IntlRo => 89,
        NumpadEnter => 96,
        ControlRight => 97,
        NumpadDivide => 98,
        PrintScreen => 99,
        AltRight => 100,
        Home => 102,
        ArrowUp => 103,
        PageUp => 104,
        ArrowLeft => 105,
        ArrowRight => 106,
        End => 107,
        ArrowDown => 108,
        PageDown => 109,
        Insert => 110,
        Delete => 111,
        Pause => 119,
        IntlYen => 124,
        SuperLeft => 125,
        SuperRight => 126,
        ContextMenu => 127,
        F13 => 183,
        F14 => 184,
        F15 => 185,
        F16 => 186,
        F17 => 187,
        F18 => 188,
        F19 => 189,
        F20 => 190,
        F21 => 191,
        F22 => 192,
        F23 => 193,
        F24 => 194,
        _ => return None,
    })
}

/// The evdev `BTN_*` code of a mouse button, or `None` for one the server
/// has no code for.
pub fn evdev_button(button: MouseButton) -> Option<u32> {
    match button {
        MouseButton::Left => Some(0x110),    // BTN_LEFT
        MouseButton::Right => Some(0x111),   // BTN_RIGHT
        MouseButton::Middle => Some(0x112),  // BTN_MIDDLE
        MouseButton::Back => Some(0x113),    // BTN_SIDE
        MouseButton::Forward => Some(0x114), // BTN_EXTRA
        MouseButton::Other(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_named_by_position() {
        assert_eq!(evdev_key(KeyCode::KeyA), Some(30));
        assert_eq!(evdev_key(KeyCode::Enter), Some(28));
        assert_eq!(evdev_key(KeyCode::SuperLeft), Some(125));
        assert_eq!(evdev_key(KeyCode::ArrowDown), Some(108));
        assert_eq!(evdev_key(KeyCode::F12), Some(88));
    }

    #[test]
    fn a_key_without_a_code_is_dropped() {
        assert_eq!(evdev_key(KeyCode::AudioVolumeUp), None);
    }

    #[test]
    fn no_two_keys_share_a_code() {
        use std::collections::HashMap;
        let all = [
            KeyCode::Escape,
            KeyCode::Digit1,
            KeyCode::Digit0,
            KeyCode::Minus,
            KeyCode::Equal,
            KeyCode::Backspace,
            KeyCode::Tab,
            KeyCode::KeyQ,
            KeyCode::KeyP,
            KeyCode::BracketLeft,
            KeyCode::BracketRight,
            KeyCode::Enter,
            KeyCode::ControlLeft,
            KeyCode::KeyA,
            KeyCode::KeyL,
            KeyCode::Semicolon,
            KeyCode::Quote,
            KeyCode::Backquote,
            KeyCode::ShiftLeft,
            KeyCode::Backslash,
            KeyCode::KeyZ,
            KeyCode::KeyM,
            KeyCode::Comma,
            KeyCode::Period,
            KeyCode::Slash,
            KeyCode::ShiftRight,
            KeyCode::NumpadMultiply,
            KeyCode::AltLeft,
            KeyCode::Space,
            KeyCode::CapsLock,
            KeyCode::F1,
            KeyCode::F10,
            KeyCode::NumLock,
            KeyCode::ScrollLock,
            KeyCode::Numpad7,
            KeyCode::NumpadSubtract,
            KeyCode::NumpadAdd,
            KeyCode::Numpad0,
            KeyCode::NumpadDecimal,
            KeyCode::IntlBackslash,
            KeyCode::F11,
            KeyCode::F12,
            KeyCode::IntlRo,
            KeyCode::NumpadEnter,
            KeyCode::ControlRight,
            KeyCode::NumpadDivide,
            KeyCode::PrintScreen,
            KeyCode::AltRight,
            KeyCode::Home,
            KeyCode::ArrowUp,
            KeyCode::PageUp,
            KeyCode::ArrowLeft,
            KeyCode::ArrowRight,
            KeyCode::End,
            KeyCode::ArrowDown,
            KeyCode::PageDown,
            KeyCode::Insert,
            KeyCode::Delete,
            KeyCode::Pause,
            KeyCode::IntlYen,
            KeyCode::SuperLeft,
            KeyCode::SuperRight,
            KeyCode::ContextMenu,
            KeyCode::F13,
            KeyCode::F24,
        ];
        let mut seen: HashMap<u32, KeyCode> = HashMap::new();
        for key in all {
            let code = evdev_key(key).unwrap_or_else(|| panic!("{key:?} has no code"));
            if let Some(other) = seen.insert(code, key) {
                panic!("{key:?} and {other:?} both map to {code}");
            }
        }
    }

    #[test]
    fn buttons_match_the_gtk_viewer() {
        assert_eq!(evdev_button(MouseButton::Left), Some(0x110));
        assert_eq!(evdev_button(MouseButton::Middle), Some(0x112));
        assert_eq!(evdev_button(MouseButton::Right), Some(0x111));
        assert_eq!(evdev_button(MouseButton::Back), Some(0x113));
        assert_eq!(evdev_button(MouseButton::Forward), Some(0x114));
        assert_eq!(evdev_button(MouseButton::Other(7)), None);
    }
}
