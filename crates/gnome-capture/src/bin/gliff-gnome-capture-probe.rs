//! Capture a GNOME screen cast with `gnome-capture` and report what arrived:
//! frame size, whether the pixels are real, how fast frames come, whether a
//! recapture works and what the cursor does.
//!
//! One line per check, `[ OK ]` or `[FAIL]`, with the raw detail. With
//! `--png FILE` the first frame is saved so it can be looked at.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use gnome_capture::{CaptureEvent, Capturer, Options, Stats};
use gnome_ipc::{discover, mutter, CursorMode, Session, StreamSource};

struct Args {
    virtual_source: bool,
    connector: Option<String>,
    cursor: CursorMode,
    png: Option<String>,
    secs: u64,
}

fn parse() -> Result<Args, String> {
    let mut a = Args {
        virtual_source: false,
        connector: None,
        cursor: CursorMode::Metadata,
        png: None,
        secs: 5,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--virtual" => a.virtual_source = true,
            "--connector" => a.connector = it.next(),
            "--png" => a.png = it.next(),
            "--secs" => {
                a.secs = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .ok_or("--secs needs a number")?
            }
            "--cursor" => {
                a.cursor = match it.next().as_deref() {
                    Some("hidden") => CursorMode::Hidden,
                    Some("embedded") => CursorMode::Embedded,
                    Some("metadata") => CursorMode::Metadata,
                    _ => return Err("--cursor hidden|embedded|metadata".into()),
                }
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
}

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

fn save_png(path: &str, w: usize, h: usize, bgra: &[u8]) -> std::io::Result<()> {
    let mut rgba = bgra.to_vec();
    for p in rgba.chunks_exact_mut(4) {
        p.swap(0, 2);
        p[3] = 255;
    }
    let mut enc = png::Encoder::new(
        std::io::BufWriter::new(std::fs::File::create(path)?),
        w as u32,
        h as u32,
    );
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(&rgba)?;
    Ok(())
}

fn run(args: &Args) -> bool {
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
    let (source, expect) = if args.virtual_source {
        (StreamSource::Virtual, None)
    } else {
        let m = match &args.connector {
            Some(c) => monitors.iter().find(|m| &m.connector == c),
            None => monitors.iter().find(|m| m.primary).or(monitors.first()),
        };
        match m {
            Some(m) => (
                StreamSource::Monitor(m.connector.clone()),
                Some((m.width, m.height)),
            ),
            None => return fail("monitor", "none to record"),
        }
    };

    let mut ses = match Session::create(&session.bus_address) {
        Ok(s) => s,
        Err(e) => return fail("create session", format!("{e:#}")),
    };
    let stream = match ses.record(&source, args.cursor) {
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
    ok(
        "stream",
        format!("{source:?}, cursor {:?}, PipeWire node {node}", args.cursor),
    );

    let (tx, rx) = mpsc::channel();
    let cap = match Capturer::start(
        Options::new(node, &session.runtime_dir, "probe"),
        Box::new(move |ev| {
            let _ = tx.send((Instant::now(), ev));
        }),
    ) {
        Ok(c) => c,
        Err(e) => return fail("connect", format!("{e:#}")),
    };
    ok("connect", "PipeWire stream connected");

    let begin = Instant::now();
    let deadline = begin + Duration::from_secs(args.secs);
    let mut healthy = true;
    let (mut frames, mut shapes, mut positions) = (0u32, 0u32, 0u32);
    let mut first: Option<Duration> = None;
    let mut ready = None;
    let mut recaptured = false;
    let mut asked_recapture = false;
    let mut last_pos = None;
    let _ = cap.request_frame();
    while Instant::now() < deadline {
        let Ok((at, ev)) = rx.recv_timeout(Duration::from_millis(200)) else {
            // Nothing for a while: on a quiet screen Mutter sends nothing, so
            // ask for the last frame again once.
            if frames > 0 && !asked_recapture {
                asked_recapture = true;
                let _ = cap.recapture();
            }
            continue;
        };
        match ev {
            CaptureEvent::Ready {
                width,
                height,
                fourcc,
                ..
            } => {
                ready = Some((width, height));
                ok("ready", format!("{width}x{height} fourcc {fourcc:#x}"));
            }
            CaptureEvent::Frame(frame) => {
                frames += 1;
                if asked_recapture {
                    recaptured = true;
                }
                first.get_or_insert(at - begin);
                let img = match frame.read_bgra() {
                    Ok(i) => i,
                    Err(e) => return fail("read frame", format!("{e}")),
                };
                if frames == 1 {
                    let nonzero = img.pixels.iter().filter(|b| **b != 0).count();
                    let frac = nonzero as f64 / img.pixels.len() as f64;
                    let dims = format!("{}x{}", img.width, img.height);
                    if nonzero == 0 {
                        healthy &= fail("pixels", format!("{dims}, every byte is zero"));
                    } else {
                        ok(
                            "pixels",
                            format!("{dims}, {:.0}% of bytes non-zero", frac * 100.0),
                        );
                    }
                    if let Some((w, h)) = expect.or(ready) {
                        // read_bgra crops to even sizes.
                        let want = ((w & !1) as usize, (h & !1) as usize);
                        if (img.width, img.height) != want {
                            healthy &= fail(
                                "size",
                                format!("frame is {dims}, monitor is {}x{}", want.0, want.1),
                            );
                        } else {
                            ok("size", format!("matches the {}x{} monitor", want.0, want.1));
                        }
                    }
                    if let Some(path) = &args.png {
                        match save_png(path, img.width, img.height, &img.pixels) {
                            Ok(()) => ok("png", path),
                            Err(e) => healthy &= fail("png", format!("{e}")),
                        }
                    }
                }
                drop(frame);
                let _ = cap.request_frame();
            }
            CaptureEvent::CursorShape { width, height, .. } => {
                shapes += 1;
                if shapes == 1 {
                    ok("cursor shape", format!("{width}x{height}"));
                }
            }
            CaptureEvent::CursorPos { x, y, .. } => {
                positions += 1;
                last_pos = Some((x, y));
            }
            CaptureEvent::Error(e) => return fail("capture", e),
            CaptureEvent::Stopped => return fail("capture", "stopped early"),
        }
        if ses.is_closed() {
            return fail("session", "Mutter closed it");
        }
    }

    match first {
        Some(d) => ok("first frame", format!("after {d:.0?}")),
        None => {
            let s = cap.stats();
            return fail(
                "first frame",
                format!(
                    "none in {} s; PipeWire delivered {} buffers, {} without pixels, {} dropped, {} with cursor metadata",
                    args.secs,
                    Stats::get(&s.buffers),
                    Stats::get(&s.empty),
                    Stats::get(&s.dropped),
                    Stats::get(&s.cursor_meta)
                ),
            );
        }
    }
    let s = cap.stats();
    ok(
        "frames",
        format!(
            "{frames} frames in {} s ({:.1} fps); PipeWire delivered {} buffers, {} without pixels, {} dropped",
            args.secs,
            f64::from(frames) / args.secs as f64,
            Stats::get(&s.buffers),
            Stats::get(&s.empty),
            Stats::get(&s.dropped),
        ),
    );
    if asked_recapture {
        if recaptured {
            ok("recapture", "the last frame was sent again");
        } else {
            healthy &= fail("recapture", "no frame after asking for one");
        }
    } else {
        ok("recapture", "not exercised: frames kept arriving");
    }
    ok(
        "cursor",
        format!(
            "{} buffers carried cursor metadata; {shapes} shapes, {positions} positions, last {last_pos:?}",
            Stats::get(&s.cursor_meta)
        ),
    );
    drop(cap);
    if let Err(e) = ses.stop() {
        healthy &= fail("stop", format!("{e:#}"));
    }
    healthy
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!(
                "{e}\nusage: gliff-gnome-capture-probe [--virtual] [--connector NAME] \
                 [--cursor hidden|embedded|metadata] [--png FILE] [--secs N]"
            );
            std::process::exit(2);
        }
    };
    std::process::exit(if run(&args) { 0 } else { 1 });
}
