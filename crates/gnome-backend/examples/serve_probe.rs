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
//! With `--rich-clipboard` the client also moves an image, two files (one of
//! 3 MB, so it takes several chunks) and a folder with a file in it through
//! the server's clipboard both ways, with a plain Mutter client playing the desktop's applications. It
//! replaces the desktop's clipboard: run it where that is fine.
//!
//! With `--headless --soak SECS` the session then stays up for SECS seconds,
//! resizing (and changing the scale) every few seconds, and samples the
//! server's memory, open files and threads, to catch a leak or a stall that a
//! short run cannot.
//!
//! With `--load SECS` (a mirrored screen, with `gliff-gnome-animate` running
//! on it; or with `--headless`, then without the resize and scale steps and
//! with `gliff-gnome-animate SECS RECTS Meta-` to cover the private screen)
//! the client then judges the stream while the picture changes all the
//! time: frames per second, the longest gap, bytes per frame, the server's CPU
//! and memory. `--stall SECS` adds a stretch where the client stops reading, as
//! a slow link would, and checks that the server's memory stays bounded and
//! that the stream resumes without replaying the backlog.
//!
//! One line per check, `[ OK ]` or `[FAIL]`. It does not decode video: it
//! checks that frames of the announced size arrive and are acknowledged.

use std::collections::{HashMap, HashSet};
use std::process::Stdio;
use std::time::{Duration, Instant};

