//! The keyboard layout the **Machine** types with. Keys go by position (see
//! `keys`), so which characters they make is the keymap's business: the
//! viewer names its Windows layout in short XKB text, which the server's
//! xkbcommon completes from its own XKB data.

/// Windows layout IDs (KLIDs) and the XKB layout, with its variant, that
/// puts the same characters on the same keys. Layouts typed through an IME
/// (Chinese) sit on the US keys.
const TABLE: &[(&str, &str)] = &[
    ("00000401", "ara"),            // Arabic (101)
    ("00000402", "bg"),             // Bulgarian (Typewriter)
    ("00000404", "us"),             // Chinese (Traditional) - US
    ("00000405", "cz"),             // Czech
    ("00000406", "dk"),             // Danish
    ("00000407", "de"),             // German
    ("00000408", "gr"),             // Greek
    ("00000409", "us"),             // US
    ("0000040A", "es"),             // Spanish
    ("0000040B", "fi"),             // Finnish
    ("0000040C", "fr"),             // French (Legacy, AZERTY)
    ("0000040D", "il"),             // Hebrew
    ("0000040E", "hu"),             // Hungarian
    ("0000040F", "is"),             // Icelandic
    ("00000410", "it"),             // Italian
    ("00000411", "jp"),             // Japanese
    ("00000412", "kr"),             // Korean
    ("00000413", "nl"),             // Dutch
    ("00000414", "no"),             // Norwegian
    ("00000415", "pl(qwertz)"),     // Polish (214)
    ("00000416", "br"),             // Portuguese (Brazil ABNT)
    ("00000418", "ro(winkeys)"),    // Romanian (Legacy)
    ("00000419", "ru"),             // Russian
    ("0000041A", "hr"),             // Croatian
    ("0000041B", "sk"),             // Slovak
    ("0000041D", "se"),             // Swedish
    ("0000041E", "th"),             // Thai Kedmanee
    ("0000041F", "tr"),             // Turkish Q
    ("00000422", "ua"),             // Ukrainian
    ("00000423", "by"),             // Belarusian
    ("00000424", "si"),             // Slovenian
    ("00000425", "ee"),             // Estonian
    ("00000426", "lv"),             // Latvian
    ("00000427", "lt(ibm)"),        // Lithuanian IBM
    ("00000429", "ir"),             // Persian
    ("0000042A", "vn"),             // Vietnamese
    ("00000438", "fo"),             // Faeroese
    ("0000043F", "kz"),             // Kazakh
    ("00000452", "gb(extd)"),       // United Kingdom Extended
    ("00000804", "us"),             // Chinese (Simplified) - US
    ("00000807", "ch"),             // Swiss German
    ("00000809", "gb"),             // United Kingdom
    ("0000080A", "latam"),          // Latin American
    ("0000080C", "be"),             // Belgian French
    ("00000813", "be"),             // Belgian (Period)
    ("00000816", "pt"),             // Portuguese
    ("0000081A", "rs(latin)"),      // Serbian (Latin)
    ("00000C0C", "ca(fr-legacy)"),  // Canadian French (Legacy)
    ("00000C1A", "rs"),             // Serbian (Cyrillic)
    ("00001009", "ca"),             // Canadian French
    ("0000100C", "ch(fr)"),         // Swiss French
    ("00001809", "ie"),             // Irish
    ("00004009", "in(eng)"),        // India
    ("00010405", "cz(qwerty)"),     // Czech (QWERTY)
    ("00010407", "de"),             // German (IBM)
    ("00010409", "us(dvorak)"),     // United States-Dvorak
    ("0001040A", "es"),             // Spanish Variation
    ("0001040C", "fr(afnor)"),      // French (Standard, AZERTY)
    ("00010410", "it"),             // Italian (142)
    ("00010415", "pl"),             // Polish (Programmers)
    ("00010416", "br"),             // Portuguese (Brazil ABNT2)
    ("00010418", "ro(std)"),        // Romanian (Standard)
    ("00010419", "ru(typewriter)"), // Russian (Typewriter)
    ("0001041B", "sk(qwerty)"),     // Slovak (QWERTY)
    ("0001041F", "tr(f)"),          // Turkish F
    ("00010427", "lt"),             // Lithuanian
    ("0001080C", "be"),             // Belgian (Comma)
    ("00011009", "ca(multix)"),     // Canadian Multilingual Standard
    ("00020409", "us(intl)"),       // United States-International
    ("00020418", "ro"),             // Romanian (Programmers)
    ("00020422", "ua"),             // Ukrainian (Enhanced)
    ("00020426", "lv"),             // Latvian (Standard)
    ("00030402", "bg"),             // Bulgarian
    ("00030409", "us(dvorak-l)"),   // United States-Dvorak for left hand
    ("00040409", "us(dvorak-r)"),   // United States-Dvorak for right hand
];

/// The XKB layout, with its variant, for a Windows layout ID (a KLID, as
/// `GetKeyboardLayoutNameW` gives it), or `None` for a layout with no entry.
pub fn xkb_layout(klid: &str) -> Option<&'static str> {
    TABLE
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(klid))
        .map(|(_, xkb)| *xkb)
}

