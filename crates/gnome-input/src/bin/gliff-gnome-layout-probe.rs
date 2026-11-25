//! Check keymap forwarding on a real GNOME session: type with a viewer layout
//! that is not the desktop's and read what a text field actually received.
//!
//! It opens a fullscreen window with one text field on the primary monitor,
//! clicks into it, and for each scenario sends the viewer's layout
//! (`SetKeymap`) and a few keys by evdev code, the way a viewer would, then
//! compares the field's text with what that layout types. One line per
//! scenario, `[ OK ]` or `[FAIL]`.
//!
//! The window covers the screen and takes the pointer and keys for a few
//! seconds: run it on a machine nobody is using.
//!
//! `gliff-gnome-layout-probe [LAYOUT...]` runs only the scenarios for the
//! named viewer layouts (`us`, `de`, `fr`, `ru`); with none it runs them all.
//! With `LAYOUT_PROBE_SWITCH=1` it first presses Super+Space on the desktop
//! (GNOME's default shortcut to switch layout, when there are several) and
//! waits up to 3 s for the server to see the layout in use change, printing it
//! before and after; it fails if the keys cannot be sent or nothing changes.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::time::Duration;

use gnome_input::{Input, InputCmd, ServerKeymap};
use gnome_ipc::{discover, mutter, CursorMode, Session, StreamSource};
use gtk4::prelude::*;
use xkbcommon::xkb;

const BTN_LEFT: u32 = 0x110;

// evdev codes of the keys the scenarios press (the names are US legends).
const KEY_1: u32 = 2;
const KEY_0: u32 = 11;
const KEY_MINUS: u32 = 12;
const KEY_Q: u32 = 16;
const KEY_Y: u32 = 21;
const KEY_LEFTBRACE: u32 = 26;
const KEY_BACKSPACE: u32 = 14;
const KEY_A: u32 = 30;
const KEY_B: u32 = 48;
const KEY_C: u32 = 46;
const KEY_LEFTCTRL: u32 = 29;
const KEY_LEFT: u32 = 105;
const KEY_SEMICOLON: u32 = 39;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_Z: u32 = 44;
const KEY_SPACE: u32 = 57;
const KEY_CAPSLOCK: u32 = 58;
const KEY_RIGHTALT: u32 = 100;

#[derive(Clone, Copy)]
enum Step {
    Tap(u32),
    Down(u32),
    Up(u32),
}
use Step::{Down, Tap, Up};

struct Scenario {
    what: &'static str,
    layout: &'static str,
    steps: &'static [Step],
    expect: &'static str,
    /// The characters have no key on the server's layout, and Mutter's keysym
    /// call does not make one: nothing typed is the known result, reported
    /// as such, not as a failure.
    known_limit: bool,
}

