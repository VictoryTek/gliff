//! Check the GNOME side of a host from a plain login: find the desktop
//! session, reach Mutter, list monitors and open a capture session.
//!
//! One line per check, `[ OK ]` or `[FAIL]`, with the raw detail. It reports
//! only what it saw; it cannot tell whether frames are correct (that is the
//! capture step's probe).

use std::time::Duration;

use gnome_ipc::{discover, mutter, Session, StreamSource};

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

fn run(virtual_source: bool) -> bool {
    let session = match discover::find() {
        Ok(s) => s,
        Err(e) => return fail("session", format!("{e:#}")),
    };
    ok(
        "session",
        format!(
            "id={} uid={} runtime_dir={} bus={}",
            session.id,
            session.uid,
            session.runtime_dir.display(),
            session.bus_address
        ),
    );

    let conn = match mutter::connect(&session.bus_address) {
        Ok(c) => c,
        Err(e) => return fail("session bus", format!("{e:#}")),
    };
    ok("session bus", "connected");

    match mutter::versions(&conn) {
        Ok((rd, sc)) => ok("mutter", format!("RemoteDesktop v{rd}, ScreenCast v{sc}")),
        Err(e) => return fail("mutter", format!("{e:#}")),
    }

    let monitors = match mutter::monitors(&conn) {
        Ok(m) => m,
        Err(e) => return fail("monitors", format!("{e:#}")),
    };
    for m in &monitors {
        ok(
            "monitor",
            format!(
                "{} {}x{}@{:.2} scale {} at {},{}{}",
                m.connector,
                m.width,
                m.height,
                m.refresh_hz,
                m.scale,
                m.x,
                m.y,
                if m.primary { " (primary)" } else { "" }
            ),
        );
    }

    let source = if virtual_source {
        StreamSource::Virtual
    } else {
        match monitors.iter().find(|m| m.primary).or(monitors.first()) {
            Some(m) => StreamSource::Monitor(m.connector.clone()),
            None => return fail("capture session", "no monitor to record"),
        }
    };
    let mut s = match Session::create(&session.bus_address) {
        Ok(s) => s,
        Err(e) => return fail("create session", format!("{e:#}")),
    };
    ok("create session", format!("linked sessions, id {}", s.id()));
    let stream = match s.record(&source) {
        Ok(st) => st,
        Err(e) => return fail("record", format!("{e:#}")),
    };
    ok("record", format!("{source:?} -> {}", stream.path()));
    if let Err(e) = s.start() {
        return fail("start", format!("{e:#}"));
    }
    ok("start", "no prompt reached the caller");
    let result = match stream.wait_node(Duration::from_secs(5)) {
        Ok(node) => {
            ok("stream", format!("PipeWire node {node}"));
            true
        }
        Err(e) => fail("stream", format!("{e:#}")),
    };
    if s.is_closed() {
        return fail("session", "Mutter closed it");
    }
    if let Err(e) = s.stop() {
        return fail("stop", format!("{e:#}"));
    }
    ok("stop", "session ended");
    result
}

fn main() {
    let mut virtual_source = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--virtual" => virtual_source = true,
            _ => {
                eprintln!("usage: gliff-gnome-probe [--virtual]");
                std::process::exit(2);
            }
        }
    }
    std::process::exit(if run(virtual_source) { 0 } else { 1 });
}
