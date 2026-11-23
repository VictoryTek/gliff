//! Capture a GNOME monitor as dmabufs and check the GPU path end to end:
//! what Mutter chose, that the buffers import and encode on the GPU, that the
//! pool does not run dry (frames keep coming while each is released), and that
//! the picture is right: the H.264 the GPU makes from the dmabuf is decoded on
//! the CPU and compared, block by block, with a plain shared-memory capture of
//! the same monitor.
//!
//! `gliff-gnome-dmabuf-probe [--render-node PATH] [--connector NAME] [--secs N] [--move] [--virtual]`
//!
//! `--virtual` captures a virtual monitor instead (what `--headless` serves),
//! skips the picture comparison, and resizes the monitor through three sizes,
//! checking after each that Mutter's new buffers are negotiated, imported and
//! encoded.
//!
//! `--move` records the cursor into the picture and moves it in circles while
//! the GPU capture runs, so the screen changes all the time: Mutter then
//! sends a stream of buffers, which is what exercises holding and returning
//! them. The pointer is back where it was not guaranteed; do not use the
//! machine meanwhile.
//!
//! One line per check, `[ OK ]` or `[FAIL]`. The screen should be awake and
//! not changing much while it runs (the comparison measures a shared-memory
//! capture against itself first, to see how much a quiet screen moves).

use std::os::fd::AsFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use drm_fourcc::DrmFourcc;
use gliff_backend::InputCmd;
use gliff_vk::{DmabufPlane, Encoder, EncoderSettings, Gpu};
use gnome_capture::{CaptureEvent, Capturer, Options};
use gnome_input::Input;
use gnome_ipc::power::{self, IdleInhibitor};
use gnome_ipc::{discover, mutter, CursorMode, Session, StreamSource};

const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;
/// The size a virtual monitor is asked for first.
const FIRST_SIZE: (u32, u32) = (1920, 1080);
/// Block edge for the picture comparison.
const BLOCK: usize = 16;

fn ok(name: &str, detail: impl std::fmt::Display) {
    println!("[ OK ] {name}: {detail}");
}

fn fail(name: &str, detail: impl std::fmt::Display) -> bool {
    println!("[FAIL] {name}: {detail}");
    false
}

struct Args {
    render_node: String,
    connector: Option<String>,
    secs: u64,
    moving: bool,
    virtual_monitor: bool,
}

fn parse() -> Result<Args> {
    let mut a = Args {
        render_node: "/dev/dri/renderD128".into(),
        connector: None,
        secs: 5,
        moving: false,
        virtual_monitor: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--render-node" => a.render_node = it.next().context("--render-node needs a path")?,
            "--move" => a.moving = true,
            "--virtual" => a.virtual_monitor = true,
            "--connector" => a.connector = Some(it.next().context("--connector needs a name")?),
            "--secs" => {
                a.secs = it
                    .next()
                    .and_then(|s| s.parse().ok())
                    .context("--secs needs a number")?
            }
            other => bail!("unknown argument {other}"),
        }
    }
    Ok(a)
}

/// A recorded monitor: the session must stay alive while it is captured.
struct Source {
    session: Session,
    /// Keeps the screen from blanking while the probe runs.
    _inhibitor: IdleInhibitor,
    stream_path: String,
    extent: (u32, u32),
    node: u32,
    runtime_dir: std::path::PathBuf,
}