const SCENARIOS: &[Scenario] = &[
    Scenario {
        what: "us: a, and Shift+a",
        layout: "us",
        steps: &[
            Tap(KEY_A),
            Down(KEY_LEFTSHIFT),
            Tap(KEY_A),
            Up(KEY_LEFTSHIFT),
        ],
        expect: "aA",
        known_limit: false,
    },
    Scenario {
        what: "us: space and digits",
        layout: "us",
        steps: &[Tap(KEY_1), Tap(KEY_SPACE), Tap(KEY_0)],
        expect: "1 0",
        known_limit: false,
    },
    Scenario {
        what: "us: Caps Lock acts on the letters (A, then a again)",
        layout: "us",
        steps: &[Tap(KEY_CAPSLOCK), Tap(KEY_A), Tap(KEY_CAPSLOCK), Tap(KEY_A)],
        expect: "Aa",
        known_limit: false,
    },
    Scenario {
        what: "us: Ctrl+A selects all, so the next letter replaces the text",
        layout: "us",
        steps: &[
            Tap(KEY_A),
            Tap(KEY_B),
            Down(KEY_LEFTCTRL),
            Tap(KEY_A),
            Up(KEY_LEFTCTRL),
            Tap(KEY_C),
        ],
        expect: "c",
        known_limit: false,
    },
    Scenario {
        what: "us: Shift+Left selects one character, so the next letter replaces it",
        layout: "us",
        steps: &[
            Tap(KEY_A),
            Tap(KEY_B),
            Down(KEY_LEFTSHIFT),
            Tap(KEY_LEFT),
            Up(KEY_LEFTSHIFT),
            Tap(KEY_C),
        ],
        expect: "ac",
        known_limit: false,
    },
    Scenario {
        what: "us: BackSpace erases",
        layout: "us",
        steps: &[Tap(KEY_A), Tap(KEY_B), Tap(KEY_BACKSPACE)],
        expect: "a",
        known_limit: false,
    },
    Scenario {
        what: "de: a letter after Shift+Y while Y is still down is not shifted (rollover)",
        layout: "de",
        steps: &[
            Down(KEY_LEFTSHIFT),
            Down(KEY_Y),
            Up(KEY_LEFTSHIFT),
            Down(KEY_A),
            Up(KEY_Y),
            Up(KEY_A),
        ],
        expect: "Za",
        known_limit: false,
    },
    Scenario {
        what: "fr: Shift held, Left, then 1: still 1, not !",
        layout: "fr",
        steps: &[
            Down(KEY_LEFTSHIFT),
            Tap(KEY_LEFT),
            Tap(KEY_1),
            Up(KEY_LEFTSHIFT),
        ],
        expect: "1",
        known_limit: false,
    },
    Scenario {
        what: "de: the Y and Z keys swap",
        layout: "de",
        steps: &[Tap(KEY_Y), Tap(KEY_Z)],
        expect: "zy",
        known_limit: false,
    },
    Scenario {
        what: "de: Shift+Y is Z",
        layout: "de",
        steps: &[Down(KEY_LEFTSHIFT), Tap(KEY_Y), Up(KEY_LEFTSHIFT)],
        expect: "Z",
        known_limit: false,
    },
    Scenario {
        what: "de: AltGr+Q is @",
        layout: "de",
        steps: &[Down(KEY_RIGHTALT), Tap(KEY_Q), Up(KEY_RIGHTALT)],
        expect: "@",
        known_limit: false,
    },
    Scenario {
        what: "de: ö, ß and ü, which a US desktop has no keys for",
        layout: "de",
        steps: &[Tap(KEY_SEMICOLON), Tap(KEY_MINUS), Tap(KEY_LEFTBRACE)],
        expect: "ößü",
        known_limit: true,
    },
    Scenario {
        what: "fr: the Q key is a, A is q, the semicolon key is m",
        layout: "fr",
        steps: &[Tap(KEY_Q), Tap(KEY_A), Tap(KEY_SEMICOLON)],
        expect: "aqm",
        known_limit: false,
    },
    Scenario {
        what: "fr: the digit row gives & and, with Shift, 1",
        layout: "fr",
        steps: &[
            Tap(KEY_1),
            Down(KEY_LEFTSHIFT),
            Tap(KEY_1),
            Up(KEY_LEFTSHIFT),
        ],
        expect: "&1",
        known_limit: false,
    },
    Scenario {
        what: "ru: the Q key is й",
        layout: "ru",
        steps: &[Tap(KEY_Q)],
        expect: "й",
        known_limit: true,
    },
];

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

fn layout_text(name: &str) -> Result<String, String> {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    xkb::Keymap::new_from_names(&ctx, "", "", name, "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
        .map(|k| k.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1))
        .ok_or_else(|| format!("cannot compile layout {name}"))
}

