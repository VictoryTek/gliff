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

    let mut healthy = true;
    for s in SCENARIOS {
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