fn open_source(args: &Args, cursor: CursorMode) -> Result<Source> {
    let desktop = discover::find()?;
    let conn = mutter::connect(&desktop.bus_address)?;
    let monitors = mutter::monitors(&conn)?;
    // A blanked screen refuses sessions: wake it, and keep it awake.
    power::wake(&conn)?;
    let inhibitor = IdleInhibitor::new(&desktop.bus_address, "gliff dmabuf probe")?;
    let session = Session::create(&desktop.bus_address)?;
    let (source, extent) = if args.virtual_monitor {
        (StreamSource::Virtual, FIRST_SIZE)
    } else {
        let monitor = match &args.connector {
            Some(c) => monitors.iter().find(|m| &m.connector == c),
            None => monitors.iter().find(|m| m.primary).or(monitors.first()),
        }
        .ok_or_else(|| anyhow!("no monitor to record"))?;
        (
            StreamSource::Monitor(monitor.connector.clone()),
            (monitor.width, monitor.height),
        )
    };
    let stream = session.record(&source, cursor)?;
    session.start()?;
    let node = stream.wait_node(Duration::from_secs(5))?;
    Ok(Source {
        session,
        _inhibitor: inhibitor,
        stream_path: stream.path().to_string(),
        extent,
        node,
        runtime_dir: desktop.runtime_dir,
    })
}

fn capture(
    source: &Source,
    dmabuf: Vec<(DrmFourcc, Vec<u64>)>,
    size: Option<(u32, u32)>,
) -> Result<(Capturer, mpsc::Receiver<CaptureEvent>)> {
    let (tx, rx) = mpsc::channel();
    let mut options = Options::new(source.node, &source.runtime_dir, "probe");
    options.dmabuf = dmabuf;
    options.size = size;
    // A size means a virtual monitor, which must not be offered a limit of 0.
    options.unlimited_rate = size.is_none();
    let cap = Capturer::start(
        options,
        Box::new(move |ev| {
            let _ = tx.send(ev);
        }),
    )?;
    Ok((cap, rx))
}

/// Block means of an image's colour channels, `BLOCK`x`BLOCK` pixels each.
fn block_means(bgra: &[u8], width: usize, height: usize) -> Vec<f64> {
    let (bw, bh) = (width / BLOCK, height / BLOCK);
    let mut out = Vec::with_capacity(bw * bh);
    for by in 0..bh {
        for bx in 0..bw {
            let mut sum = 0u64;
            for y in by * BLOCK..(by + 1) * BLOCK {
                let row = &bgra[(y * width + bx * BLOCK) * 4..][..BLOCK * 4];
                sum += row
                    .chunks_exact(4)
                    .map(|p| u64::from(p[0]) + u64::from(p[1]) + u64::from(p[2]))
                    .sum::<u64>();
            }
            out.push(sum as f64 / (BLOCK * BLOCK * 3) as f64);
        }
    }
    out
}

/// Mean absolute difference of two block-mean lists, and the share of blocks
/// that differ by more than 16 levels.
fn compare(a: &[f64], b: &[f64]) -> (f64, f64) {
    let n = a.len().min(b.len()).max(1) as f64;
    let diffs: Vec<f64> = a.iter().zip(b).map(|(x, y)| (x - y).abs()).collect();
    (
        diffs.iter().sum::<f64>() / n,
        diffs.iter().filter(|d| **d > 16.0).count() as f64 / n,
    )
}

/// One shared-memory frame of the monitor as packed BGRA.
fn shm_frame(source: &Source) -> Result<(usize, usize, Vec<u8>)> {
    let (cap, rx) = capture(source, Vec::new(), None)?;
    cap.request_frame()?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if let Ok(CaptureEvent::Frame(frame)) = rx.recv_timeout(Duration::from_millis(200)) {
            let img = frame.read_bgra()?;
            return Ok((img.width, img.height, img.pixels));
        }
    }
    bail!("no shared-memory frame in 5 s")
}

/// The GPU encoder (and a CPU decoder to look at what it made), rebuilt when
/// the stream's size changes, as the server does.
struct GpuSide {
    gpu: Arc<Gpu>,
    encoder: Option<Encoder>,
    decoder: Option<gliff_sw::Decoder>,
}

/// What one run of [`drive`] saw.
#[derive(Default)]
struct Outcome {
    frames: u32,
    encoded: u32,
    encode_ms: f64,
    /// Ready events seen, with the last one's size and modifier.
    readies: u32,
    size: (u32, u32),
    modifier: u64,
    /// The first frame, GPU-encoded and CPU-decoded.
    picture: Option<Vec<u8>>,
}

