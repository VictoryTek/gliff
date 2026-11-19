//! Inject input into a GNOME session and check what can be checked from
//! outside: where the pointer lands (it is read back through the capture
//! stream's cursor metadata) and Caps Lock (read from the session).
//!
//! Actions run in order, each reported as `[ OK ]` or `[FAIL]`:
//!
//!   move:X,Y              absolute pointer move, verified through the cursor
//!   click[:left|right|middle]
//!   type:TEXT             letters, digits, space (US layout), sent as keys
//!   wheel:N               N wheel steps down (negative: up)
//!   paste | copy          press Ctrl+Shift+V / Ctrl+Shift+C (a terminal's paste and copy)
//!   key:tab|enter|esc|left|right   tap one named key
//!   caps                  tap Caps Lock twice and check it toggled and returned
//!   wait:MS
//!
//! Clicks, keys and wheel steps go to whatever window is under the pointer or
//! focused: aim them at a test window, never at someone's work. What a
//! window received is for the test window to report.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use gnome_capture::{CaptureEvent, Capturer, Options};
use gnome_input::{Axis, Input, InputCmd};
use gnome_ipc::{discover, mutter, CursorMode, Session, StreamSource};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;
const BTN_MIDDLE: u32 = 0x112;
const KEY_LEFTSHIFT: u32 = 42;
const KEY_CAPSLOCK: u32 = 58;
const KEY_LEFTCTRL: u32 = 29;
const KEY_V: u32 = 47;
const KEY_C: u32 = 46;

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

/// evdev keycode and whether it needs Shift, for a US layout.
fn keycode(c: char) -> Option<(u32, bool)> {
    const ROW: [(char, u32); 26] = [
        ('a', 30),
        ('b', 48),
        ('c', 46),
        ('d', 32),
        ('e', 18),
        ('f', 33),
        ('g', 34),
        ('h', 35),
        ('i', 23),
        ('j', 36),
        ('k', 37),
        ('l', 38),
        ('m', 50),
        ('n', 49),
        ('o', 24),
        ('p', 25),
        ('q', 16),
        ('r', 19),
        ('s', 31),
        ('t', 20),
        ('u', 22),
        ('v', 47),
        ('w', 17),
        ('x', 45),
        ('y', 21),
        ('z', 44),
    ];
    match c {
        'a'..='z' => ROW.iter().find(|(l, _)| *l == c).map(|(_, k)| (*k, false)),
        'A'..='Z' => ROW
            .iter()
            .find(|(l, _)| *l == c.to_ascii_lowercase())
            .map(|(_, k)| (*k, true)),
        '1'..='9' => Some((c as u32 - '1' as u32 + 2, false)),
        '0' => Some((11, false)),
        ' ' => Some((57, false)),
        _ => None,
    }
}

fn key(input: &Input, code: u32, pressed: bool) -> anyhow::Result<()> {
    input.send(InputCmd::Key { code, pressed })
}

fn tap(input: &Input, code: u32) -> anyhow::Result<()> {
    key(input, code, true)?;
    std::thread::sleep(Duration::from_millis(15));
    key(input, code, false)?;
    std::thread::sleep(Duration::from_millis(15));
    Ok(())
}

