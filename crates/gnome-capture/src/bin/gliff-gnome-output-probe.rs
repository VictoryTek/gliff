//! Check `VirtualOutput`: create a virtual monitor, resize it, remove it, and
//! check the machine is kept awake while it exists.
//!
//! One line per check, `[ OK ]` or `[FAIL]`, with the raw detail.
//!
//!   gliff-gnome-output-probe            create, resize, two at once, remove
//!   gliff-gnome-output-probe scales     resize through sizes and scales and report
//!                                       what Mutter chose
//!   gliff-gnome-output-probe hold SECS  keep one output for SECS seconds and
//!                                       report the screen state every 10 s
//!   gliff-gnome-output-probe idle SECS  no output: report the screen state
//!                                       every 10 s (the control for `hold`)

use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};

use gnome_capture::{CaptureEvent, VirtualOutput};
use gnome_ipc::{discover, mutter, power, CursorMode, GraphicalSession};

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

fn create(
    desktop: &GraphicalSession,
    size: (u32, u32),
) -> anyhow::Result<(VirtualOutput, Receiver<CaptureEvent>)> {
    create_at(desktop, size, 1.0)
}

fn create_at(
    desktop: &GraphicalSession,
    size: (u32, u32),
    scale: f64,
) -> anyhow::Result<(VirtualOutput, Receiver<CaptureEvent>)> {
    let (tx, rx) = mpsc::channel();
    let out = VirtualOutput::create(
        desktop,
        size,
        scale,
        CursorMode::Metadata,
        Vec::new(),
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    Ok((out, rx))
}

/// The size of the next frame, after `Ready` events for `want`.
fn frame_size(
    out: &VirtualOutput,
    rx: &Receiver<CaptureEvent>,
    want: (u32, u32),
) -> Result<(u32, u32), String> {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = None;
    let _ = out.capturer().request_frame();
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(CaptureEvent::Ready { width, height, .. }) => {
                ready = Some((width, height));
                let _ = out.capturer().request_frame();
            }
            Ok(CaptureEvent::Frame(f)) => {
                let img = f.read_bgra().map_err(|e| e.to_string())?;
                let nonzero = img.pixels.iter().any(|b| *b != 0);
                // read_bgra crops to even sizes.
                let got = (img.width as u32, img.height as u32);
                // A request for the size it already has brings no new `Ready`.
                if ready.is_none_or(|r| r == want) && got == (want.0 & !1, want.1 & !1) {
                    return if nonzero {
                        Ok(got)
                    } else {
                        Err("frame is all zero".into())
                    };
                }
                drop(f);
                let _ = out.capturer().request_frame();
            }
            Ok(CaptureEvent::Error(e)) => return Err(e),
            Ok(_) => {}
            Err(_) => {
                let _ = out.capturer().recapture();
            }
        }
    }
    Err(format!("no {want:?} frame in 5 s (last Ready {ready:?})"))
}