/// Encode every frame the capture delivers for `secs`, asking for the next
/// one as each is released.
fn drive(
    side: &mut GpuSide,
    cap: &Capturer,
    rx: &mpsc::Receiver<CaptureEvent>,
    secs: f64,
    want_picture: bool,
) -> Result<Outcome, String> {
    let deadline = Instant::now() + Duration::from_secs_f64(secs);
    let mut out = Outcome::default();
    cap.request_frame().map_err(|e| e.to_string())?;
    while Instant::now() < deadline {
        let Ok(ev) = rx.recv_timeout(Duration::from_millis(500)) else {
            // A quiet screen sends nothing; send the last frame again.
            let _ = cap.recapture();
            continue;
        };
        match ev {
            CaptureEvent::Ready {
                width,
                height,
                modifier,
                ..
            } => {
                if modifier == MOD_INVALID {
                    return Err(format!("{width}x{height} came as shared memory"));
                }
                out.readies += 1;
                out.size = (width, height);
                out.modifier = modifier;
                // A new size is a new encoder.
                side.encoder = None;
            }
            CaptureEvent::Frame(frame) if frame.buffer.is_shm() => {
                return Err("a frame came as shared memory".into());
            }
            CaptureEvent::Frame(frame) => {
                out.frames += 1;
                let info = &frame.buffer.info;
                let (w, h) = (info.width & !1, info.height & !1);
                let first = side.encoder.is_none();
                if first {
                    let settings = EncoderSettings {
                        width: w,
                        height: h,
                        bitrate: 4 * EncoderSettings::default_bitrate(w, h, 60),
                        framerate: 60,
                        vbv_ms: EncoderSettings::DEFAULT_VBV_MS,
                    };
                    side.encoder = Some(
                        Encoder::new(&side.gpu, settings, false)
                            .map_err(|e| format!("create the encoder: {e}"))?,
                    );
                    side.decoder =
                        Some(gliff_sw::Decoder::new(false, w, h).map_err(|e| e.to_string())?);
                }
                let fourcc = DrmFourcc::try_from(info.fourcc)
                    .map_err(|_| format!("fourcc {:#x}", info.fourcc))?;
                let plane = DmabufPlane {
                    fd: info.fd.as_fd(),
                    width: info.width,
                    height: info.height,
                    offset: info.planes[0].offset,
                    stride: info.planes[0].stride,
                    fourcc,
                    modifier: info.modifier,
                };
                let key = (frame.buffer.generation << 32) | frame.buffer.index as u64;
                let t0 = Instant::now();
                let encoded = side
                    .encoder
                    .as_mut()
                    .expect("made above")
                    .encode_dmabuf_transformed(key, &plane, frame.transform, first)
                    .map_err(|e| format!("encode frame {}: {e}", out.frames))?;
                out.encode_ms += t0.elapsed().as_secs_f64() * 1000.0;
                out.encoded += 1;
                if want_picture && out.picture.is_none() {
                    let dec = side.decoder.as_mut().expect("made above");
                    let mut pic = dec.decode(&encoded.main, &[]).map_err(|e| e.to_string())?;
                    if pic.is_none() {
                        pic = dec.flush().map_err(|e| e.to_string())?;
                    }
                    out.picture = pic.map(|p| p.pixels);
                }
                drop(frame);
                let _ = cap.request_frame();
            }
            CaptureEvent::Error(e) => return Err(e),
            _ => {}
        }
    }
    Ok(out)
}