/// XKB keymap text that only names `layout` (`de`, or `de(nodeadkeys)`):
/// the includes `setxkbmap` would print for it on a pc105 keyboard, which
/// the server resolves against its own XKB data.
pub fn keymap_text(layout: &str) -> String {
    format!(
        "xkb_keymap {{\n\
         \txkb_keycodes {{ include \"evdev+aliases(qwerty)\" }};\n\
         \txkb_types {{ include \"complete\" }};\n\
         \txkb_compat {{ include \"complete\" }};\n\
         \txkb_symbols {{ include \"pc+{layout}+inet(evdev)\" }};\n\
         }};\n"
    )
}

/// What the **Session** sends for a Windows layout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    /// XKB keymap text for `Hello` or `Keymap`.
    pub text: String,
    /// The line for the tab when the layout has no entry and US stands in.
    pub notice: Option<String>,
}

/// The keymap for a Windows layout; US, with a notice, for one with no entry.
pub fn keymap(klid: &str) -> Keymap {
    match xkb_layout(klid) {
        Some(layout) => Keymap {
            text: keymap_text(layout),
            notice: None,
        },
        None => Keymap {
            text: keymap_text("us"),
            notice: Some(if klid.is_empty() {
                "keyboard layout unknown, typing as US".to_owned()
            } else {
                format!("keyboard layout {klid} unknown, typing as US")
            }),
        },
    }
}

/// The Windows keyboard layout of the calling thread (the window's), as a
/// KLID such as `00000407`; empty if Windows cannot say.
#[cfg(windows)]
pub fn current() -> String {
    use windows_sys::Win32::UI::Input::KeyboardAndMouse::GetKeyboardLayoutNameW;
    use windows_sys::Win32::UI::WindowsAndMessaging::KL_NAMELENGTH;
    let mut name = [0u16; KL_NAMELENGTH as usize];
    // SAFETY: the buffer holds KL_NAMELENGTH characters, as the call needs.
    if unsafe { GetKeyboardLayoutNameW(name.as_mut_ptr()) } == 0 {
        return String::new();
    }
    let len = name.iter().position(|&c| c == 0).unwrap_or(name.len());
    String::from_utf16_lossy(&name[..len])
}

/// Off Windows there is no Windows layout to read.
#[cfg(not(windows))]
pub fn current() -> String {
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_layouts_are_named_in_xkb() {
        assert_eq!(xkb_layout("00000407"), Some("de"));
        assert_eq!(xkb_layout("0000040C"), Some("fr"));
        assert_eq!(xkb_layout("00000409"), Some("us"));
        assert_eq!(xkb_layout("00010409"), Some("us(dvorak)"));
        assert_eq!(xkb_layout("00000807"), Some("ch"));
    }

    #[test]
    fn a_layout_id_matches_in_either_case() {
        assert_eq!(xkb_layout("0000040c"), Some("fr"));
    }

    #[test]
    fn a_layout_with_no_entry_has_no_name() {
        assert_eq!(xkb_layout("00000000"), None);
        assert_eq!(xkb_layout("a0000407"), None); // a custom (MSKLC) layout
        assert_eq!(xkb_layout(""), None);
    }

    #[test]
    fn no_layout_id_is_listed_twice() {
        let mut seen = std::collections::HashSet::new();
        for (klid, _) in TABLE {
            assert!(
                seen.insert(klid.to_ascii_uppercase()),
                "{klid} is listed twice"
            );
        }
    }

    /// The keymap text compiles against this machine's XKB data, as the
    /// server's xkbcommon compiles it against its own.
    #[cfg(unix)]
    mod xkb {
        use super::super::*;
        use xkbcommon::xkb;

        fn compile(text: &str) -> Option<xkb::Keymap> {
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            xkb::Keymap::new_from_string(
                &ctx,
                text.to_owned(),
                xkb::KEYMAP_FORMAT_TEXT_V1,
                xkb::KEYMAP_COMPILE_NO_FLAGS,
            )
        }

        /// The keysym name on the first level of the key with this evdev code.
        fn symbol(keymap: &xkb::Keymap, evdev: u32) -> String {
            keymap
                .key_get_syms_by_level(xkb::Keycode::new(evdev + 8), 0, 0)
                .first()
                .map_or_else(String::new, |s| xkb::keysym_get_name(*s))
        }

        #[test]
        fn the_text_names_the_layout() {
            const KEY_Y: u32 = 21;
            const KEY_Q: u32 = 16;
            let de = compile(&keymap_text("de")).expect("de compiles");
            assert_eq!(symbol(&de, KEY_Y), "z");
            let fr = compile(&keymap_text("fr")).expect("fr compiles");
            assert_eq!(symbol(&fr, KEY_Q), "a");
            let us = compile(&keymap_text("us")).expect("us compiles");
            assert_eq!(symbol(&us, KEY_Y), "y");
        }

        #[test]
        fn a_name_xkb_does_not_know_does_not_compile() {
            assert!(compile(&keymap_text("de(nosuchvariant)")).is_none());
        }

        #[test]
        fn every_layout_in_the_table_compiles() {
            for (klid, layout) in TABLE {
                assert!(
                    compile(&keymap_text(layout)).is_some(),
                    "{klid}: {layout} does not compile"
                );
            }
        }
    }
}