/// For the log: how many layouts the server's keymap has, and for each how
/// many keys differ from the viewer's first layout (0 means the viewer's
/// layout counts as that one, which keeps key codes).
fn describe(viewer: &str, server: &str) -> String {
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let compile = |text: &str| {
        xkb::Keymap::new_from_string(
            &ctx,
            text.to_owned(),
            xkb::KEYMAP_FORMAT_TEXT_V1,
            xkb::KEYMAP_COMPILE_NO_FLAGS,
        )
    };
    let (Some(v), Some(s)) = (compile(viewer), compile(server)) else {
        return "cannot compile a keymap".into();
    };
    let lo = v.min_keycode().raw().min(s.min_keycode().raw());
    let hi = v.max_keycode().raw().max(s.max_keycode().raw());
    let groups: Vec<String> = (0..s.num_layouts())
        .map(|g| {
            let differ: Vec<xkb::Keycode> = (lo..=hi)
                .map(xkb::Keycode::new)
                .filter(|&key| {
                    let levels = v.num_levels_for_key(key, 0);
                    !(levels == s.num_levels_for_key(key, g)
                        && (0..levels).all(|l| {
                            v.key_get_syms_by_level(key, 0, l) == s.key_get_syms_by_level(key, g, l)
                        }))
                })
                .collect();
            // A handful of keys: name them, that is the interesting case.
            let which = if differ.len() <= 8 {
                let names: Vec<String> = differ
                    .iter()
                    .map(|&k| {
                        let syms = |m: &xkb::Keymap, g| {
                            (0..m.num_levels_for_key(k, g))
                                .map(|l| {
                                    m.key_get_syms_by_level(k, g, l)
                                        .iter()
                                        .map(|x| xkb::keysym_get_name(*x))
                                        .collect::<Vec<_>>()
                                        .join("+")
                                })
                                .collect::<Vec<_>>()
                                .join("/")
                        };
                        format!(
                            "{} {} viewer [{}] server [{}]",
                            k.raw(),
                            v.key_get_name(k).unwrap_or("?"),
                            syms(&v, 0),
                            syms(&s, g)
                        )
                    })
                    .collect();
                format!(" ({})", names.join("; "))
            } else {
                String::new()
            };
            format!(
                "{:?}: {} keys differ{which}",
                s.layout_get_name(g),
                differ.len()
            )
        })
        .collect();
    format!("server layouts {}", groups.join(", "))
}

/// What the worker asks the window thread for.
enum Request {
    /// Read the text field and empty it.
    Take(Sender<String>),
}

fn press(input: &Input, code: u32, pressed: bool) -> Result<(), String> {
    input
        .send(InputCmd::Key { code, pressed })
        .map_err(|e| format!("{e:#}"))
}