use gliff_proto::{
    features, ChromaMode, ClientCaps, ClientMsg, ClipboardFile, ClipboardItem, Codec, ServerMsg,
    PROTOCOL_VERSION,
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

/// The decoder of `--decode`: the viewer's CPU one, or with `--decode-gpu`
/// its GPU one (VA-API decode and the Vulkan recombine, read back).
enum PictureDecoder {
    Cpu(Box<gliff_sw::Decoder>),
    Gpu(Box<gliff_vk::Decoder>, (u32, u32)),
}

/// A decoded picture: its size and BGRA pixels.
type Picture = ((u32, u32), Vec<u8>);

impl PictureDecoder {
    /// The size and BGRA pixels of the next picture, if there is one.
    fn decode(&mut self, main: &[u8], aux: &[u8]) -> Result<Option<Picture>, String> {
        match self {
            Self::Cpu(d) => d
                .decode(main, aux)
                .map(|f| f.map(|f| ((f.width, f.height), f.pixels)))
                .map_err(|e| e.to_string()),
            Self::Gpu(d, size) => d
                .decode_to_bgra(main, aux)
                .map(|f| f.map(|px| (*size, px)))
                .map_err(|e| e.to_string()),
        }
    }

    /// The picture a quiet stream leaves inside the decoder. The CPU decoder
    /// holds the first picture of a stream back until the next unit; the
    /// viewer flushes it after 150 ms without one (`IDLE_DRAIN`), and so must
    /// we, or a still screen is seen one picture late. The GPU decoder holds
    /// nothing.
    fn drain(&mut self) -> Result<Option<Picture>, String> {
        match self {
            Self::Cpu(d) if d.has_pending() => d
                .flush()
                .map(|f| f.map(|f| ((f.width, f.height), f.pixels)))
                .map_err(|e| e.to_string()),
            _ => Ok(None),
        }
    }
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
    /// When set, every video frame is logged: arrival time, id, bytes, key.
    frame_log: Option<Vec<(Instant, u64, usize, bool)>>,
    /// With `--decode`: a decoder for the current stream, the mean colour
    /// (r, g, b) of the middle of the last decoded picture with its size,
    /// how many pictures were decoded, and what went wrong.
    decode: bool,
    /// The GPU for `--decode-gpu`, opened once.
    decode_gpu: Option<std::sync::Arc<gliff_vk::Gpu>>,
    decoder: Option<PictureDecoder>,
    /// What the decoder was made for: chroma mode and stream size.
    decoder_for: Option<(ChromaMode, u32, u32)>,
    /// Pictures decoded when the last `StreamConfig` came, so a check after
    /// a step can require a picture from after it, and when the last video
    /// frame arrived (the viewer's idle drain counts quiet from there).
    decoded_at_config: u64,
    last_video: Option<Instant>,
    decoded: Option<((u32, u32), [f64; 3])>,
    decoded_count: u64,
    decode_errors: Vec<String>,
    /// Frames after the last `StreamConfig`, and the first of them that was
    /// a keyframe (its position among them, from 1).
    frames_since_config: u64,
    first_key_since_config: Option<u64>,
    cursor_shapes: u32,
    cursor: Option<(f64, f64, bool)>,
    /// Selections the server reported, by mime types.
    offers: Vec<Vec<String>>,
    /// The last offer's serial and files.
    offer_serial: u32,
    offer_files: Vec<ClipboardFile>,
    /// Clipboard data the server sent, by request id, and the ids whose last
    /// chunk came.
    incoming: HashMap<u32, Vec<u8>>,
    finished: HashSet<u32>,
    /// Requests the server made of us (id, serial, item), not yet answered,
    /// and the most each of our answers has been acknowledged up to.
    requests: Vec<(u32, u32, ClipboardItem)>,
    acks: HashMap<u32, u64>,
    /// Bytes of each answer of ours sent so far, ids the server aborted, the
    /// serial of the offer we made (requests must name it) and everything the
    /// server did that the protocol does not allow.
    sent: HashMap<u32, u64>,
    aborted: HashSet<u32>,
    my_serial: Option<u32>,
    violations: Vec<String>,
}

struct Client {
    rd: Framed<ChildStdout>,
    wr: Framed<ChildStdin>,
    seen: Seen,
}

impl Client {
    /// Note a decoded picture, or a decode error.
    fn note_picture(&mut self, decoded: Result<Option<Picture>, String>) {
        match decoded {
            Ok(Some(((w, h), px))) => {
                self.seen.decoded_count += 1;
                self.seen.decoded = Some(((w, h), middle_colour(&px, w, h)));
            }
            Ok(None) => {}
            Err(e) => {
                if self.seen.decode_errors.len() < 5 {
                    self.seen.decode_errors.push(e);
                }
            }
        }
    }

    /// The viewer's idle drain: 150 ms without a video frame (whatever else
    /// arrives meanwhile) releases the picture the CPU decoder holds back.
    fn drain_if_quiet(&mut self) {
        const IDLE_DRAIN: Duration = Duration::from_millis(150);
        let quiet = self
            .seen
            .last_video
            .is_none_or(|t| t.elapsed() >= IDLE_DRAIN);
        if !quiet {
            return;
        }
        let drained = self.seen.decoder.as_mut().map(PictureDecoder::drain);
        if let Some(drained) = drained {
            self.note_picture(drained);
        }
    }

    /// Read one message (and its payload), acknowledging video frames.
    async fn step(&mut self, wait: Duration) -> Result<Option<ServerMsg>, String> {
        let msg = match timeout(wait, self.rd.read_msg::<ServerMsg>()).await {
            Err(_) => {
                self.drain_if_quiet();
                return Ok(None);
            }
            Ok(Err(e)) => return Err(format!("{e}")),
            Ok(Ok(m)) => m,
        };
        match &msg {
            ServerMsg::VideoFrame {
                frame_id,
                data_len,
                aux_len,
                keyframe,
                ..
            } => {
                let payload = self
                    .rd
                    .read_payload(data_len + aux_len)
                    .await
                    .map_err(|e| e.to_string())?;
                if let Some(decoder) = self.seen.decoder.as_mut() {
                    let (main, aux) = payload.split_at(*data_len as usize);
                    let decoded = decoder.decode(main, aux);
                    self.note_picture(decoded);
                }
                self.seen.last_video = Some(Instant::now());
                self.seen.frames += 1;
                self.seen.frames_since_config += 1;
                if *keyframe && self.seen.first_key_since_config.is_none() {
                    self.seen.first_key_since_config = Some(self.seen.frames_since_config);
                }
                self.seen.last_frame_id = *frame_id;
                if let Some(log) = self.seen.frame_log.as_mut() {
                    log.push((
                        Instant::now(),
                        *frame_id,
                        (*data_len + *aux_len) as usize,
                        *keyframe,
                    ));
                }
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
            ServerMsg::ClipboardOffer {
                serial,
                mime_types,
                files,
            } => {
                self.seen.offers.push(mime_types.clone());
                self.seen.offer_serial = *serial;
                self.seen.offer_files = files.clone();
            }
            ServerMsg::ClipboardData {
                id,
                offset,
                data_len,
                done,
            } => {
                let chunk = self
                    .rd
                    .read_payload(*data_len)
                    .await
                    .map_err(|e| e.to_string())?;
                let got = self.seen.incoming.entry(*id).or_default();
                if *offset != got.len() as u64 {
                    self.seen.violations.push(format!(
                        "chunk of request {id} at offset {offset}, expected {}",
                        got.len()
                    ));
                }
                if chunk.len() > 256 * 1024 {
                    self.seen
                        .violations
                        .push(format!("chunk of request {id} is {} bytes", chunk.len()));
                }
                got.extend_from_slice(&chunk);
                let received = got.len() as u64;
                if *done {
                    self.seen.finished.insert(*id);
                } else {
                    let ack = ClientMsg::ClipboardAck { id: *id, received };
                    self.wr.write_msg(&ack).await.map_err(|e| e.to_string())?;
                }
            }
            ServerMsg::ClipboardRequest { id, serial, item } => {
                // The server's ids are even, and it must name our offer.
                if id % 2 != 0 || self.seen.my_serial.is_some_and(|m| m != *serial) {
                    self.seen.violations.push(format!(
                        "request {id} for serial {serial} (ours is {:?})",
                        self.seen.my_serial
                    ));
                } else {
                    self.seen.requests.push((*id, *serial, item.clone()));
                }
            }
            ServerMsg::ClipboardAck { id, received } => {
                if *received > self.seen.sent.get(id).copied().unwrap_or(0) {
                    self.seen.violations.push(format!(
                        "ack of {received} bytes for request {id}, which got {:?}",
                        self.seen.sent.get(id)
                    ));
                }
                let at = self.seen.acks.entry(*id).or_default();
                *at = (*at).max(*received);
            }
            ServerMsg::ClipboardAbort { id } => {
                self.seen.aborted.insert(*id);
            }
            ServerMsg::StreamConfig {
                width,
                height,
                pipeline,
                scale_milli,
                chroma,
                ..
            } => {
                self.seen.frames_since_config = 0;
                self.seen.first_key_since_config = None;
                self.seen.decoded_at_config = self.seen.decoded_count;
                // As the viewer does: a change that only paces the stream (the
                // frame-rate cap) keeps the decoder; a change of the coded
                // stream makes a new one.
                let coded = (*chroma, *width, *height);
                if self.seen.decode
                    && (self.seen.decoder_for != Some(coded) || self.seen.decoder.is_none())
                {
                    // Both stream kinds but the plain 4:2:0 one have an
                    // auxiliary stream.
                    self.seen.decoded = None;
                    self.seen.decoder_for = Some(coded);
                    let dual = *chroma != ChromaMode::Single420;
                    let made = match &self.seen.decode_gpu {
                        Some(gpu) => gliff_vk::Decoder::new(gpu, dual, *width, *height)
                            .map(|d| PictureDecoder::Gpu(Box::new(d), (*width, *height)))
                            .map_err(|e| e.to_string()),
                        None => gliff_sw::Decoder::new(dual, *width, *height)
                            .map(|d| PictureDecoder::Cpu(Box::new(d)))
                            .map_err(|e| e.to_string()),
                    };
                    self.seen.decoder = match made {
                        Ok(d) => Some(d),
                        Err(e) => {
                            self.seen.decode_errors.push(format!("decoder: {e}"));
                            None
                        }
                    };
                }
                self.seen.stream = Some((*width, *height));
                self.seen.scale_milli = *scale_milli;
                self.seen.configs += 1;
                self.seen.frames_at_config = self.seen.frames;
                self.seen.pipeline = *pipeline;
            }
            _ => {}
        }
        self.drain_if_quiet();
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

/// Every monitor's connector and refresh rate, sorted by connector.
fn monitor_rates() -> Result<Vec<(String, f64)>, String> {
    let desktop = gnome_ipc::find_session().map_err(|e| format!("{e:#}"))?;
    let conn = gnome_ipc::connect(&desktop.bus_address).map_err(|e| format!("{e:#}"))?;
    let mut rates: Vec<(String, f64)> = gnome_ipc::monitors(&conn)
        .map_err(|e| format!("{e:#}"))?
        .into_iter()
        .map(|m| (m.connector, m.refresh_hz))
        .collect();
    rates.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(rates)
}

/// The mean (r, g, b) of the middle half of a BGRA picture, from a grid of
/// samples.
fn middle_colour(bgra: &[u8], width: u32, height: u32) -> [f64; 3] {
    let (w, h) = (width as usize, height as usize);
    let (x0, x1, y0, y1) = (w / 4, w * 3 / 4, h / 4, h * 3 / 4);
    let (mut sum, mut n) = ([0.0f64; 3], 0.0f64);
    for y in (y0..y1).step_by(8) {
        for x in (x0..x1).step_by(8) {
            let i = (y * w + x) * 4;
            if let Some(px) = bgra.get(i..i + 3) {
                sum[0] += f64::from(px[2]);
                sum[1] += f64::from(px[1]);
                sum[2] += f64::from(px[0]);
                n += 1.0;
            }
        }
    }
    if n == 0.0 {
        return [0.0; 3];
    }
    [sum[0] / n, sum[1] / n, sum[2] / n]
}

/// Whether `got` is `want` within `tolerance` on every channel.
fn colour_matches(got: [f64; 3], want: [u8; 3], tolerance: f64) -> bool {
    got.iter()
        .zip(want)
        .all(|(g, w)| (g - f64::from(w)).abs() <= tolerance)
}

/// With `--decode`, after a change of the stream: the decoded picture is the
/// screen's colour again, at the stream's size within 10 s. (A change of the
/// coded stream makes a new decoder, which needs a keyframe first.)
async fn decode_follows(c: &mut Client, want: Option<[u8; 3]>, what: &str) -> bool {
    let Some(want) = want else { return true };
    // A picture decoded after the step's StreamConfig: one left from before
    // (a same-size scale change keeps the decoder and the picture) would
    // satisfy the colour without showing anything about the step.
    let hit = |s: &Seen| {
        s.decoded_count > s.decoded_at_config
            && s.decoded
                .is_some_and(|(size, m)| s.stream == Some(size) && colour_matches(m, want, 16.0))
    };
    let name = format!("decoded after {what}");
    match c.until(Duration::from_secs(10), hit).await {
        Ok(true) => {
            let (size, m) = c.seen.decoded.unwrap_or_default();
            ok(
                &name,
                format!(
                    "{size:?}, middle rgb ({:.0}, {:.0}, {:.0})",
                    m[0], m[1], m[2]
                ),
            );
            true
        }
        Ok(false) => fail(
            &name,
            format!(
                "stream {:?}, last {:?}; errors {:?}; {} stream config(s), {} frame(s) since the last, first keyframe among them at {:?}",
                c.seen.stream,
                c.seen.decoded,
                c.seen.decode_errors,
                c.seen.configs,
                c.seen.frames_since_config,
                c.seen.first_key_since_config
            ),
        ),
        Err(e) => fail(&name, e),
    }
}

fn monitor_names() -> Result<Vec<String>, String> {
    Ok(monitor_rates()?.into_iter().map(|(n, _)| n).collect())
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
    let rich = match extra.iter().position(|a| a == "--rich-clipboard") {
        Some(i) => {
            extra.remove(i);
            true
        }
        None => false,
    };
    let mut soak = None;
    if let Some(i) = extra.iter().position(|a| a == "--soak") {
        soak = extra
            .get(i + 1)
            .and_then(|s| s.parse::<u64>().ok())
            .filter(|&secs| secs > 0 && secs <= 7 * 24 * 3600);
        if soak.is_none() || !headless {
            eprintln!("--soak SECS needs a duration of 1 s to a week, and --headless");
            std::process::exit(2);
        }
        extra.drain(i..(i + 2).min(extra.len()));
    }
    // --load SECS [--stall SECS]: judge the stream while the screen changes
    // all the time (run gliff-gnome-animate beside it), and then while the
    // client stops reading.
    let mut load = None;
    let mut stall = None;
    for (flag, slot) in [("--load", &mut load), ("--stall", &mut stall)] {
        if let Some(i) = extra.iter().position(|a| a == flag) {
            *slot = extra
                .get(i + 1)
                .and_then(|s| s.parse::<u64>().ok())
                .filter(|&secs| secs > 0 && secs <= 3600);
            if slot.is_none() {
                eprintln!("{flag} needs a duration of 1 s to an hour");
                std::process::exit(2);
            }
            extra.drain(i..(i + 2).min(extra.len()));
        }
    }
    if stall.is_some() && load.is_none() {
        eprintln!("--stall needs --load");
        std::process::exit(2);
    }
    // --decode RRGGBB: decode the stream like the viewer and require the
    // middle of the picture to be this colour (cover the mirrored or private
    // screen with `gliff-gnome-animate SECS solid:RRGGBB [CONNECTOR]`).
    let mut decode = None;
    if let Some(i) = extra.iter().position(|a| a == "--decode") {
        decode = extra
            .get(i + 1)
            .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
            .and_then(|h| {
                let v = u32::from_str_radix(h, 16).ok()?;
                Some([(v >> 16) as u8, (v >> 8) as u8, v as u8])
            });
        if decode.is_none() {
            eprintln!("--decode needs a colour as six hex digits, RRGGBB");
            std::process::exit(2);
        }
        extra.drain(i..(i + 2).min(extra.len()));
    }
    // --decode-gpu: with --decode, decode on the GPU as the viewer does.
    let mut decode_gpu = false;
    if let Some(i) = extra.iter().position(|a| a == "--decode-gpu") {
        if decode.is_none() {
            eprintln!("--decode-gpu needs --decode RRGGBB");
            std::process::exit(2);
        }
        decode_gpu = true;
        extra.remove(i);
    }
    let healthy = run(
        &server,
        &extra,
        headless,
        clipboard,
        rich,
        soak,
        load.map(|l| (l, stall)),
        decode.map(|colour| (colour, decode_gpu)),
    )
    .await;
    std::process::exit(if healthy { 0 } else { 1 });
}

#[allow(clippy::too_many_arguments)]
async fn run(
    server: &str,
    extra: &[String],
    headless: bool,
    clipboard: Option<u64>,
    rich: bool,
    soak_secs: Option<u64>,
    load: Option<(u64, Option<u64>)>,
    decode: Option<([u8; 3], bool)>,
) -> bool {
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
    let decode_gpu = match decode {
        Some((_, true)) => {
            let node = std::env::var_os("GLIFF_RENDER_NODE").map(std::path::PathBuf::from);
            match gliff_vk::Gpu::open(node.as_deref()) {
                Ok(gpu) if gpu.can_decode() => Some(gpu),
                Ok(gpu) => {
                    return fail(
                        "decode gpu",
                        format!("{} has no VA-API H.264 decoder", gpu.name),
                    )
                }
                Err(e) => return fail("decode gpu", format!("{e:#}")),
            }
        }
        _ => None,
    };
    let mut c = Client {
        rd: Framed::new(child.stdout.take().expect("stdout")),
        wr: Framed::new(child.stdin.take().expect("stdin")),
        seen: Seen {
            decode: decode.is_some(),
            decode_gpu,
            ..Seen::default()
        },
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

    if let Some((want, _)) = decode {
        // The first pictures may be what was on the screen before the
        // coloured window came up: wait for the colour.
        let hit = |s: &Seen| {
            s.decoded
                .is_some_and(|(size, m)| s.stream == Some(size) && colour_matches(m, want, 16.0))
        };
        match c.until(Duration::from_secs(25), hit).await {
            Ok(true) => {
                let (size, m) = c.seen.decoded.unwrap_or_default();
                let on = if c.seen.decode_gpu.is_some() {
                    "GPU"
                } else {
                    "CPU"
                };
                ok(
                    "decoded picture",
                    format!(
                        "on the {on} decoder, {size:?}, middle rgb ({:.0}, {:.0}, {:.0}) for wanted {:02x}{:02x}{:02x}; {} pictures decoded, {} decode errors",
                        m[0], m[1], m[2], want[0], want[1], want[2],
                        c.seen.decoded_count, c.seen.decode_errors.len()
                    ),
                );
            }
            Ok(false) => {
                healthy &= fail(
                    "decoded picture",
                    format!(
                        "last {:?} for wanted {:02x}{:02x}{:02x} within 25 s; {} decoded, errors {:?}; {} stream config(s), {} frame(s) since the last, first keyframe among them at {:?}",
                        c.seen.decoded, want[0], want[1], want[2],
                        c.seen.decoded_count, c.seen.decode_errors,
                        c.seen.configs, c.seen.frames_since_config, c.seen.first_key_since_config
                    ),
                );
            }
            Err(e) => return fail("decoded picture", e),
        }
    }

    if headless {
        // One snapshot: the new monitor and its rate come from the same read.
        match monitor_rates() {
            Ok(now) if now.len() == before.len() + 1 => {
                let new: Vec<_> = now.iter().filter(|(n, _)| !before.contains(n)).collect();
                let names: Vec<_> = new.iter().map(|(n, _)| n).collect();
                ok("new monitor", format!("{names:?} while the session runs"));
                // Mutter takes a virtual monitor's refresh rate from the frame
                // rate negotiated for its stream: asking for a limit of 0 once
                // made a 0 Hz screen that still produced sized frames.
                if new.is_empty() {
                    healthy &= fail("new monitor rate", "no monitor is new");
                }
                for (name, hz) in new {
                    if *hz >= 30.0 {
                        ok("new monitor rate", format!("{name} at {hz:.3} Hz"));
                    } else {
                        healthy &= fail(
                            "new monitor rate",
                            format!("{name} is at {hz:.3} Hz, not a usable refresh rate"),
                        );
                    }
                }
            }
            Ok(now) => {
                let now: Vec<_> = now.into_iter().map(|(n, _)| n).collect();
                healthy &= fail("new monitor", format!("{before:?} -> {now:?}"))
            }
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

    // The resize and scale steps change the screen under a --load run.
    if headless && load.is_none() {
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
            healthy &=
                decode_follows(&mut c, decode.map(|d| d.0), &format!("resize to {w}x{h}")).await;
        }
    }

    if headless && load.is_none() {
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
        healthy &= decode_follows(&mut c, decode.map(|d| d.0), "scale 2").await;
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
            healthy &= decode_follows(
                &mut c,
                decode.map(|d| d.0),
                &format!("scale change to {w}x{h} at {scale}"),
            )
            .await;
        }
    }

    if let (Some(secs), true, Some(pid)) = (soak_secs, headless, child.id()) {
        healthy &= soak(&mut c, pid, secs).await;
    }

    if let (Some((secs, stall)), Some(pid)) = (load, child.id()) {
        healthy &= load_test(&mut c, pid, secs, stall).await;
    }

    // After a failed rich check the stream may be in the middle of a message:
    // the text offer below is skipped, and the serial the rich check made the
    // server's requests name no longer applies.
    let rich_ok = !rich || rich_clipboard(&mut c).await;
    healthy &= rich_ok;
    c.seen.my_serial = None;

    if let Some(secs) = clipboard.filter(|_| rich_ok) {
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
        Ok(Ok(s)) if s.success() => ok("exit", format!("{s}")),
        Ok(Ok(s)) => healthy &= fail("exit", format!("the server ended with {s}")),
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
                    || l.contains("ladder step")
            })
            .collect::<Vec<_>>();
        println!("server log ({} lines), notable:", lines.len());
        for l in interesting {
            println!("  {l}");
        }
    }
    healthy
}

// ---- the rich clipboard check ------------------------------------------

const PNG: &str = "image/png";
const TEXT: &str = "text/plain;charset=utf-8";
const URI_LIST: &str = "text/uri-list";
const GNOME_FILES: &str = "x-special/gnome-copied-files";

/// Bytes that do not repeat, so a chunk in the wrong place is noticed.
fn blob(len: usize, seed: u32) -> Vec<u8> {
    let mut x = seed;
    (0..len)
        .map(|_| {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            (x >> 24) as u8
        })
        .collect()
}

/// A plain Mutter clipboard client: it plays the desktop's applications, with
/// none of the bridge's rules.
struct App {
    _session: gnome_ipc::Session,
    port: gnome_ipc::ClipboardPort,
    selections: std::sync::mpsc::Receiver<Vec<String>>,
    payloads: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>>,
}

impl App {
    fn start() -> Result<App, String> {
        let e = |e: anyhow::Error| format!("{e:#}");
        let address = gnome_ipc::find_session().map_err(e)?.bus_address;
        let session = gnome_ipc::Session::create_remote_only(&address).map_err(e)?;
        let (port, signals) = session.enable_clipboard().map_err(e)?;
        session.start().map_err(e)?;
        let payloads: std::sync::Arc<std::sync::Mutex<HashMap<String, Vec<u8>>>> =
            Default::default();
        let (tx, selections) = std::sync::mpsc::channel();
        {
            let (port, payloads) = (port.clone(), payloads.clone());
            std::thread::spawn(move || {
                while let Ok(signal) = signals.recv() {
                    match signal {
                        gnome_ipc::ClipboardSignal::OwnerChanged {
                            mime_types,
                            is_owner: false,
                        } => {
                            let _ = tx.send(mime_types);
                        }
                        gnome_ipc::ClipboardSignal::OwnerChanged { .. } => {}
                        gnome_ipc::ClipboardSignal::Transfer { mime_type, serial } => {
                            use std::io::Write;
                            let data = payloads.lock().unwrap().get(&mime_type).cloned();
                            let done = match data {
                                Some(data) => port
                                    .selection_write(serial)
                                    .and_then(gnome_clipboard::into_blocking)
                                    .map(|fd| std::fs::File::from(fd).write_all(&data).is_ok())
                                    .unwrap_or(false),
                                None => false,
                            };
                            let _ = port.selection_write_done(serial, done);
                        }
                    }
                }
            });
        }
        Ok(App {
            _session: session,
            port,
            selections,
            payloads,
        })
    }

    fn offer(&self, items: &[(&str, Vec<u8>)]) {
        let mut types = Vec::new();
        for (mime, data) in items {
            self.payloads
                .lock()
                .unwrap()
                .insert(mime.to_string(), data.clone());
            types.push(mime.to_string());
        }
        let _ = self.port.set_selection(&types);
    }

    /// The selection's data in `mime`, as an application pasting would read it.
    fn paste(port: gnome_ipc::ClipboardPort, mime: String) -> Result<Vec<u8>, String> {
        use std::io::Read;
        let fd = port
            .selection_read(&mime)
            .and_then(gnome_clipboard::into_blocking)
            .map_err(|e| format!("{e:#}"))?;
        let mut out = Vec::new();
        std::fs::File::from(fd)
            .read_to_end(&mut out)
            .map_err(|e| e.to_string())?;
        Ok(out)
    }
}

impl Client {
    /// Ask the server for `item` of its offer `serial` and collect the data,
    /// within 30 s in all (a write or a half-sent payload included). After an
    /// error the stream may be in the middle of a message: do not go on.
    async fn fetch(
        &mut self,
        id: u32,
        serial: u32,
        item: ClipboardItem,
    ) -> Result<Vec<u8>, String> {
        timeout(Duration::from_secs(30), self.fetch_inner(id, serial, item))
            .await
            .map_err(|_| format!("request {id}: not finished in 30 s"))?
    }

    async fn fetch_inner(
        &mut self,
        id: u32,
        serial: u32,
        item: ClipboardItem,
    ) -> Result<Vec<u8>, String> {
        let req = ClientMsg::ClipboardRequest { id, serial, item };
        self.wr.write_msg(&req).await.map_err(|e| e.to_string())?;
        self.until(Duration::from_secs(30), |s| {
            s.finished.contains(&id) || s.aborted.contains(&id) || !s.violations.is_empty()
        })
        .await?;
        if let Some(v) = self.seen.violations.first() {
            return Err(format!("protocol violation by the server: {v}"));
        }
        if self.seen.aborted.contains(&id) {
            return Err(format!("request {id}: the server aborted it"));
        }
        if !self.seen.finished.contains(&id) {
            return Err(format!("request {id}: no final chunk in 30 s"));
        }
        Ok(self.seen.incoming.remove(&id).unwrap_or_default())
    }

    /// Answer the server's requests with `give` until `done` hands back the
    /// result of the paste they are for. Fails after 30 s in all (writes and
    /// half-sent payloads included), at once when the server breaks the
    /// protocol or aborts a transfer. After an error the stream may be in the
    /// middle of a message: do not go on.
    async fn answer<T>(
        &mut self,
        give: &dyn Fn(&ClipboardItem) -> Option<Vec<u8>>,
        done: impl FnMut() -> Option<T>,
    ) -> Result<T, String> {
        timeout(Duration::from_secs(30), self.answer_inner(give, done))
            .await
            .map_err(|_| "no answer to the paste in 30 s".to_string())?
    }

    async fn answer_inner<T>(
        &mut self,
        give: &dyn Fn(&ClipboardItem) -> Option<Vec<u8>>,
        mut done: impl FnMut() -> Option<T>,
    ) -> Result<T, String> {
        loop {
            if let Some(v) = self.seen.violations.first() {
                return Err(format!("protocol violation by the server: {v}"));
            }
            if !self.seen.aborted.is_empty() {
                return Err(format!(
                    "the server aborted transfer(s) {:?}",
                    self.seen.aborted
                ));
            }
            if let Some(result) = done() {
                return Ok(result);
            }
            self.step(Duration::from_millis(50)).await?;
            while let Some((id, _serial, item)) = self.seen.requests.pop() {
                let Some(data) = give(&item) else {
                    let _ = self.wr.write_msg(&ClientMsg::ClipboardAbort { id }).await;
                    continue;
                };
                self.send_chunks(id, &data).await?;
            }
        }
    }

    /// Send `data` as the answer to request `id`, at most a window of chunks
    /// ahead of what the server has acknowledged.
    async fn send_chunks(&mut self, id: u32, data: &[u8]) -> Result<(), String> {
        const CHUNK: usize = 256 * 1024;
        const WINDOW: usize = 4;
        let chunks: Vec<&[u8]> = if data.is_empty() {
            vec![&[][..]]
        } else {
            data.chunks(CHUNK).collect()
        };
        let mut offset = 0u64;
        for (n, chunk) in chunks.iter().enumerate() {
            // The chunk n - WINDOW must be acknowledged before chunk n goes.
            if n >= WINDOW {
                let need: u64 = chunks[..n - WINDOW + 1]
                    .iter()
                    .map(|c| c.len() as u64)
                    .sum();
                let end = Instant::now() + Duration::from_secs(10);
                while self.seen.acks.get(&id).copied().unwrap_or(0) < need {
                    if Instant::now() > end {
                        return Err(format!("request {id}: no acknowledgement of {need} bytes"));
                    }
                    if self.seen.aborted.contains(&id) {
                        return Err(format!("request {id}: the server aborted it"));
                    }
                    self.step(Duration::from_millis(50)).await?;
                }
            }
            let msg = ClientMsg::ClipboardData {
                id,
                offset,
                data_len: chunk.len() as u32,
                done: n + 1 == chunks.len(),
            };
            offset += chunk.len() as u64;
            self.seen.sent.insert(id, offset);
            self.wr
                .write_msg_with_payloads(&msg, &[chunk])
                .await
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }
}

/// A directory that goes away with the value, on every way out.
struct TempDir(std::path::PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn file_urls(dir: &std::path::Path, names: &[&str]) -> Vec<String> {
    names
        .iter()
        .map(|n| format!("file://{}", dir.join(n).display()))
        .collect()
}

/// Read the selection's `mime` as an application would, on a thread of its
/// own: the result comes on the channel. A read the server never answers
/// leaves its thread blocked until the probe exits; the probe does not wait
/// for it.
fn paste_in_background(
    port: &gnome_ipc::ClipboardPort,
    mime: &str,
) -> std::sync::mpsc::Receiver<Result<Vec<u8>, String>> {
    let (tx, rx) = std::sync::mpsc::channel();
    let (port, mime) = (port.clone(), mime.to_string());
    std::thread::spawn(move || {
        let _ = tx.send(App::paste(port, mime));
    });
    rx
}

/// The result of a background paste, once there is one.
fn paste_result(
    rx: &std::sync::mpsc::Receiver<Result<Vec<u8>, String>>,
) -> Option<Result<Vec<u8>, String>> {
    match rx.try_recv() {
        Ok(r) => Some(r),
        Err(std::sync::mpsc::TryRecvError::Empty) => None,
        Err(std::sync::mpsc::TryRecvError::Disconnected) => {
            Some(Err("the paste thread died".into()))
        }
    }
}

/// The entries an offer of the test files must list, sorted: exactly these,
/// each once.
fn expected_entries(note: &[u8], big: &[u8], inner: &[u8]) -> Vec<(String, u64, bool)> {
    let mut v = vec![
        ("note.txt".to_string(), note.len() as u64, false),
        ("big.bin".to_string(), big.len() as u64, false),
        ("pack".to_string(), 0, true),
        ("pack/inner.txt".to_string(), inner.len() as u64, false),
    ];
    v.sort();
    v
}

fn entry_set(files: &[ClipboardFile]) -> Vec<(String, u64, bool)> {
    let mut v: Vec<_> = files
        .iter()
        .map(|f| (f.path.clone(), f.size, f.dir))
        .collect();
    v.sort();
    v
}

/// An image, text, two files and a folder through the server's clipboard, both
/// ways.
async fn rich_clipboard(c: &mut Client) -> bool {
    let mut healthy = true;
    let dir = TempDir(std::env::temp_dir().join(format!("gliff-rich-{}", std::process::id())));
    let _ = std::fs::remove_dir_all(&dir.0);
    let png = blob(200_000, 1);
    let note = b"a small file\nwith two lines\n".to_vec();
    let big = blob(3_000_000, 2);
    let inner = b"inside a folder\n".to_vec();
    if std::fs::create_dir_all(dir.0.join("pack")).is_err()
        || std::fs::write(dir.0.join("note.txt"), &note).is_err()
        || std::fs::write(dir.0.join("big.bin"), &big).is_err()
        || std::fs::write(dir.0.join("pack/inner.txt"), &inner).is_err()
    {
        return fail("rich clipboard", "cannot write the test files");
    }
    let app = match App::start() {
        Ok(a) => a,
        Err(e) => return fail("rich clipboard", format!("desktop client: {e}")),
    };
    let want_entries = expected_entries(&note, &big, &inner);

    // 1. The desktop copies an image, text and files; the viewer sees the
    // offer and fetches each item.
    let urls = file_urls(&dir.0, &["note.txt", "big.bin", "pack"]);
    let uri_body = urls.join("\r\n") + "\r\n";
    let gnome_body = format!("copy\n{}", urls.join("\n"));
    app.offer(&[
        (PNG, png.clone()),
        (TEXT, b"hello from the desktop".to_vec()),
        (URI_LIST, uri_body.clone().into_bytes()),
        (GNOME_FILES, gnome_body.into_bytes()),
    ]);
    let seen = c
        .until(Duration::from_secs(10), |s| {
            s.offers.iter().any(|o| o.iter().any(|m| m == PNG)) && !s.offer_files.is_empty()
        })
        .await;
    if !matches!(seen, Ok(true)) {
        return fail(
            "desktop offer",
            format!(
                "offers {:?}, files {:?} ({seen:?})",
                c.seen.offers, c.seen.offer_files
            ),
        );
    }
    let types = c.seen.offers.last().cloned().unwrap_or_default();
    let has = |m: &str| types.iter().any(|t| t == m);
    if has(PNG)
        && has(TEXT)
        && !has(URI_LIST)
        && !has(GNOME_FILES)
        && entry_set(&c.seen.offer_files) == want_entries
    {
        ok(
            "desktop offer",
            format!("{types:?}, entries {:?}", entry_set(&c.seen.offer_files)),
        );
    } else {
        return fail(
            "desktop offer",
            format!(
                "types {types:?}, entries {:?}, wanted {want_entries:?}",
                entry_set(&c.seen.offer_files)
            ),
        );
    }
    let serial = c.seen.offer_serial;
    let files = c.seen.offer_files.clone();
    // Request ids: the client's are odd.
    let mut next_id = 1u32;
    let mut id = || {
        next_id += 2;
        next_id - 2
    };
    match c.fetch(id(), serial, ClipboardItem::Mime(PNG.into())).await {
        Ok(got) if got == png => ok("desktop image", format!("{} bytes intact", got.len())),
        Ok(got) => {
            healthy &= fail(
                "desktop image",
                format!("{} bytes, not the image", got.len()),
            )
        }
        Err(e) => return fail("desktop image", e),
    }
    match c
        .fetch(id(), serial, ClipboardItem::Mime(TEXT.into()))
        .await
    {
        Ok(got) if got == b"hello from the desktop" => ok("desktop text", "intact"),
        Ok(got) => healthy &= fail("desktop text", format!("{got:?}")),
        Err(e) => return fail("desktop text", e),
    }
    for (index, file) in files.iter().enumerate() {
        if file.dir {
            continue;
        }
        let want = match file.path.as_str() {
            "note.txt" => &note,
            "big.bin" => &big,
            "pack/inner.txt" => &inner,
            other => {
                healthy &= fail("desktop file", format!("unexpected entry {other}"));
                continue;
            }
        };
        match c
            .fetch(id(), serial, ClipboardItem::File(index as u32))
            .await
        {
            Ok(got) if &got == want => ok(
                "desktop file",
                format!("{} ({} bytes) intact", file.path, got.len()),
            ),
            Ok(got) => {
                healthy &= fail(
                    "desktop file",
                    format!("{}: {} bytes, wrong", file.path, got.len()),
                )
            }
            Err(e) => return fail("desktop file", format!("{}: {e}", file.path)),
        }
    }

    // 1b. A copy that offers only the GNOME file-list type (what the file
    // manager does first) is read through that type alone.
    // It names only the note and the folder, so its entries differ from the
    // first copy's and a late repeat of that one cannot pass for it.
    let gnome_only = format!(
        "copy\n{}",
        file_urls(&dir.0, &["note.txt", "pack"]).join("\n")
    );
    let mut want_gnome = want_entries.clone();
    want_gnome.retain(|(path, _, _)| path != "big.bin");
    app.offer(&[(GNOME_FILES, gnome_only.into_bytes())]);
    let seen = c
        .until(Duration::from_secs(10), |s| {
            entry_set(&s.offer_files) == want_gnome
        })
        .await;
    if matches!(seen, Ok(true)) {
        ok(
            "desktop files, gnome type only",
            format!("{:?}", entry_set(&c.seen.offer_files)),
        );
    } else {
        healthy &= fail(
            "desktop files, gnome type only",
            format!("{seen:?}, entries {:?}", entry_set(&c.seen.offer_files)),
        );
    }

    // 2. The viewer copies the same things; applications on the desktop see
    // the offer and paste each type.
    while app.selections.try_recv().is_ok() {}
    c.seen.my_serial = Some(77);
    let entries = vec![
        ClipboardFile {
            path: "note.txt".into(),
            size: note.len() as u64,
            dir: false,
        },
        ClipboardFile {
            path: "big.bin".into(),
            size: big.len() as u64,
            dir: false,
        },
        ClipboardFile {
            path: "pack".into(),
            size: 0,
            dir: true,
        },
        ClipboardFile {
            path: "pack/inner.txt".into(),
            size: inner.len() as u64,
            dir: false,
        },
    ];
    let offer = ClientMsg::ClipboardOffer {
        serial: 77,
        mime_types: vec![PNG.into(), TEXT.into()],
        files: entries,
    };
    let _ = c.wr.write_msg(&offer).await;
    let types = loop {
        match app.selections.recv_timeout(Duration::from_secs(10)) {
            Ok(t) if t.iter().any(|m| m == PNG) => break t,
            Ok(_) => continue,
            Err(_) => return fail("viewer offer", "the desktop never saw the offer"),
        }
    };
    let has = |m: &str| types.iter().any(|t| t == m);
    if has(PNG) && has(TEXT) && has(URI_LIST) && has(GNOME_FILES) {
        ok("viewer offer", format!("{types:?}"));
    } else {
        healthy &= fail("viewer offer", format!("{types:?}"));
    }

    let (png2, note2, big2, inner2) = (png.clone(), note.clone(), big.clone(), inner.clone());
    let give = move |item: &ClipboardItem| match item {
        ClipboardItem::Mime(m) if m == PNG => Some(png2.clone()),
        ClipboardItem::Mime(m) if m == TEXT => Some(b"hello from the viewer".to_vec()),
        ClipboardItem::File(0) => Some(note2.clone()),
        ClipboardItem::File(1) => Some(big2.clone()),
        ClipboardItem::File(3) => Some(inner2.clone()),
        _ => None,
    };
    for (label, mime) in [("image", PNG), ("text", TEXT)] {
        let rx = paste_in_background(&app.port, mime);
        match c.answer(&give, || paste_result(&rx)).await {
            Ok(Ok(got)) if mime == PNG && got == png => {
                ok("viewer image", format!("{} bytes intact", got.len()))
            }
            Ok(Ok(got)) if mime == TEXT && got == b"hello from the viewer" => {
                ok("viewer text", "intact")
            }
            Ok(Ok(got)) => {
                healthy &= fail(
                    &format!("viewer {label}"),
                    format!("{} bytes, wrong", got.len()),
                )
            }
            Ok(Err(e)) | Err(e) => return fail(&format!("viewer {label}"), e),
        }
    }
    // Files: the server fetches them into a spool and answers with a list of
    // paths there, the same list for both types.
    for mime in [URI_LIST, GNOME_FILES] {
        let rx = paste_in_background(&app.port, mime);
        let body = match c.answer(&give, || paste_result(&rx)).await {
            Ok(Ok(b)) => String::from_utf8_lossy(&b).into_owned(),
            Ok(Err(e)) | Err(e) => return fail("viewer files", format!("{mime}: {e}")),
        };
        let paths = gliff_proto::clipboard::parse_uri_list(&body);
        let mut names: Vec<String> = paths
            .iter()
            .filter_map(|p| p.file_name().and_then(|n| n.to_str()).map(str::to_string))
            .collect();
        names.sort();
        let mut good = names == ["big.bin", "note.txt", "pack"];
        for path in &paths {
            good &= match path.file_name().and_then(|n| n.to_str()) {
                Some("note.txt") => std::fs::read(path).ok().as_deref() == Some(&note[..]),
                Some("big.bin") => std::fs::read(path).ok().as_deref() == Some(&big[..]),
                Some("pack") => {
                    path.is_dir()
                        && std::fs::read(path.join("inner.txt")).ok().as_deref() == Some(&inner[..])
                }
                _ => false,
            };
        }
        if mime == GNOME_FILES {
            good &= body.starts_with("copy\n");
        }
        if good {
            ok(
                "viewer files",
                format!(
                    "{mime}: {names:?} intact at {:?}",
                    paths.first().and_then(|p| p.parent())
                ),
            );
        } else {
            healthy &= fail("viewer files", format!("{mime}: {body:?}"));
        }
    }
    // Let a late abort or a late bad message arrive before judging.
    match timeout(
        Duration::from_secs(5),
        c.until(Duration::from_millis(500), |_| false),
    )
    .await
    {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => healthy &= fail("clipboard protocol", format!("read failed: {e}")),
        Err(_) => healthy &= fail("clipboard protocol", "a late message never finished"),
    }
    if !c.seen.aborted.is_empty() {
        healthy &= fail(
            "clipboard protocol",
            format!("the server aborted transfer(s) {:?}", c.seen.aborted),
        );
    }
    if let Some(v) = c.seen.violations.first() {
        healthy &= fail(
            "clipboard protocol",
            format!("{} violation(s), the first: {v}", c.seen.violations.len()),
        );
    }
    healthy
}

// ---- the soak -----------------------------------------------------------

/// What `/proc` says about the server process.
#[derive(Clone, Copy, Debug)]
struct Usage {
    rss_kb: u64,
    fds: usize,
    threads: u64,
}

fn usage(pid: u32) -> Option<Usage> {
    let status = std::fs::read_to_string(format!("/proc/{pid}/status")).ok()?;
    let field = |name: &str| -> Option<u64> {
        status
            .lines()
            .find_map(|l| l.strip_prefix(name))
            .and_then(|v| v.trim().trim_end_matches("kB").trim().parse().ok())
    };
    Some(Usage {
        rss_kb: field("VmRSS:")?,
        threads: field("Threads:")?,
        fds: std::fs::read_dir(format!("/proc/{pid}/fd")).ok()?.count(),
    })
}

/// Keep the session up for `secs`, resizing every few seconds through sizes
/// and scales, and watch that every resize gets the stream it asked for and a
/// frame after it, and that the server's resources do not grow. The server is
/// sampled after every full cycle of sizes, so the samples are at the same
/// point of the same sawtooth (a 4K step costs memory that the next smaller
/// one gives back).
async fn soak(c: &mut Client, pid: u32, secs: u64) -> bool {
    const STEPS: [(u32, u32, f32); 6] = [
        (1280, 720, 1.0),
        (1920, 1080, 1.0),
        (2560, 1440, 2.0),
        (1600, 900, 1.0),
        (3840, 2160, 2.0),
        (1728, 972, 1.5),
    ];
    // Each size always comes with the same scale: Mutter keeps the scale a
    // virtual monitor first had at a size and gives it back when the size
    // returns, whatever scale is asked for then (see docs/PLAN.md).
    let mut healthy = true;
    let Some(first) = usage(pid) else {
        return fail("soak", "cannot read the server's /proc entry");
    };
    println!("soak: {secs} s, server pid {pid}, at start {first:?}");
    let start = Instant::now();
    let Some(end) = start.checked_add(Duration::from_secs(secs)) else {
        return fail("soak", "the duration is too long");
    };
    // The reference for growth: the sample after the first full cycle
    // (one-off allocations and caches are in by then). Every later sample is
    // held against it, all three counters.
    let mut baseline: Option<Usage> = None;
    let mut later = 0u32;
    let mut worst: Option<(Usage, String)> = None;
    let mut last = first;
    let (mut resizes, mut slowest, mut total_wait) = (0u32, Duration::ZERO, Duration::ZERO);
    let mut n = 0usize;
    'run: while Instant::now() < end {
        let (w, h, scale) = STEPS[n % STEPS.len()];
        n += 1;
        let configs = c.seen.configs;
        let sent = Instant::now();
        let msg = ClientMsg::Resize {
            width: w,
            height: h,
            scale,
        };
        let step = async {
            c.wr.write_msg(&msg).await.map_err(|e| e.to_string())?;
            c.until(Duration::from_secs(10), |s| {
                s.configs > configs && s.frames > s.frames_at_config
            })
            .await
        };
        match timeout(Duration::from_secs(20), step).await {
            Ok(Ok(true)) => {
                let took = sent.elapsed();
                resizes += 1;
                total_wait += took;
                slowest = slowest.max(took);
            }
            Ok(Ok(false)) => {
                healthy &= fail(
                    "soak",
                    format!("resize {n} to {w}x{h} at {scale}: no new stream and frame in 10 s"),
                );
                break;
            }
            Ok(Err(e)) => {
                healthy &= fail("soak", format!("resize {n}: {e}"));
                break;
            }
            Err(_) => {
                healthy &= fail("soak", format!("resize {n}: stuck for 20 s"));
                break;
            }
        }
        // The stream must be what was asked for: the size as asked, and the
        // scale as asked (a fractional one may be rounded by Mutter, to a
        // scale between 1 and 2 here).
        let got = (c.seen.stream, c.seen.scale_milli);
        let scale_ok = if scale.fract() == 0.0 {
            got.1 == (scale * 1000.0) as u32
        } else {
            (1000..=2000).contains(&got.1)
        };
        if got.0 != Some((w, h)) || !scale_ok {
            healthy &= fail(
                "soak",
                format!(
                    "resize {n}: asked {w}x{h} at {scale}, the stream is {:?} at scale {}",
                    got.0, got.1
                ),
            );
            break;
        }
        // Idle for a few seconds, still reading (and acknowledging) whatever
        // the server sends; a read that fails (the server went away) ends the
        // soak.
        match timeout(
            Duration::from_secs(5),
            c.until(Duration::from_secs(3), |_| false),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => {
                healthy &= fail("soak", format!("the connection ended: {e}"));
                break;
            }
            Err(_) => {
                healthy &= fail("soak", "a message never finished");
                break;
            }
        }
        // After every full cycle.
        if n.is_multiple_of(STEPS.len()) {
            let Some(u) = usage(pid) else {
                healthy &= fail("soak", "the server process is gone");
                break 'run;
            };
            println!(
                "soak: t={:>4}s  rss {:>7} kB  fds {:>3}  threads {:>3}  resizes {:>3}  frames {}",
                start.elapsed().as_secs(),
                u.rss_kb,
                u.fds,
                u.threads,
                resizes,
                c.seen.frames
            );
            last = u;
            match baseline {
                None => baseline = Some(u),
                Some(b) => {
                    later += 1;
                    let breach = growth_breach(&b, &u);
                    if let Some(what) = breach {
                        if worst.is_none() {
                            worst = Some((u, what));
                        }
                    }
                }
            }
        }
    }
    // One last look, so a server that went away during the last idle is seen.
    // Growth is judged only on the samples at the end of a full cycle: after
    // a 4K step, for one, the memory is not comparable with the baseline.
    if usage(pid).is_none() {
        healthy &= fail("soak", "the server process is gone at the end");
    }
    if resizes > 0 {
        ok(
            "soak resizes",
            format!(
                "{resizes} in {} s, each with the stream asked for and a frame; slowest {} ms, mean {} ms",
                start.elapsed().as_secs(),
                slowest.as_millis(),
                (total_wait / resizes).as_millis()
            ),
        );
    }
    match (baseline, later) {
        (Some(b), 1..) => {
            let line = format!(
                "after the first cycle {b:?}, at the end {last:?}, {later} later sample(s), one per cycle"
            );
            match worst {
                None => ok("soak resources", line),
                Some((u, what)) => {
                    healthy &= fail("soak resources", format!("{what} at {u:?}; {line}"))
                }
            }
        }
        _ => {
            healthy &= fail(
                "soak resources",
                "no verdict: the run was too short for the first cycle and at least one later one (about a minute)",
            )
        }
    }
    healthy
}

/// What, if anything, a sample has grown past the margins from the baseline:
/// memory by more than a quarter and 50 MB, open files by more than 8,
/// threads by more than 4. A tolerance, not a leak detector: a slow leak
/// under the margins needs a run of hours to show.
fn growth_breach(b: &Usage, u: &Usage) -> Option<String> {
    if u.rss_kb > b.rss_kb + b.rss_kb / 4 + 50_000 {
        Some(format!("memory {} -> {} kB", b.rss_kb, u.rss_kb))
    } else if u.fds > b.fds + 8 {
        Some(format!("open files {} -> {}", b.fds, u.fds))
    } else if u.threads > b.threads + 4 {
        Some(format!("threads {} -> {}", b.threads, u.threads))
    } else {
        None
    }
}

// ---- the load test ------------------------------------------------------

/// Clock ticks a second, as `/proc/PID/stat` counts CPU time (usually 100).
fn clock_ticks() -> f64 {
    std::process::Command::new("getconf")
        .arg("CLK_TCK")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(100.0)
}

/// CPU time the process has used, in seconds (user plus system).
fn cpu_secs(pid: u32) -> Option<f64> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name is in parentheses and may hold spaces: count from the
    // closing one. Fields 14 and 15 are the user and system ticks.
    let after = &stat[stat.rfind(')')? + 2..];
    let f: Vec<&str> = after.split_whitespace().collect();
    let ticks: u64 = f.get(11)?.parse::<u64>().ok()? + f.get(12)?.parse::<u64>().ok()?;
    Some(ticks as f64 / clock_ticks())
}

/// What the frames logged since `from` add up to.
struct Rate {
    frames: usize,
    per_second: f64,
    mean_bytes: usize,
    max_gap_ms: u128,
    keyframes: usize,
}

/// `begin` and `end` bound the window: the wait for the first frame and the
/// silence after the last one count as gaps too.
fn rate(
    log: &[(Instant, u64, usize, bool)],
    from: usize,
    window: Duration,
    begin: Instant,
    end: Instant,
) -> Rate {
    let part = &log[from.min(log.len())..];
    let inner = part
        .windows(2)
        .map(|w| w[1].0.duration_since(w[0].0))
        .max()
        .unwrap_or(Duration::ZERO);
    let edges = match (part.first(), part.last()) {
        (Some(first), Some(last)) => first
            .0
            .duration_since(begin)
            .max(end.saturating_duration_since(last.0)),
        _ => window,
    };
    let max_gap = inner.max(edges);
    Rate {
        frames: part.len(),
        per_second: part.len() as f64 / window.as_secs_f64(),
        mean_bytes: part.iter().map(|f| f.2).sum::<usize>() / part.len().max(1),
        max_gap_ms: max_gap.as_millis(),
        keyframes: part.iter().filter(|f| f.3).count(),
    }
}

/// Read for `dur`, each read bounded as a whole (a half-sent payload or a
/// blocked acknowledgement ends it with an error).
async fn read_for(c: &mut Client, dur: Duration) -> Result<(), String> {
    let end = Instant::now() + dur;
    while Instant::now() < end {
        match timeout(Duration::from_secs(5), c.step(Duration::from_millis(100))).await {
            Ok(Ok(_)) => {}
            Ok(Err(e)) => return Err(e),
            Err(_) => return Err("a message never finished (5 s)".into()),
        }
    }
    Ok(())
}

async fn load_test(c: &mut Client, pid: u32, secs: u64, stall: Option<u64>) -> bool {
    let healthy = load_test_inner(c, pid, secs, stall).await;
    c.seen.frame_log = None;
    healthy
}

async fn load_test_inner(c: &mut Client, pid: u32, secs: u64, stall: Option<u64>) -> bool {
    let mut healthy = true;
    let (Some(rss0), Some(cpu0)) = (usage(pid), cpu_secs(pid)) else {
        return fail("load", "cannot read the server's /proc entry");
    };
    c.seen.frame_log = Some(Vec::new());
    let started = Instant::now();
    if let Err(e) = read_for(c, Duration::from_secs(secs)).await {
        return fail("load", format!("read failed: {e}"));
    }
    let ended = Instant::now();
    let window = ended.duration_since(started);
    let log = c.seen.frame_log.take().unwrap_or_default();
    // Frames per second in slices of ten seconds: the server starts at a low
    // frame rate and raises it as the link proves itself.
    let slices: Vec<String> = (0..window.as_secs() / 10)
        .map(|i| {
            let (lo, hi) = (
                Duration::from_secs(i * 10),
                Duration::from_secs(i * 10 + 10),
            );
            let n = log
                .iter()
                .filter(|f| {
                    let t = f.0.duration_since(started);
                    t >= lo && t < hi
                })
                .count();
            format!("{:.0}", n as f64 / 10.0)
        })
        .collect();
    println!(
        "load: frames per second by ten seconds: {}",
        slices.join(", ")
    );
    let r = rate(&log, 0, window, started, ended);
    let cpu = cpu_secs(pid).unwrap_or(cpu0) - cpu0;
    let rss1 = usage(pid).unwrap_or(rss0);
    let line = format!(
        "{} frames in {:.0} s = {:.1} per second, {} bytes a frame ({:.1} Mbit/s), longest gap {} ms, {} key frame(s); server CPU {:.0}% of one core, memory {} -> {} kB",
        r.frames,
        window.as_secs_f64(),
        r.per_second,
        r.mean_bytes,
        r.per_second * r.mean_bytes as f64 * 8.0 / 1e6,
        r.max_gap_ms,
        r.keyframes,
        cpu / window.as_secs_f64() * 100.0,
        rss0.rss_kb,
        rss1.rss_kb
    );
    // A changing picture must give a steady stream: not a few frames a second
    // (the animation is not running, or the server cannot keep up), and no
    // pause of a second.
    if r.per_second >= 20.0 && r.max_gap_ms < 1000 {
        ok("load", line);
    } else {
        healthy &= fail(
            "load",
            format!("{line} (wanted at least 20 per second and no gap of a second)"),
        );
    }

    let Some(stall) = stall else {
        return healthy;
    };
    // The client stops reading and acknowledging, as a slow link would. What
    // is measured is the server's memory at samples during the stall (bounded
    // by whatever combination of unacknowledged-byte limit, blocked writer and
    // replaced capture it uses; this does not say which) and that frames come
    // promptly when the client reads again. It cannot say whether the first
    // frames are fresh: some come from the pipe's buffer.
    let before = usage(pid).unwrap_or(rss1);
    let last_id = c.seen.last_frame_id;
    println!("load: not reading for {stall} s");
    let stalled = Instant::now();
    let mut peak = before.rss_kb;
    while stalled.elapsed() < Duration::from_secs(stall) {
        tokio::time::sleep(Duration::from_secs(2)).await;
        if let Some(u) = usage(pid) {
            peak = peak.max(u.rss_kb);
        } else {
            return fail("stall", "the server process is gone");
        }
    }
    c.seen.frame_log = Some(Vec::new());
    let resumed = Instant::now();
    let resume_window = Duration::from_secs(10);
    if let Err(e) = read_for(c, resume_window).await {
        return fail("stall", format!("read failed after the stall: {e}"));
    }
    let resumed_end = Instant::now();
    let log = c.seen.frame_log.take().unwrap_or_default();
    let first_after = log.first().map(|f| f.0.duration_since(resumed));
    let first_two: Vec<_> = log
        .iter()
        .take_while(|f| f.0.duration_since(resumed) < Duration::from_secs(2))
        .collect();
    let all = rate(&log, 0, resume_window, resumed, resumed_end);
    let jump = log.first().map(|f| f.1.saturating_sub(last_id));
    let line = format!(
        "after {stall} s: memory peaked at {peak} kB (was {} kB); first frame {:?} after reading again, frame id jumped by {:?}; {} frames in the first 2 s, {:.1} per second over 10 s",
        before.rss_kb,
        first_after.map(|d| d.as_millis()),
        jump,
        first_two.len(),
        all.per_second
    );
    // Memory: no more than 100 MB over where it was at the samples. Resume: a
    // frame within 3 s, and the first two seconds not a replay of a long
    // stall's frames (a backlog would be hundreds).
    let mem_ok = peak <= before.rss_kb + 100_000;
    let resumed_ok = first_after.is_some_and(|d| d < Duration::from_secs(3));
    let backlog_ok = first_two.len() <= 240;
    if mem_ok && resumed_ok && backlog_ok && all.per_second >= 20.0 {
        ok("stall", line);
    } else {
        healthy &= fail("stall", line);
    }
    healthy
}