fn run(actions: &[String]) -> bool {
    let session = match discover::find() {
        Ok(s) => s,
        Err(e) => return fail("session", format!("{e:#}")),
    };
    let conn = match mutter::connect(&session.bus_address) {
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

    let ses = match Session::create(&session.bus_address) {
        Ok(s) => s,
        Err(e) => return fail("create session", format!("{e:#}")),
    };
    let stream = match ses.record(
        &StreamSource::Monitor(mon.connector.clone()),
        CursorMode::Metadata,
    ) {
        Ok(s) => s,
        Err(e) => return fail("record", format!("{e:#}")),
    };
    if let Err(e) = ses.start() {
        return fail("start", format!("{e:#}"));
    }
    let node = match stream.wait_node(Duration::from_secs(5)) {
        Ok(n) => n,
        Err(e) => return fail("stream", format!("{e:#}")),
    };
    let (tx, cursor_rx) = mpsc::channel();
    let _capture = match Capturer::start(
        Options::new(node, &session.runtime_dir, "input-probe"),
        Box::new(move |ev| {
            if let CaptureEvent::CursorPos { x, y, .. } = ev {
                let _ = tx.send((x, y));
            }
        }),
    ) {
        Ok(c) => c,
        Err(e) => return fail("capture", format!("{e:#}")),
    };
    let port = ses.input();
    let input = Input::new(port.clone(), stream.path(), extent);
    ok(
        "ready",
        format!(
            "{} {}x{} (pointer space {}x{})",
            mon.connector, mon.width, mon.height, extent.0, extent.1
        ),
    );

    let mut healthy = true;
    for action in actions {
        let (name, arg) = action.split_once(':').unwrap_or((action.as_str(), ""));
        let result: Result<(), String> = (|| match name {
            "move" => {
                let (x, y) = arg
                    .split_once(',')
                    .and_then(|(x, y)| Some((x.parse::<f64>().ok()?, y.parse::<f64>().ok()?)))
                    .ok_or("move:X,Y")?;
                while cursor_rx.try_recv().is_ok() {}
                input
                    .send(InputCmd::Motion { x, y })
                    .map_err(|e| format!("{e:#}"))?;
                let deadline = Instant::now() + Duration::from_millis(1500);
                let mut last = None;
                while Instant::now() < deadline {
                    if let Ok(p) = cursor_rx.recv_timeout(Duration::from_millis(100)) {
                        last = Some(p);
                        if (f64::from(p.0) - x).abs() <= 2.0 && (f64::from(p.1) - y).abs() <= 2.0 {
                            ok(
                                "move",
                                format!("sent {x},{y}; the cursor reports {},{}", p.0, p.1),
                            );
                            return Ok(());
                        }
                    }
                }
                Err(format!("sent {x},{y}; the cursor reported {last:?}"))
            }
            "click" => {
                let button = match arg {
                    "" | "left" => BTN_LEFT,
                    "right" => BTN_RIGHT,
                    "middle" => BTN_MIDDLE,
                    _ => return Err("click[:left|right|middle]".into()),
                };
                for pressed in [true, false] {
                    input
                        .send(InputCmd::Button { button, pressed })
                        .map_err(|e| format!("{e:#}"))?;
                    std::thread::sleep(Duration::from_millis(30));
                }
                ok("click", format!("button {button:#x} down and up"));
                Ok(())
            }
            "type" => {
                for c in arg.chars() {
                    let (code, shift) = keycode(c).ok_or(format!("no key for {c:?}"))?;
                    let run = || -> anyhow::Result<()> {
                        if shift {
                            key(&input, KEY_LEFTSHIFT, true)?;
                        }
                        tap(&input, code)?;
                        if shift {
                            key(&input, KEY_LEFTSHIFT, false)?;
                        }
                        Ok(())
                    };
                    run().map_err(|e| format!("{e:#}"))?;
                }
                ok("type", format!("{} keys", arg.chars().count()));
                Ok(())
            }
            "wheel" => {
                let steps: i32 = arg.parse().map_err(|_| "wheel:N".to_string())?;
                input
                    .send(InputCmd::Axis {
                        axis: Axis::Vertical,
                        value: f64::from(steps) * 10.0,
                        discrete: Some(steps),
                        stop: false,
                    })
                    .map_err(|e| format!("{e:#}"))?;
                ok("wheel", format!("{steps} steps"));
                Ok(())
            }
            "paste" | "copy" => {
                let letter = if name == "paste" { KEY_V } else { KEY_C };
                let chord = || -> anyhow::Result<()> {
                    key(&input, KEY_LEFTCTRL, true)?;
                    key(&input, KEY_LEFTSHIFT, true)?;
                    tap(&input, letter)?;
                    key(&input, KEY_LEFTSHIFT, false)?;
                    key(&input, KEY_LEFTCTRL, false)
                };
                chord().map_err(|e| format!("{e:#}"))?;
                ok(
                    name,
                    format!("Ctrl+Shift+{}", if name == "paste" { 'V' } else { 'C' }),
                );
                Ok(())
            }
            "key" => {
                let code = match arg {
                    "tab" => 15,
                    "enter" => 28,
                    "esc" => 1,
                    "left" => 105,
                    "right" => 106,
                    _ => return Err("key:tab|enter|esc|left|right".to_string()),
                };
                tap(&input, code).map_err(|e| format!("{e:#}"))?;
                ok("key", arg);
                Ok(())
            }
            "caps" => {
                let before = port.lock_state().map_err(|e| format!("{e:#}"))?.0;
                tap(&input, KEY_CAPSLOCK).map_err(|e| format!("{e:#}"))?;
                std::thread::sleep(Duration::from_millis(200));
                let during = port.lock_state().map_err(|e| format!("{e:#}"))?.0;
                tap(&input, KEY_CAPSLOCK).map_err(|e| format!("{e:#}"))?;
                std::thread::sleep(Duration::from_millis(200));
                let after = port.lock_state().map_err(|e| format!("{e:#}"))?.0;
                if during != before && after == before {
                    ok(
                        "caps",
                        format!("Caps Lock went {before} -> {during} -> {after}"),
                    );
                    Ok(())
                } else {
                    Err(format!("Caps Lock went {before} -> {during} -> {after}"))
                }
            }
            "wait" => {
                std::thread::sleep(Duration::from_millis(
                    arg.parse().map_err(|_| "wait:MS".to_string())?,
                ));
                Ok(())
            }
            other => Err(format!("unknown action {other:?}")),
        })();
        if let Err(e) = result {
            healthy &= fail(name, e);
        }
        if ses.is_closed() {
            return fail("session", "Mutter closed it");
        }
    }
    drop(input);
    healthy
}

fn main() {
    let actions: Vec<String> = std::env::args().skip(1).collect();
    if actions.is_empty() {
        eprintln!("usage: gliff-gnome-input-probe ACTION...  (move:X,Y click[:left|right|middle] type:TEXT key:NAME wheel:N paste copy caps wait:MS)");
        std::process::exit(2);
    }
    std::process::exit(if run(&actions) { 0 } else { 1 });
}