/// After `use_shm`: the stream comes back as shared memory, and a frame of it
/// can be read by the CPU.
fn check_shm(cap: &Capturer, rx: &mpsc::Receiver<CaptureEvent>) -> bool {
    // Use up the pending request first, as the server's is when its first GPU
    // encode fails: after `use_shm` a frame must arrive without asking.
    let _ = cap.request_frame();
    let used = Instant::now() + Duration::from_secs(3);
    while Instant::now() < used {
        if let Ok(CaptureEvent::Frame(_)) = rx.recv_timeout(Duration::from_millis(200)) {
            break;
        }
    }
    if let Err(e) = cap.use_shm() {
        return fail("use_shm", e);
    }
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut ready = false;
    while Instant::now() < deadline {
        match rx.recv_timeout(Duration::from_millis(300)) {
            Ok(CaptureEvent::Ready { modifier, .. }) => ready = modifier == MOD_INVALID,
            Ok(CaptureEvent::Frame(frame)) if ready && frame.buffer.is_shm() => {
                return match frame.read_bgra() {
                    Ok(img) => {
                        ok(
                            "use_shm",
                            format!(
                                "back to shared memory, {}x{} read by the CPU",
                                img.width, img.height
                            ),
                        );
                        true
                    }
                    Err(e) => fail(
                        "use_shm",
                        format!("a shared-memory frame did not read: {e}"),
                    ),
                };
            }
            Ok(CaptureEvent::Error(e)) => return fail("use_shm", e),
            // Dmabuf frames still in flight.
            Ok(_) | Err(_) => {}
        }
    }
    fail("use_shm", "no shared-memory frame in 5 s")
}

