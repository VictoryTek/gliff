//! A headless gliff client that drives a real `gliff-server` on this GNOME
//! desktop, to check the whole path: backend choice, output, capture, input
//! and cleanup. Run it on the GNOME machine itself.
//!
//!   serve_probe /path/to/gliff-server [--headless] [--clipboard SECS] [server args...]
//!
//! With `--clipboard SECS` the client offers a text selection and waits SECS
//! seconds, reporting every selection the server sends meanwhile: run
//! `gliff-gnome-clipboard-probe watch` or `serve` beside it to see both
//! directions.
//!
//! One line per check, `[ OK ]` or `[FAIL]`. It does not decode video: it
//! checks that frames of the announced size arrive and are acknowledged.

use std::process::Stdio;
use std::time::{Duration, Instant};

use gliff_proto::{
    features, ChromaMode, ClientCaps, ClientMsg, Codec, ServerMsg, PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::time::timeout;

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

#[derive(Default)]
struct Seen {
    frames: u64,
    last_frame_id: u64,
    stream: Option<(u32, u32)>,
    /// The stream's scale x1000 from the last `StreamConfig`.
    scale_milli: u32,
    /// `StreamConfig`s seen, and the frames seen when the last one came, so
    /// a step can wait for its own config and a frame after it.
    configs: u32,
    frames_at_config: u64,
    pipeline: Option<gliff_proto::VideoPipeline>,
    cursor_shapes: u32,
    cursor: Option<(f64, f64, bool)>,
    /// Selections the server reported, by mime types.
    offers: Vec<Vec<String>>,
}

struct Client {
    rd: Framed<ChildStdout>,
    wr: Framed<ChildStdin>,
    seen: Seen,
}

impl Client {
    /// Read one message (and its payload), acknowledging video frames.
    async fn step(&mut self, wait: Duration) -> Result<Option<ServerMsg>, String> {
        let msg = match timeout(wait, self.rd.read_msg::<ServerMsg>()).await {
            Err(_) => return Ok(None),
            Ok(Err(e)) => return Err(format!("{e}")),
            Ok(Ok(m)) => m,
        };
        match &msg {
            ServerMsg::VideoFrame {
                frame_id,
                data_len,
                aux_len,
                ..
            } => {
                self.rd
                    .read_payload(data_len + aux_len)
                    .await
                    .map_err(|e| e.to_string())?;
                self.seen.frames += 1;
                self.seen.last_frame_id = *frame_id;
                let ack = ClientMsg::FrameAck {
                    frame_id: *frame_id,
                    decoded_at_ms: 0,
                };
                self.wr.write_msg(&ack).await.map_err(|e| e.to_string())?;
            }
            ServerMsg::CursorShape { argb_len, .. } => {
                self.rd
                    .read_payload(*argb_len)
                    .await
                    .map_err(|e| e.to_string())?;
                self.seen.cursor_shapes += 1;
            }
            ServerMsg::CursorPos { x, y, visible, .. } => {
                self.seen.cursor = Some((*x, *y, *visible));
            }
            ServerMsg::ClipboardOffer { mime_types, .. } => {
                self.seen.offers.push(mime_types.clone());
            }
            ServerMsg::StreamConfig {
                width,
                height,
                pipeline,
                scale_milli,
                ..
            } => {
                self.seen.stream = Some((*width, *height));
                self.seen.scale_milli = *scale_milli;
                self.seen.configs += 1;
                self.seen.frames_at_config = self.seen.frames;
                self.seen.pipeline = *pipeline;
            }
            _ => {}
        }
        Ok(Some(msg))
    }

    /// Read until `done` is true or `wait` passes.
    async fn until(
        &mut self,
        wait: Duration,
        mut done: impl FnMut(&Seen) -> bool,
    ) -> Result<bool, String> {
        let end = Instant::now() + wait;
        while Instant::now() < end {
            if done(&self.seen) {
                return Ok(true);
            }
            self.step(Duration::from_millis(200)).await?;
        }
        Ok(done(&self.seen))
    }
}

fn monitor_names() -> Result<Vec<String>, String> {
    let desktop = gnome_ipc::find_session().map_err(|e| format!("{e:#}"))?;
    let conn = gnome_ipc::connect(&desktop.bus_address).map_err(|e| format!("{e:#}"))?;
    let mut names: Vec<String> = gnome_ipc::monitors(&conn)
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .map(|m| m.connector)
        .collect();
    names.sort();
    Ok(names)
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let mut args = std::env::args().skip(1);
    let Some(server) = args.next() else {
        eprintln!("usage: serve_probe /path/to/gliff-server [--headless] [server args...]");
        std::process::exit(2);
    };
    let mut extra: Vec<String> = args.collect();
    let headless = extra.iter().any(|a| a == "--headless");
    let mut clipboard = None;
    if let Some(i) = extra.iter().position(|a| a == "--clipboard") {
        clipboard = extra.get(i + 1).and_then(|s| s.parse::<u64>().ok());
        extra.drain(i..(i + 2).min(extra.len()));
    }
    let healthy = run(&server, &extra, headless, clipboard).await;
    std::process::exit(if healthy { 0 } else { 1 });
}

async fn run(server: &str, extra: &[String], headless: bool, clipboard: Option<u64>) -> bool {
    let before = match monitor_names() {
        Ok(n) => n,
        Err(e) => return fail("monitors", e),
    };
    let mut child: Child = match Command::new(server)
        .arg("--stdio")
        .args(extra)
        .env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or_else(|_| "info".into()),
        )
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return fail("spawn", format!("{server}: {e}")),
    };
    let logs = BufReader::new(child.stderr.take().expect("stderr")).lines();
    let log_task = tokio::spawn(async move {
        let mut logs = logs;
        let mut lines = Vec::new();
        while let Ok(Some(l)) = logs.next_line().await {
            lines.push(l);
        }
        lines
    });
    let mut c = Client {
        rd: Framed::new(child.stdout.take().expect("stdout")),
        wr: Framed::new(child.stdin.take().expect("stdin")),
        seen: Seen::default(),
    };
    let mut healthy = true;

    let hello = ClientMsg::Hello {
        version: PROTOCOL_VERSION,
        keymap: String::new(),
        caps: ClientCaps {
            codecs: vec![Codec::H264],
            max_width: 3840,
            max_height: 2160,
            chroma: vec![ChromaMode::Single420],
            features: features(),
        },
    };
    if let Err(e) = c.wr.write_msg(&hello).await {
        return fail("hello", e);
    }
    match c.step(Duration::from_secs(20)).await {
        Ok(Some(ServerMsg::HelloAck {
            session, outputs, ..
        })) => {
            ok(
                "hello",
                format!(
                    "headless={} output={} outputs={:?}",
                    session.headless,
                    session.output,
                    outputs
                        .iter()
                        .map(|o| (o.name.as_str(), o.width, o.height))
                        .collect::<Vec<_>>()
                ),
            );
            if session.headless != headless {
                healthy &= fail("mode", "headless flag does not match the request");
            }
        }
        Ok(Some(ServerMsg::Error { message, .. })) => return fail("hello", message),
        Ok(other) => return fail("hello", format!("unexpected {other:?}")),
        Err(e) => return fail("hello", e),
    }

    match c
        .until(Duration::from_secs(15), |s| {
            s.frames > 0 && s.stream.is_some()
        })
        .await
    {
        Ok(true) => ok(
            "first frame",
            format!(
                "stream {:?}, pipeline {:?}, {} frame(s)",
                c.seen.stream, c.seen.pipeline, c.seen.frames
            ),
        ),
        Ok(false) => healthy &= fail("first frame", "none in 15 s"),
        Err(e) => return fail("first frame", e),
    }

    if headless {
        match monitor_names() {
            Ok(now) if now.len() == before.len() + 1 => {
                let new: Vec<_> = now.iter().filter(|n| !before.contains(n)).collect();
                ok("new monitor", format!("{new:?} while the session runs"));
            }
            Ok(now) => healthy &= fail("new monitor", format!("{before:?} -> {now:?}")),
            Err(e) => healthy &= fail("new monitor", e),
        }
    }

    // Pointer: the cursor should follow within a pixel.
    let probe = ClientMsg::PointerMotion { x: 100.0, y: 200.0 };
    let _ = c.wr.write_msg(&probe).await;
    let seen_at = |s: &Seen| {
        s.cursor
            .is_some_and(|(x, y, _)| (x - 100.0).abs() < 2.0 && (y - 200.0).abs() < 2.0)
    };
    match c.until(Duration::from_secs(5), seen_at).await {
        Ok(true) => ok("pointer", format!("cursor at {:?}", c.seen.cursor)),
        Ok(false) => healthy &= fail("pointer", format!("cursor is {:?}", c.seen.cursor)),
        Err(e) => return fail("pointer", e),
    }
    // The shape follows the pointer onto the monitor, a moment later.
    let _ = c
        .until(Duration::from_secs(3), |s| s.cursor_shapes > 0)
        .await;
    if c.seen.cursor_shapes > 0 {
        ok("cursor shape", format!("{} received", c.seen.cursor_shapes));
    } else {
        healthy &= fail("cursor shape", "none received");
    }

    if headless {
        for (w, h) in [(1280u32, 720u32), (1600, 900)] {
            let resize = ClientMsg::Resize {
                width: w,
                height: h,
                scale: 1.0,
            };
            let _ = c.wr.write_msg(&resize).await;
            let after = c.seen.last_frame_id;
            let reached = c
                .until(Duration::from_secs(10), |s| {
                    s.stream == Some((w, h)) && s.last_frame_id > after
                })
                .await;
            match reached {
                Ok(true) => ok("resize", format!("stream {w}x{h} with a frame after it")),
                Ok(false) => {
                    healthy &= fail("resize", format!("{w}x{h}: stream is {:?}", c.seen.stream))
                }
                Err(e) => return fail("resize", e),
            }
        }
    }

    if headless {
        // A HiDPI viewer: 2560x1440 pixels at scale 2 is a 1280x720 desktop.
        let (w, h) = (2560u32, 1440u32);
        let resize = ClientMsg::Resize {
            width: w,
            height: h,
            scale: 2.0,
        };
        let _ = c.wr.write_msg(&resize).await;
        let after = c.seen.last_frame_id;
        let reached = c
            .until(Duration::from_secs(10), |s| {
                s.stream == Some((w, h)) && s.last_frame_id > after
            })
            .await;
        match reached {
            Ok(true) if c.seen.scale_milli == 2000 => ok(
                "scale 2",
                format!("stream {w}x{h}, scale {}", c.seen.scale_milli),
            ),
            Ok(true) => {
                healthy &= fail(
                    "scale 2",
                    format!("stream {w}x{h} but scale {}", c.seen.scale_milli),
                )
            }
            Ok(false) => healthy &= fail("scale 2", format!("stream is {:?}", c.seen.stream)),
            Err(e) => return fail("scale 2", e),
        }
        // Motion goes in the logical space (1280x720 here); the cursor comes
        // back in stream pixels, so twice the numbers.
        let _ =
            c.wr.write_msg(&ClientMsg::PointerMotion {
                x: 1200.0,
                y: 650.0,
            })
            .await;
        let near = |s: &Seen| {
            s.cursor
                .is_some_and(|(x, y, _)| (x - 2400.0).abs() < 3.0 && (y - 1300.0).abs() < 3.0)
        };
        match c.until(Duration::from_secs(5), near).await {
            Ok(true) => ok("scale 2 pointer", format!("cursor at {:?}", c.seen.cursor)),
            Ok(false) => {
                healthy &= fail("scale 2 pointer", format!("cursor is {:?}", c.seen.cursor))
            }
            Err(e) => return fail("scale 2 pointer", e),
        }
        // Back to 1 again, a fractional scale (what Mutter makes of it depends
        // on the desktop), a 4K window at scale 2 and a scale change at the
        // size it already has (Mutter ignores that one). The stream may be
        // scaled to the encoder's limit, so only the frames are required.
        for (w, h, scale) in [
            (1600u32, 900u32, 1.0f32),
            (1920, 1080, 1.5),
            (3840, 2160, 2.0),
            (1600, 900, 1.0),
            (1600, 900, 2.0),
        ] {
            let resize = ClientMsg::Resize {
                width: w,
                height: h,
                scale,
            };
            let _ = c.wr.write_msg(&resize).await;
            let configs = c.seen.configs;
            let reached = c
                .until(Duration::from_secs(10), |s| {
                    s.configs > configs && s.frames > s.frames_at_config
                })
                .await;
            match reached {
                Ok(true) => ok(
                    "scale change",
                    format!(
                        "asked {w}x{h} at {scale}: stream {:?} at scale {}",
                        c.seen.stream, c.seen.scale_milli
                    ),
                ),
                Ok(false) => {
                    healthy &= fail("scale change", format!("{w}x{h} at {scale}: no frames"))
                }
                Err(e) => return fail("scale change", e),
            }
        }
    }

    if let Some(secs) = clipboard {
        let offer = ClientMsg::ClipboardOffer {
            serial: 1,
            mime_types: vec!["text/plain;charset=utf-8".into()],
            files: Vec::new(),
        };
        let _ = c.wr.write_msg(&offer).await;
        println!("offered a text selection; waiting {secs} s for the server's");
        let end = Instant::now() + Duration::from_secs(secs);
        while Instant::now() < end {
            match c.step(Duration::from_millis(200)).await {
                Ok(Some(
                    ServerMsg::VideoFrame { .. }
                    | ServerMsg::CursorPos { .. }
                    | ServerMsg::CursorShape { .. },
                ))
                | Ok(None) => {}
                Ok(Some(other)) => println!("  server sent {other:?}"),
                Err(e) => {
                    println!("  read failed: {e}");
                    break;
                }
            }
        }
        println!("selections from the server: {:?}", c.seen.offers);
    }

    // End the session by closing stdin, as ssh does when the viewer goes.
    drop(c.wr);
    let status = timeout(Duration::from_secs(10), child.wait()).await;
    match status {
        Ok(Ok(s)) => ok("exit", format!("{s}")),
        Ok(Err(e)) => healthy &= fail("exit", e),
        Err(_) => {
            healthy &= fail("exit", "the server did not end within 10 s");
            let _ = child.kill().await;
        }
    }
    tokio::time::sleep(Duration::from_millis(500)).await;
    match monitor_names() {
        Ok(after) if after == before => ok("cleanup", format!("monitors are back to {after:?}")),
        Ok(after) => healthy &= fail("cleanup", format!("{before:?} -> {after:?}")),
        Err(e) => healthy &= fail("cleanup", e),
    }
    if let Ok(lines) = log_task.await {
        let interesting = lines
            .iter()
            .filter(|l| {
                l.contains("WARN")
                    || l.contains("clipboard")
                    || l.contains("ERROR")
                    || l.contains("session output ready")
            })
            .collect::<Vec<_>>();
        println!("server log ({} lines), notable:", lines.len());
        for l in interesting {
            println!("  {l}");
        }
    }
    healthy
}
