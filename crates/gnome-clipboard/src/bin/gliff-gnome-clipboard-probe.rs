//! Check the GNOME clipboard bridge from a plain login.
//!
//!   roundtrip           the bridge against two plain Mutter clipboard clients
//!                       that play other applications: each direction, a large
//!                       item, no echo (not even of a republished copy) and a
//!                       withdraw
//!   serve TEXT SECS     offer TEXT as plain text for SECS seconds and report
//!                       every paste; paste into a test window to try it
//!   watch SECS          report what the desktop's selection offers and read it
//!
//! One line per check, `[ OK ]` or `[FAIL]`. `roundtrip` replaces the
//! desktop's selection: run it where that is fine.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gnome_clipboard::{into_blocking, Clipboard, ClipboardEvent, ClipboardSink, OWN_SOURCE_MIME};
use gnome_ipc::{discover, ClipboardPort, ClipboardSignal, Session};

const TEXT: &str = "text/plain;charset=utf-8";

enum Ev {
    Selection(Vec<String>),
    Paste(String),
}

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

/// A sink that reports events and answers every paste with `payload`.
fn sink(payload: Vec<u8>, tx: Sender<Ev>) -> ClipboardSink {
    Box::new(move |ev| match ev {
        ClipboardEvent::Selection { mime_types } => {
            let _ = tx.send(Ev::Selection(mime_types));
        }
        ClipboardEvent::Paste { mime_type, fd } => {
            let payload = payload.clone();
            std::thread::spawn(move || {
                let _ = std::fs::File::from(fd).write_all(&payload);
            });
            let _ = tx.send(Ev::Paste(mime_type));
        }
    })
}

/// Selection reports until one offers something (all of them, the last
/// decides), or `within` runs out.
fn selections(rx: &Receiver<Ev>, within: Duration) -> Vec<Vec<String>> {
    let end = Instant::now() + within;
    let mut seen = Vec::new();
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Ev::Selection(m)) => {
                let offers = !m.is_empty();
                seen.push(m);
                if offers {
                    break;
                }
            }
            Ok(Ev::Paste(_)) => {}
            Err(_) => break,
        }
    }
    seen
}

fn read_all(fd: OwnedFd) -> Result<Vec<u8>, String> {
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut out = Vec::new();
        let r = std::fs::File::from(fd).read_to_end(&mut out);
        let _ = tx.send(r.map(|_| out).map_err(|e| e.to_string()));
    });
    rx.recv_timeout(Duration::from_secs(5))
        .map_err(|_| "no EOF within 5 s".to_string())?
}

/// A plain Mutter clipboard client, with none of the bridge's rules: it plays
/// another application (or a clipboard manager).
struct Raw {
    _session: Session,
    port: ClipboardPort,
    selections: Receiver<Vec<String>>,
    payload: Arc<Mutex<Vec<u8>>>,
}

impl Raw {
    fn start(address: &str, payload: &[u8]) -> Result<Raw, String> {
        let e = |e: anyhow::Error| format!("{e:#}");
        let session = Session::create_remote_only(address).map_err(e)?;
        let (port, signals) = session.enable_clipboard().map_err(e)?;
        session.start().map_err(e)?;
        let payload = Arc::new(Mutex::new(payload.to_vec()));
        let (tx, selections) = mpsc::channel();
        {
            let (port, payload) = (port.clone(), payload.clone());
            std::thread::spawn(move || {
                while let Ok(signal) = signals.recv() {
                    match signal {
                        ClipboardSignal::OwnerChanged {
                            mime_types,
                            is_owner: false,
                        } => {
                            let _ = tx.send(mime_types);
                        }
                        ClipboardSignal::OwnerChanged { .. } => {}
                        ClipboardSignal::Transfer { serial, .. } => {
                            let data = payload.lock().unwrap().clone();
                            let done = port
                                .selection_write(serial)
                                .and_then(into_blocking)
                                .map(|fd| std::fs::File::from(fd).write_all(&data).is_ok())
                                .unwrap_or(false);
                            let _ = port.selection_write_done(serial, done);
                        }
                    }
                }
            });
        }
        Ok(Raw {
            _session: session,
            port,
            selections,
            payload,
        })
    }

    fn offer(&self, types: &[&str]) {
        let types: Vec<String> = types.iter().map(|t| t.to_string()).collect();
        let _ = self.port.set_selection(&types);
    }

    fn set_payload(&self, data: &[u8]) {
        *self.payload.lock().unwrap() = data.to_vec();
    }