fn run(desktop: &GraphicalSession) -> bool {
    let conn = match mutter::connect(&desktop.bus_address) {
        Ok(c) => c,
        Err(e) => return fail("session bus", format!("{e:#}")),
    };
    let mut healthy = true;

    // The screen blanks when idle and Mutter then refuses sessions.
    let blanked = power::blank(&conn).and_then(|()| {
        // The shield takes a moment to come up.
        for _ in 0..40 {
            if power::is_blanked(&conn)? {
                return Ok(true);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        Ok(false)
    });
    match blanked {
        Ok(true) => ok(
            "blank",
            "the screen is blanked before the output is created",
        ),
        Ok(false) => healthy &= fail("blank", "could not blank the screen to test the wake"),
        Err(e) => return fail("blank", format!("{e:#}")),
    }
    let first = (1280, 720);
    let (out, rx) = match create(desktop, first) {
        Ok(v) => v,
        Err(e) => return fail("create", format!("{e:#}")),
    };
    ok("create", format!("{} at {first:?}", out.connector()));
    match power::is_blanked(&conn) {
        Ok(false) => ok("wake", "the screen is awake"),
        Ok(true) => healthy &= fail("wake", "still blanked"),
        Err(e) => healthy &= fail("wake", format!("{e:#}")),
    }
    match power::idle_inhibited(&conn) {
        Ok(true) => ok("inhibit", "idle is inhibited while the output exists"),
        Ok(false) => healthy &= fail("inhibit", "idle is not inhibited"),
        Err(e) => healthy &= fail("inhibit", format!("{e:#}")),
    }
    match out.monitor() {
        Ok(m) if (m.width, m.height) == first => ok(
            "monitor",
            format!(
                "{} {}x{} @{:.0} Hz scale {}",
                m.connector, m.width, m.height, m.refresh_hz, m.scale
            ),
        ),
        Ok(m) => {
            healthy &= fail(
                "monitor",
                format!("{}x{}, wanted {first:?}", m.width, m.height),
            )
        }
        Err(e) => healthy &= fail("monitor", format!("{e:#}")),
    }
    match frame_size(&out, &rx, first) {
        Ok(s) => ok("frame", format!("{}x{} with pixels", s.0, s.1)),
        Err(e) => healthy &= fail("frame", e),
    }

    for size in [
        (1600, 900),
        (1024, 768),
        (1366, 768),
        (1920, 1080),
        (1280, 720),
    ] {
        let t = Instant::now();
        match out.resize(size.0, size.1, 1.0) {
            Ok(()) => ok(
                "resize",
                format!("{size:?} reported after {:.0?}", t.elapsed()),
            ),
            Err(e) => {
                healthy &= fail("resize", format!("{size:?}: {e:#}"));
                continue;
            }
        }
        match frame_size(&out, &rx, size) {
            Ok(s) => ok("resize frame", format!("{}x{} with pixels", s.0, s.1)),
            Err(e) => healthy &= fail("resize frame", format!("{size:?}: {e}")),
        }
    }

    // A second output next to the first, resized on its own.
    match create(desktop, (1024, 768)) {
        Ok((second, rx2)) => {
            if second.connector() == out.connector() {
                healthy &= fail("second output", "same connector as the first");
            } else {
                ok(
                    "second output",
                    format!("{} beside {}", second.connector(), out.connector()),
                );
            }
            let a = second.resize(800, 600, 1.0);
            let b = out.monitor().map(|m| (m.width, m.height));
            match (a, b) {
                (Ok(()), Ok((1280, 720))) => ok("second resize", "the first output kept its size"),
                (a, b) => healthy &= fail("second resize", format!("{a:?} / first is {b:?}")),
            }
            if let Err(e) = frame_size(&second, &rx2, (800, 600)) {
                healthy &= fail("second frame", e);
            }
            match second.close() {
                Ok(()) => ok("second close", "its monitor is gone"),
                Err(e) => healthy &= fail("second close", format!("{e:#}")),
            }
            match out.monitor() {
                Ok(_) => ok("first survives", "the first output is still listed"),
                Err(e) => healthy &= fail("first survives", format!("{e:#}")),
            }
        }
        Err(e) => healthy &= fail("second output", format!("{e:#}")),
    }

    let connector = out.connector().to_string();
    match out.close() {
        Ok(()) => ok("close", format!("{connector} is gone")),
        Err(e) => healthy &= fail("close", format!("{e:#}")),
    }
    // Mutter takes a moment to drop the inhibition's owner? No: it is ours.
    std::thread::sleep(Duration::from_millis(300));
    match power::idle_inhibited(&conn) {
        Ok(false) => ok("release", "idle is no longer inhibited"),
        Ok(true) => healthy &= fail("release", "idle is still inhibited"),
        Err(e) => healthy &= fail("release", format!("{e:#}")),
    }
    match mutter::monitors(&conn) {
        Ok(m) => ok(
            "monitors",
            format!(
                "{} left: {:?}",
                m.len(),
                m.iter().map(|m| &m.connector).collect::<Vec<_>>()
            ),
        ),
        Err(e) => healthy &= fail("monitors", format!("{e:#}")),
    }
    healthy
}

/// A new output for each size and scale, to see which scales Mutter gives.
fn fresh(desktop: &GraphicalSession) -> bool {
    let mut healthy = true;
    for size in [(1920, 1080), (2560, 1440), (3840, 2160)] {
        for scale in [1.0, 1.25, 1.5, 1.75, 2.0, 3.0] {
            match create_at(desktop, size, scale) {
                Ok((out, _rx)) => {
                    match out.monitor() {
                        Ok(m) => ok(
                            "fresh",
                            format!(
                                "{size:?} asked {scale}: reports {}x{} at {}",
                                m.width, m.height, m.scale
                            ),
                        ),
                        Err(e) => healthy &= fail("fresh", format!("{e:#}")),
                    }
                    let _ = out.close();
                }
                Err(e) => healthy &= fail("fresh", format!("{size:?} at {scale}: {e:#}")),
            }
        }
    }
    healthy
}

/// Ask for sizes and scales and say what Mutter reports and delivers.
fn scales(desktop: &GraphicalSession) -> bool {
    let (out, rx) = match create(desktop, (1280, 720)) {
        Ok(v) => v,
        Err(e) => return fail("create", format!("{e:#}")),
    };
    let mut healthy = true;
    let steps = [
        ((1280, 720), 2.0),
        ((1280, 720), 1.0),
        ((1920, 1080), 1.5),
        ((1920, 1080), 1.25),
        ((2560, 1440), 2.0),
        ((1920, 1080), 1.0),
    ];
    for (size, scale) in steps {
        match out.resize(size.0, size.1, scale) {
            Ok(()) => match out.monitor() {
                Ok(m) => ok(
                    "scale",
                    format!(
                        "asked {size:?} at {scale}: Mutter reports {}x{} at {} (logical {:.0}x{:.0})",
                        m.width,
                        m.height,
                        m.scale,
                        f64::from(m.width) / m.scale,
                        f64::from(m.height) / m.scale
                    ),
                ),
                Err(e) => healthy &= fail("scale", format!("{e:#}")),
            },
            Err(e) => healthy &= fail("scale", format!("asked {size:?} at {scale}: {e:#}")),
        }
        match frame_size(&out, &rx, size) {
            Ok(s) => ok("scale frame", format!("{}x{} with pixels", s.0, s.1)),
            Err(e) => healthy &= fail("scale frame", e),
        }
    }
    healthy
}

/// Go back to a size the monitor had at another scale, and say what Mutter
/// reports at once and two seconds later (the server's soak found it giving
/// the old scale back).
fn revisit(desktop: &GraphicalSession) -> bool {
    let (out, rx) = match create(desktop, (1280, 720)) {
        Ok(v) => v,
        Err(e) => return fail("create", format!("{e:#}")),
    };
    let mut healthy = true;
    // Every step changes the size, so the scale asked for is applied.
    let steps = [
        ((1920, 1080), 1.5),
        ((1280, 720), 1.0),
        ((1920, 1080), 1.0),
        ((1280, 720), 1.0),
        ((1920, 1080), 2.0),
        ((1280, 720), 1.0),
        ((1920, 1080), 1.0),
        ((2560, 1440), 2.0),
        ((1920, 1080), 1.0),
    ];
    for (size, scale) in steps {
        if let Err(e) = out.resize(size.0, size.1, scale) {
            healthy &= fail("revisit", format!("asked {size:?} at {scale}: {e:#}"));
            continue;
        }
        let at = |out: &VirtualOutput| out.monitor().map(|m| m.scale).unwrap_or(f64::NAN);
        let now = at(&out);
        std::thread::sleep(Duration::from_secs(2));
        let later = at(&out);
        let line = format!("asked {size:?} at {scale}: reports {now} at once, {later} 2 s later");
        if (now - scale).abs() < 0.01 && (later - scale).abs() < 0.01 {
            ok("revisit", line);
        } else {
            healthy &= fail("revisit", line);
        }
        if let Err(e) = frame_size(&out, &rx, size) {
            healthy &= fail("revisit frame", e);
        }
    }
    healthy
}

/// Report the screen state every 10 s for `secs`, with or without an output.
fn watch(desktop: &GraphicalSession, secs: u64, hold: bool) -> bool {
    let conn = match mutter::connect(&desktop.bus_address) {
        Ok(c) => c,
        Err(e) => return fail("session bus", format!("{e:#}")),
    };
    let held = if hold {
        match create(desktop, (1280, 720)) {
            Ok(v) => {
                ok("create", v.0.connector());
                Some(v)
            }
            Err(e) => return fail("create", format!("{e:#}")),
        }
    } else {
        None
    };
    let begin = Instant::now();
    let mut healthy = true;
    while begin.elapsed() < Duration::from_secs(secs) {
        std::thread::sleep(Duration::from_secs(10));
        let blanked = power::is_blanked(&conn);
        let gone = held.as_ref().map(|(o, _)| o.monitor().is_err());
        println!(
            "  t={:>3}s blanked={blanked:?} monitor_gone={gone:?} session_closed={:?}",
            begin.elapsed().as_secs(),
            held.as_ref().map(|(o, _)| o.session().is_closed())
        );
        if hold && (matches!(blanked, Ok(true)) || gone == Some(true)) {
            healthy = false;
        }
    }
    if let Some((o, rx)) = &held {
        match frame_size(o, rx, (1280, 720)) {
            Ok(s) => ok("frame", format!("{}x{} after {secs} s", s.0, s.1)),
            Err(e) => healthy &= fail("frame", e),
        }
    }
    if hold {
        if healthy {
            ok("hold", "never blanked, monitor and session survived");
        } else {
            fail("hold", "the screen blanked or the output went away");
        }
    }
    healthy
}

fn main() {
    let desktop = match discover::find() {
        Ok(s) => s,
        Err(e) => {
            println!("[FAIL] session: {e:#}");
            std::process::exit(1);
        }
    };
    let args: Vec<String> = std::env::args().skip(1).collect();
    let healthy = match args.first().map(String::as_str) {
        None => run(&desktop),
        Some("scales") => scales(&desktop),
        Some("fresh") => fresh(&desktop),
        Some("revisit") => revisit(&desktop),
        Some(mode @ ("hold" | "idle")) => {
            let secs = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(60);
            watch(&desktop, secs, mode == "hold")
        }
        Some(_) => {
            eprintln!("usage: gliff-gnome-output-probe [hold|idle SECS]");
            std::process::exit(2);
        }
    };
    std::process::exit(if healthy { 0 } else { 1 });
}