fn run(args: &Args) -> Result<bool> {
    let mut healthy = true;
    let node = std::path::Path::new(&args.render_node);
    let gpu = Gpu::open(Some(node)).map_err(|e| anyhow!("open the GPU on {node:?}: {e}"))?;
    let offer: Vec<(DrmFourcc, Vec<u64>)> = [DrmFourcc::Xrgb8888, DrmFourcc::Argb8888]
        .into_iter()
        .map(|f| (f, gpu.dmabuf_modifiers(f)))
        .collect();
    for (f, m) in &offer {
        ok(
            "vulkan modifiers",
            format!("{f:?}: {} importable ({m:x?})", m.len()),
        );
    }

    let cursor = if args.moving {
        CursorMode::Embedded
    } else {
        CursorMode::Hidden
    };
    let source = open_source(args, cursor)?;
    let size = args.virtual_monitor.then_some(FIRST_SIZE);
    let (cap, rx) = capture(&source, offer, size)?;
    ok("connect", "PipeWire stream connected");
    // The pointer goes in circles for as long as `going` is set.
    let going = Arc::new(AtomicBool::new(args.moving));
    let mover = args.moving.then(|| {
        let input = Input::new(
            source.session.input(),
            source.stream_path.clone(),
            source.extent,
        );
        let (cx, cy) = (
            f64::from(source.extent.0) / 2.0,
            f64::from(source.extent.1) / 2.0,
        );
        let going = going.clone();
        std::thread::spawn(move || {
            let mut t = 0.0f64;
            while going.load(Ordering::Relaxed) {
                t += 0.05;
                let _ = input.send(InputCmd::Motion {
                    x: cx + 300.0 * t.cos(),
                    y: cy + 300.0 * t.sin(),
                });
                std::thread::sleep(Duration::from_millis(8));
            }
        })
    });

    let mut side = GpuSide {
        gpu,
        encoder: None,
        decoder: None,
    };
    let begin = Instant::now();
    let first = drive(
        &mut side,
        &cap,
        &rx,
        args.secs as f64,
        !args.virtual_monitor,
    );
    going.store(false, Ordering::Relaxed);
    if let Some(m) = mover {
        let _ = m.join();
    }
    let first = match first {
        Ok(o) => o,
        Err(e) => return Ok(fail("capture", e)),
    };
    ok(
        "negotiated",
        format!(
            "{}x{}, dmabuf, modifier {:#x} ({} format announcement(s))",
            first.size.0, first.size.1, first.modifier, first.readies
        ),
    );
    if first.encoded == 0 {
        return Ok(fail("frames", "none arrived"));
    }
    let secs = begin.elapsed().as_secs_f64();
    let stats = cap.stats();
    ok(
        "fixation",
        format!(
            "Mutter handed the modifier choice back {} time(s)",
            gnome_capture::Stats::get(&stats.fixations)
        ),
    );
    ok(
        "frames",
        format!(
            "{} frames in {secs:.1} s, {} encoded on the GPU ({:.1} ms each); PipeWire delivered {} buffers, {} without a picture, {} dropped",
            first.frames,
            first.encoded,
            first.encode_ms / f64::from(first.encoded),
            gnome_capture::Stats::get(&stats.buffers),
            gnome_capture::Stats::get(&stats.empty),
            gnome_capture::Stats::get(&stats.dropped),
        ),
    );

    if args.virtual_monitor {
        for (w, h) in [(1280, 720), (1600, 900), (1920, 1080)] {
            cap.resize(w, h, 1.0)?;
            match drive(&mut side, &cap, &rx, 3.0, false) {
                Ok(o) if o.size == (w, h) && o.encoded > 0 => ok(
                    "resize",
                    format!(
                        "{w}x{h}: modifier {:#x}, {} frames encoded on the GPU",
                        o.modifier, o.encoded
                    ),
                ),
                Ok(o) => {
                    healthy &= fail(
                        "resize",
                        format!(
                            "asked for {w}x{h}: stream is {:?}, {} frames encoded",
                            o.size, o.encoded
                        ),
                    )
                }
                Err(e) => healthy &= fail("resize", format!("{w}x{h}: {e}")),
            }
        }
        // The choice stays across a resize: asking for a new size now gives
        // shared memory.
        healthy &= check_shm(&cap, &rx);
        cap.resize(1280, 720, 1.0)?;
        let after = drive(&mut side, &cap, &rx, 2.0, false);
        match after {
            Err(e) if e.contains("shared memory") => {
                ok("use_shm", "a resize afterwards stays shared memory")
            }
            other => {
                healthy &= fail(
                    "use_shm",
                    format!(
                        "after a resize the stream should be shared memory; got {}",
                        match other {
                            Ok(o) => format!("{} dmabuf frames", o.encoded),
                            Err(e) => e,
                        }
                    ),
                )
            }
        }
        return Ok(healthy);
    }
    healthy &= check_shm(&cap, &rx);
    drop(cap);

    // The picture: the GPU's frame against two plain captures.
    let Some(gpu_pixels) = first.picture else {
        return Ok(fail("picture", "the decoder produced no picture"));
    };
    let size = (first.size.0 as usize & !1, first.size.1 as usize & !1);
    let (w1, h1, shm1) = shm_frame(&source)?;
    std::thread::sleep(Duration::from_millis(700));
    let (w2, h2, shm2) = shm_frame(&source)?;
    if (w1, h1) != size || (w2, h2) != size {
        return Ok(fail(
            "picture",
            format!("sizes differ: gpu {size:?}, shm {w1}x{h1} and {w2}x{h2}"),
        ));
    }
    let g = block_means(&gpu_pixels, size.0, size.1);
    let a = block_means(&shm1, size.0, size.1);
    let b = block_means(&shm2, size.0, size.1);
    let (noise, _) = compare(&a, &b);
    let (diff, far) = compare(&g, &a);
    let detail = format!(
        "mean block difference gpu vs shm {diff:.2} levels, {:.1}% of blocks off by more than 16; two shm captures differ by {noise:.2}",
        far * 100.0
    );
    if diff <= noise + 4.0 && far < 0.02 {
        ok("picture", detail);
    } else {
        healthy &= fail("picture", detail);
    }
    Ok(healthy)
}

fn main() {
    let args = match parse() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("{e:#}");
            std::process::exit(2);
        }
    };
    match run(&args) {
        Ok(true) => println!("PASS"),
        Ok(false) => {
            println!("FAIL");
            std::process::exit(1);
        }
        Err(e) => {
            println!("[FAIL] {e:#}");
            println!("FAIL");
            std::process::exit(1);
        }
    }
}