    /// Selection reports within `within`.
    fn reports(&self, within: Duration) -> Vec<Vec<String>> {
        let end = Instant::now() + within;
        let mut seen = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self.selections.recv_timeout(left) {
                Ok(m) => {
                    let offers = !m.is_empty();
                    seen.push(m);
                    if offers {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        seen
    }

    /// Reports within `within`, including the ones after an empty report.
    fn reports_while_empty(&self, within: Duration) -> Vec<Vec<String>> {
        let end = Instant::now() + within;
        let mut seen = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            match self
                .selections
                .recv_timeout(left.min(Duration::from_millis(400)))
            {
                Ok(m) => seen.push(m),
                Err(_) if !seen.is_empty() => break,
                Err(_) => {}
            }
        }
        seen
    }

    fn read(&self, mime: &str) -> Result<Vec<u8>, String> {
        let fd = self
            .port
            .selection_read(mime)
            .and_then(into_blocking)
            .map_err(|e| format!("{e:#}"))?;
        read_all(fd)
    }
}

fn roundtrip(address: &str) -> bool {
    let (tx, rx) = mpsc::channel();
    let a = match Clipboard::start(address, sink(b"from-A".to_vec(), tx)) {
        Ok(c) => c,
        Err(e) => return fail("start the bridge", format!("{e:#}")),
    };
    let b = match Raw::start(address, b"from-B") {
        Ok(c) => c,
        Err(e) => return fail("start a client", e),
    };
    ok("start", "the bridge and one other client");
    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
    while b.selections.try_recv().is_ok() {}
    let mut healthy = true;
    let offers_text =
        |seen: &Vec<Vec<String>>| seen.last().is_some_and(|m| m.iter().any(|t| t == TEXT));

    // Another application copies; the bridge sees it and reads it.
    b.offer(&[TEXT]);
    let seen = selections(&rx, Duration::from_secs(3));
    if offers_text(&seen) {
        ok("bridge sees a copy", format!("reports: {seen:?}"));
    } else {
        healthy &= fail("bridge sees a copy", format!("reports: {seen:?}"));
    }
    match a
        .receive(TEXT.to_string())
        .map_err(|e| format!("{e:#}"))
        .and_then(read_all)
    {
        Ok(d) if d == b"from-B" => ok("bridge reads it", "from-B"),
        other => healthy &= fail("bridge reads it", format!("{other:?}")),
    }

    // The remote copies; another application sees it and pastes it.
    a.offer(vec![TEXT.to_string()]);
    let seen = b.reports(Duration::from_secs(3));
    if offers_text(&seen) {
        ok(
            "application sees the remote's copy",
            format!("reports: {seen:?}"),
        );
    } else {
        healthy &= fail(
            "application sees the remote's copy",
            format!("reports: {seen:?}"),
        );
    }
    match b.read(TEXT) {
        Ok(d) if d == b"from-A" => ok("application pastes it", "from-A"),
        other => healthy &= fail("application pastes it", format!("{other:?}")),
    }

    // A big item goes through intact in both directions.
    let big: Vec<u8> = (0..2_000_000u32).map(|i| (i % 251) as u8).collect();
    b.set_payload(&big);
    b.offer(&[TEXT]);
    let _ = selections(&rx, Duration::from_secs(3));
    match a
        .receive(TEXT.to_string())
        .map_err(|e| format!("{e:#}"))
        .and_then(read_all)
    {
        Ok(d) if d == big => ok("2 MB copied in", "arrived intact"),
        Ok(d) => {
            healthy &= fail(
                "2 MB copied in",
                format!("{} bytes, not the 2000000 sent", d.len()),
            )
        }
        Err(e) => healthy &= fail("2 MB copied in", e),
    }
    let (btx, brx) = mpsc::channel();
    let big_a = Clipboard::start(address, sink(big.clone(), btx));
    drop(brx);
    match big_a {
        Ok(big_a) => {
            big_a.offer(vec![TEXT.to_string()]);
            let _ = b.reports(Duration::from_secs(3));
            match b.read(TEXT) {
                Ok(d) if d == big => ok("2 MB pasted out", "arrived intact"),
                Ok(d) => {
                    healthy &= fail(
                        "2 MB pasted out",
                        format!("{} bytes, not the 2000000 sent", d.len()),
                    )
                }
                Err(e) => healthy &= fail("2 MB pasted out", e),
            }
        }
        Err(e) => healthy &= fail("2 MB pasted out", format!("{e:#}")),
    }

    // No echo: not of our own offer, and not of a copy a clipboard manager
    // republishes with our marker. (Let go of an owner first: Mutter then hands
    // the selection back to whoever held it before, which is reported late.)
    std::thread::sleep(Duration::from_millis(1500));
    while rx.try_recv().is_ok() {}
    a.offer(vec![TEXT.to_string()]);
    let seen = selections(&rx, Duration::from_millis(800));
    if seen.is_empty() {
        ok("no echo", "the bridge was not told about its own selection");
    } else {
        healthy &= fail("no echo", format!("the bridge was told {seen:?}"));
    }
    b.offer(&[TEXT, OWN_SOURCE_MIME]);
    let seen = selections(&rx, Duration::from_millis(800));
    if seen.is_empty() {
        ok("no echo of a republished copy", "the marker kept it out");
    } else {
        healthy &= fail(
            "no echo of a republished copy",
            format!("the bridge was told {seen:?}"),
        );
    }

    // A withdraw reaches others; one we no longer hold leaves the rest alone.
    a.offer(vec![TEXT.to_string()]);
    let c = match Raw::start(address, b"from-C") {
        Ok(c) => c,
        Err(e) => return fail("start a second client", e),
    };
    std::thread::sleep(Duration::from_millis(300));
    while c.selections.try_recv().is_ok() {}
    a.offer(vec![]);
    // Mutter reports the selection empty, then whatever was there before we
    // took it comes back; the first report is the withdraw.
    let seen = c.reports_while_empty(Duration::from_secs(2));
    if seen.first().is_some_and(|m| m.is_empty()) {
        ok(
            "withdraw seen",
            format!("another client was told: {seen:?}"),
        );
    } else {
        healthy &= fail("withdraw seen", format!("another client saw {seen:?}"));
    }
    b.offer(&[TEXT]);
    let _ = c.reports(Duration::from_secs(2));
    while c.selections.try_recv().is_ok() {}
    a.offer(vec![]);
    let seen = c.reports(Duration::from_millis(800));
    if seen.is_empty() {
        ok(
            "withdraw without holding",
            "left someone else's selection alone",
        );
    } else {
        healthy &= fail("withdraw without holding", format!("a client saw {seen:?}"));
    }

    // After a withdraw the bridge must still hear about copies (a bad withdraw
    // once switched the notifications off for good).
    std::thread::sleep(Duration::from_millis(500));
    while rx.try_recv().is_ok() {}
    b.offer(&[TEXT]);
    let seen = selections(&rx, Duration::from_secs(3));
    if offers_text(&seen) {
        ok(
            "bridge hears copies after a withdraw",
            format!("reports: {seen:?}"),
        );
    } else {
        healthy &= fail(
            "bridge hears copies after a withdraw",
            format!("reports: {seen:?}"),
        );
    }

    // An offer straight followed by a withdraw leaves nothing of ours behind.
    while c.selections.try_recv().is_ok() {}
    a.offer(vec![TEXT.to_string()]);
    a.offer(vec![]);
    let seen = c.reports_while_empty(Duration::from_secs(2));
    if seen.iter().any(|m| m.is_empty())
        && !seen
            .last()
            .is_some_and(|m| m.iter().any(|t| t == OWN_SOURCE_MIME))
    {
        ok(
            "offer then withdraw",
            format!("another client was told: {seen:?}"),
        );
    } else {
        healthy &= fail(
            "offer then withdraw",
            format!("another client was told: {seen:?}"),
        );
    }
    healthy
}

fn serve(address: &str, text: &str, secs: u64) -> bool {
    let (tx, rx) = mpsc::channel();
    let c = match Clipboard::start(address, sink(text.as_bytes().to_vec(), tx)) {
        Ok(c) => c,
        Err(e) => return fail("start", format!("{e:#}")),
    };
    c.offer(vec![
        TEXT.to_string(),
        "text/plain".to_string(),
        "UTF8_STRING".to_string(),
    ]);
    ok("offer", format!("{text:?} for {secs} s"));
    let end = Instant::now() + Duration::from_secs(secs);
    let mut pastes = 0;
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        match rx.recv_timeout(left) {
            Ok(Ev::Paste(mime)) => {
                pastes += 1;
                ok("paste", format!("asked for {mime}"));
            }
            Ok(Ev::Selection(m)) => ok("selection changed", format!("{m:?}")),
            Err(_) => break,
        }
    }
    ok("done", format!("{pastes} pastes"));
    true
}

fn watch(address: &str, secs: u64) -> bool {
    let (tx, rx) = mpsc::channel();
    let c = match Clipboard::start(address, sink(Vec::new(), tx)) {
        Ok(c) => c,
        Err(e) => return fail("start", format!("{e:#}")),
    };
    let end = Instant::now() + Duration::from_secs(secs);
    while let Some(left) = end.checked_duration_since(Instant::now()) {
        if let Ok(Ev::Selection(m)) = rx.recv_timeout(left) {
            ok("selection", format!("{m:?}"));
            if m.iter().any(|t| t == TEXT) {
                match c
                    .receive(TEXT.to_string())
                    .map_err(|e| format!("{e:#}"))
                    .and_then(read_all)
                {
                    Ok(d) => ok(
                        "read",
                        format!("{:?}", String::from_utf8_lossy(&d[..d.len().min(200)])),
                    ),
                    Err(e) => return fail("read", e),
                }
            }
        }
    }
    true
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let address = match discover::find() {
        Ok(s) => s.bus_address,
        Err(e) => {
            fail("session", format!("{e:#}"));
            std::process::exit(1);
        }
    };
    let healthy = match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["roundtrip"] => roundtrip(&address),
        ["serve", text, secs] => serve(&address, text, secs.parse().unwrap_or(10)),
        ["watch", secs] => watch(&address, secs.parse().unwrap_or(10)),
        _ => {
            eprintln!(
                "usage: gliff-gnome-clipboard-probe roundtrip | serve TEXT SECS | watch SECS"
            );
            std::process::exit(2);
        }
    };
    std::process::exit(if healthy { 0 } else { 1 });
}