/// How long a key is held, `GLIFF_TAP_MS` (default 25).
fn hold() -> Duration {
    let ms = std::env::var("GLIFF_TAP_MS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(25);
    Duration::from_millis(ms)
}

fn tap(input: &Input, code: u32) -> Result<(), String> {
    press(input, code, true)?;
    std::thread::sleep(hold());
    press(input, code, false)?;
    std::thread::sleep(Duration::from_millis(25));
    Ok(())
}

fn work(requests: Sender<Request>) -> bool {
    let take = || -> String {
        let (tx, rx) = channel();
        let _ = requests.send(Request::Take(tx));
        rx.recv_timeout(Duration::from_secs(3)).unwrap_or_default()
    };
    let desktop = match discover::find() {
        Ok(s) => s,
        Err(e) => return fail("session", format!("{e:#}")),
    };
    let conn = match mutter::connect(&desktop.bus_address) {
        Ok(c) => c,
        Err(e) => return fail("session bus", format!("{e:#}")),
    };
    let monitors = match mutter::monitors(&conn) {
        Ok(m) => m,
        Err(e) => return fail("monitors", format!("{e:#}")),
    };
    let Some(mon) = monitors.iter().find(|m| m.primary).or(monitors.first()) else {
        return fail("monitor", "none");
    };
    let extent = (
        (f64::from(mon.width) / mon.scale) as u32,
        (f64::from(mon.height) / mon.scale) as u32,
    );
    // The desktop may be asleep, and a sleeping one refuses sessions.
    let _ = gnome_ipc::power::wake(&conn);
    let ses = match Session::create(&desktop.bus_address) {
        Ok(s) => s,
        Err(e) => return fail("create session", format!("{e:#}")),
    };
    let stream = match ses.record(
        &StreamSource::Monitor(mon.connector.clone()),
        CursorMode::Hidden,
    ) {
        Ok(s) => s,
        Err(e) => return fail("record", format!("{e:#}")),
    };
    if let Err(e) = ses.start() {
        return fail("start", format!("{e:#}"));
    }
    if let Err(e) = stream.wait_node(Duration::from_secs(5)) {
        return fail("stream", format!("{e:#}"));
    }
    // The desktop's own layout decides how keys go; wait for it to arrive.
    let server = ServerKeymap::follow(&desktop);
    for _ in 0..60 {
        if server.is_known() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if !server.is_known() {
        return fail(
            "server layout",
            "the desktop's keymap did not arrive in 3 s",
        );
    }
    ok("server layout", "read from the compositor");
    let server_text = server.text();
    let in_use = server.clone();
    let input = Input::new(ses.input(), stream.path(), extent).with_server_keymap(server);

    // Give the window time to appear, then click into it.
    std::thread::sleep(Duration::from_millis(2500));
    let click = || -> Result<(), String> {
        input
            .send(InputCmd::Motion {
                x: f64::from(extent.0) / 2.0,
                y: f64::from(extent.1) / 2.0,
            })
            .map_err(|e| format!("{e:#}"))?;
        std::thread::sleep(Duration::from_millis(100));
        input
            .send(InputCmd::Button {
                button: BTN_LEFT,
                pressed: true,
            })
            .map_err(|e| format!("{e:#}"))?;
        input
            .send(InputCmd::Button {
                button: BTN_LEFT,
                pressed: false,
            })
            .map_err(|e| format!("{e:#}"))
    };
    if let Err(e) = click() {
        return fail("click", e);
    }
    std::thread::sleep(Duration::from_millis(400));
    // `LAYOUT_PROBE_SWITCH=1`: switch the desktop's layout the way a viewer
    // user would (Super+Space, as key codes, before any viewer layout is
    // set), and wait up to 3 s for the server to see the layout in use
    // change. It fails when the keys cannot be sent or no change is seen
    // (one layout, the shortcut off or rebound), so a run that was meant to
    // test a switch cannot pass without one.
    if std::env::var("LAYOUT_PROBE_SWITCH").is_ok_and(|v| v == "1") {
        const KEY_LEFTMETA: u32 = 125;
        let before = in_use.active_layout();
        let mut sent = Ok(());
        for (code, pressed) in [
            (KEY_LEFTMETA, true),
            (KEY_SPACE, true),
            (KEY_SPACE, false),
            (KEY_LEFTMETA, false),
        ] {
            // A failed press still gets its releases tried.
            if sent.is_ok() || !pressed {
                if let Err(e) = input.send(InputCmd::Key { code, pressed }) {
                    sent = Err(format!("{e:#}"));
                }
            }
            std::thread::sleep(Duration::from_millis(60));
        }
        let _ = input.send(InputCmd::ReleaseAll);
        if let Err(e) = sent {
            return fail("switch", format!("Super+Space could not be sent: {e}"));
        }
        let mut after = in_use.active_layout();
        for _ in 0..30 {
            if after.is_some() && after != before {
                break;
            }
            std::thread::sleep(Duration::from_millis(100));
            after = in_use.active_layout();
        }
        std::thread::sleep(Duration::from_millis(300));
        let _ = take();
        if after.is_none() || after == before {
            return fail(
                "switch",
                format!(
                    "Super+Space was sent but the layout in use did not change \
                     ({before:?} -> {after:?}): one layout, the shortcut off, or the \
                     layout in use not read"
                ),
            );
        }
        ok("switch", format!("layout in use {before:?} -> {after:?}"));
    }
    // A first key proves the field has the keyboard.
    let _ = input.send(InputCmd::SetKeymap(layout_text("us").unwrap_or_default()));
    let _ = tap(&input, KEY_A);
    std::thread::sleep(Duration::from_millis(300));
    let first = take();
    if first != "a" {
        return fail(
            "focus",
            format!("the text field got {first:?} for a first a; it may not have the keyboard"),
        );
    }
    ok("focus", "the text field has the keyboard");

    // Layouts named on the command line limit the scenarios to those (a
    // server with several layouts cannot pass the ones for a layout it only
    // has as a second choice).
    let only: Vec<String> = std::env::args().skip(1).collect();
    let mut healthy = true;
    for s in SCENARIOS
        .iter()
        .filter(|s| only.is_empty() || only.iter().any(|l| l == s.layout))
    {
        let result = (|| -> Result<String, String> {
            input
                .send(InputCmd::SetKeymap(layout_text(s.layout)?))
                .map_err(|e| format!("{e:#}"))?;
            for step in s.steps {
                match *step {
                    Tap(code) => tap(&input, code)?,
                    Down(code) => press(&input, code, true)?,
                    Up(code) => press(&input, code, false)?,
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            std::thread::sleep(Duration::from_millis(300));
            Ok(take())
        })();
        let _ = input.send(InputCmd::ReleaseAll);
        if let (Ok(viewer), Some(server)) = (layout_text(s.layout), server_text.as_deref()) {
            let active = match in_use.active_layout() {
                Some((group, of)) => format!("layout {} of {} in use", group + 1, of),
                None => "layout in use not known".to_owned(),
            };
            println!(
                "       {} keys go as {:?} ({active}); {}",
                s.layout,
                input.key_mode(),
                describe(&viewer, server)
            );
        }
        match result {
            Ok(got) if got == s.expect => ok(s.what, format!("{got:?}")),
            Ok(got) if s.known_limit && got.is_empty() => ok(
                s.what,
                "nothing typed (known limit: the server's layout has no key for it)",
            ),
            Ok(got) => {
                healthy &= fail(
                    s.what,
                    format!("expected {:?}, the field got {got:?}", s.expect),
                )
            }
            Err(e) => healthy &= fail(s.what, e),
        }
    }
    drop(input);
    healthy
}

fn main() {
    // Before a window takes the screen: a name with no scenarios would
    // silently run fewer (or none) and could report PASS.
    if let Some(bad) = std::env::args()
        .skip(1)
        .find(|l| !SCENARIOS.iter().any(|s| s.layout == l.as_str()))
    {
        fail(
            "argument",
            format!("{bad:?} is not a layout with scenarios"),
        );
        println!("FAIL");
        std::process::exit(1);
    }
    if gtk4::init().is_err() {
        println!("[FAIL] gtk: cannot open a display");
        println!("FAIL");
        std::process::exit(1);
    }
    let entry = gtk4::Entry::new();
    let window = gtk4::Window::new();
    window.set_title(Some("gliff layout probe"));
    window.set_child(Some(&entry));
    window.fullscreen();
    window.present();

    let (requests, wanted) = channel::<Request>();
    let finished = Arc::new(AtomicBool::new(false));
    let healthy = Arc::new(AtomicBool::new(false));
    let worker = {
        let (finished, healthy) = (finished.clone(), healthy.clone());
        std::thread::spawn(move || {
            healthy.store(work(requests), Ordering::SeqCst);
            finished.store(true, Ordering::SeqCst);
        })
    };

    let main_loop = gtk4::glib::MainLoop::new(None, false);
    {
        let main_loop = main_loop.clone();
        gtk4::glib::timeout_add_local(Duration::from_millis(50), move || {
            while let Ok(Request::Take(reply)) = wanted.try_recv() {
                let text = entry.text().to_string();
                entry.set_text("");
                let _ = reply.send(text);
            }
            if finished.load(Ordering::SeqCst) {
                main_loop.quit();
                return gtk4::glib::ControlFlow::Break;
            }
            gtk4::glib::ControlFlow::Continue
        });
    }
    main_loop.run();
    let _ = worker.join();
    window.close();
    if healthy.load(Ordering::SeqCst) {
        println!("PASS");
    } else {
        println!("FAIL");
        std::process::exit(1);
    }
}
