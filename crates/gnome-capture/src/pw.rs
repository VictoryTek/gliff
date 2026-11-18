//! The PipeWire side: negotiate a raw video format, copy frames into the
//! ring and feed the [`Pacer`].

use std::cell::RefCell;
use std::fs::File;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::os::unix::fs::FileExt;
use std::os::unix::net::UnixStream;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::time::Instant;

use drm_fourcc::DrmFourcc;
use hypr_capture::{CaptureBuffer, CaptureEvent, EventSink, FrameSource, OutputInfo, Rect};
use pipewire as pw;
use pw::spa;
use pw::spa::buffer::meta::MetaCursor;
use pw::spa::param::video::{VideoFormat, VideoInfoRaw};
use pw::spa::pod::{Object, Pod, Property, Value};
use pw::spa::sys as spa_sys;
use rustix::fs::{memfd_create, MemfdFlags};

use crate::cursor::{CursorTracker, Sprite};
use crate::pacer::Pacer;
use crate::{Cmd, Options, Stats};

/// The 32-bit layouts we accept, with the DRM format the consumer is told.
fn drm_format(f: VideoFormat) -> Option<DrmFourcc> {
    match f {
        VideoFormat::BGRx => Some(DrmFourcc::Xrgb8888),
        VideoFormat::BGRA => Some(DrmFourcc::Argb8888),
        VideoFormat::RGBx => Some(DrmFourcc::Xbgr8888),
        VideoFormat::RGBA => Some(DrmFourcc::Abgr8888),
        _ => None,
    }
}

/// Room for a 384x384 cursor bitmap, the biggest we ask for.
fn cursor_meta_size(side: u32) -> i32 {
    (std::mem::size_of::<spa_sys::spa_meta_cursor>()
        + std::mem::size_of::<spa_sys::spa_meta_bitmap>()
        + (side * side * 4) as usize) as i32
}

fn serialize(obj: Object) -> Vec<u8> {
    spa::pod::serialize::PodSerializer::serialize(Cursor::new(Vec::new()), &Value::Object(obj))
        .expect("serialize pod")
        .0
        .into_inner()
}

fn enum_format() -> Vec<u8> {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};

    let formats = [
        VideoFormat::BGRx,
        VideoFormat::BGRA,
        VideoFormat::RGBx,
        VideoFormat::RGBA,
    ];
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                F::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                F::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(
                F::VideoFormat.as_raw(),
                Value::Choice(ChoiceValue::Id(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Enum {
                        default: Id(formats[0].as_raw()),
                        alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
                    },
                ))),
            ),
            Property::new(
                F::VideoSize.as_raw(),
                Value::Choice(ChoiceValue::Rectangle(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Rectangle {
                            width: 1920,
                            height: 1080,
                        },
                        min: Rectangle {
                            width: 1,
                            height: 1,
                        },
                        max: Rectangle {
                            width: 16384,
                            height: 16384,
                        },
                    },
                ))),
            ),
            Property::new(
                F::VideoFramerate.as_raw(),
                Value::Choice(ChoiceValue::Fraction(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: Fraction { num: 60, denom: 1 },
                        min: Fraction { num: 0, denom: 1 },
                        max: Fraction { num: 240, denom: 1 },
                    },
                ))),
            ),
        ],
    })
}

/// Buffers in shared memory only: the CPU path reads them.
fn buffers_param() -> Vec<u8> {
    let types = (1 << spa_sys::SPA_DATA_MemFd) | (1 << spa_sys::SPA_DATA_MemPtr);
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![Property::new(
            spa_sys::SPA_PARAM_BUFFERS_dataType,
            Value::Int(types),
        )],
    })
}

/// Ask for cursor metadata, in a size range the way PipeWire's own examples
/// do: a fixed size does not always match the producer's range.
fn cursor_meta_param() -> Vec<u8> {
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa_sys::SPA_PARAM_META_type,
                Value::Id(spa::utils::Id(spa_sys::SPA_META_Cursor)),
            ),
            Property::new(
                spa_sys::SPA_PARAM_META_size,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: cursor_meta_size(64),
                        min: cursor_meta_size(1),
                        max: cursor_meta_size(384),
                    },
                ))),
            ),
        ],
    })
}

struct State {
    sink: EventSink,
    frames: FrameSource,
    pacer: Pacer,
    options: Options,
    stats: Arc<Stats>,
    started: Instant,
    mainloop: pw::main_loop::MainLoopRc,
    /// The ring of the current format; `files[i]` is what `ring[i]` reads.
    ring: Vec<Arc<CaptureBuffer>>,
    files: Vec<File>,
    generation: u64,
    size: (u32, u32),
    fourcc: u32,
    sequence: u64,
    cursor: CursorTracker,
}

impl State {
    fn emit(&mut self, event: CaptureEvent) {
        (self.sink)(event);
    }

    fn fail(&mut self, message: String) {
        self.emit(CaptureEvent::Error(message));
        self.mainloop.quit();
    }

    /// A new format: a fresh ring, and the consumer learns the size.
    fn reconfigure(&mut self, size: (u32, u32), fourcc: u32) -> Result<(), String> {
        let (w, h) = size;
        if let Some(limit) = self.options.max_size {
            if w > limit.side || h > limit.side || u64::from(w) * u64::from(h) > limit.pixels {
                return Err(format!(
                    "the stream is {w}x{h}, past the {}-pixel-side, {}-pixel limit",
                    limit.side, limit.pixels
                ));
            }
        }
        let stride = w.checked_mul(4).ok_or("stream too wide")?;
        let len = u64::from(stride) * u64::from(h);
        self.generation += 1;
        let (mut ring, mut files) = (Vec::new(), Vec::new());
        for index in 0..self.options.buffers.max(1) + 1 {
            let fd = memfd_create(c"gliff-gnome", MemfdFlags::CLOEXEC)
                .map_err(|e| format!("memfd_create: {e}"))?;
            let file = File::from(fd);
            file.set_len(len)
                .map_err(|e| format!("size a buffer: {e}"))?;
            let reader = file.try_clone().map_err(|e| e.to_string())?;
            let buffer =
                CaptureBuffer::from_memfd(index, self.generation, size, fourcc, stride, reader)
                    .map_err(|e| e.to_string())?;
            ring.push(Arc::new(buffer));
            files.push(file);
        }
        self.pacer.reset(ring.len());
        self.ring = ring;
        self.files = files;
        self.size = size;
        self.fourcc = fourcc;
        let output = OutputInfo {
            name: self.options.name.clone(),
            description: "GNOME screen cast".into(),
            x: 0,
            y: 0,
            width: w as i32,
            height: h as i32,
            scale: 1,
            transform: 0,
            refresh_mhz: 0,
            done: true,
        };
        self.emit(CaptureEvent::Ready {
            output,
            width: w,
            height: h,
            fourcc,
            modifier: 0x00ff_ffff_ffff_ffff,
        });
        Ok(())
    }

    fn send(&mut self, slot: usize) {
        let (w, h) = self.size;
        let damage = vec![Rect {
            x: 0,
            y: 0,
            width: w as i32,
            height: h as i32,
        }];
        self.sequence += 1;
        let frame = self.frames.frame(
            self.ring[slot].clone(),
            damage,
            self.started.elapsed().as_nanos() as u64,
            self.sequence,
        );
        self.emit(CaptureEvent::Frame(frame));
    }

    fn command(&mut self, cmd: Cmd) {
        match cmd {
            Cmd::Request => {
                if let Some(s) = self.pacer.request() {
                    self.send(s);
                }
            }
            Cmd::Recapture => {
                if let Some(s) = self.pacer.recapture() {
                    self.send(s);
                }
            }
            Cmd::Release { index, generation } => {
                if generation == self.generation {
                    self.pacer.release(index);
                }
            }
            Cmd::Stop => self.mainloop.quit(),
        }
    }

    /// Copy one PipeWire frame into a free slot.
    fn ingest(&mut self, bytes: &[u8], stride: usize) {
        let (w, h) = (self.size.0 as usize, self.size.1 as usize);
        let row = w * 4;
        if self.ring.is_empty() || stride < row || bytes.len() < stride * (h - 1) + row {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        let Some(slot) = self.pacer.acquire() else {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let file = &self.files[slot];
        let written = if stride == row {
            file.write_all_at(&bytes[..row * h], 0)
        } else {
            (0..h).try_for_each(|y| {
                file.write_all_at(&bytes[y * stride..y * stride + row], (y * row) as u64)
            })
        };
        if written.is_err() {
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if let Some(s) = self.pacer.filled(slot) {
            self.send(s);
        }
    }

    fn cursor(&mut self, meta: &MetaCursor) {
        // An id of 0 means this buffer carries no cursor data.
        if meta.id() == 0 {
            return;
        }
        self.stats.cursor_meta.fetch_add(1, Ordering::Relaxed);
        let sprite = match meta.bitmap() {
            None => Sprite::Unchanged,
            Some(bm) if bm.size().width == 0 || bm.size().height == 0 => Sprite::Hidden,
            Some(bm) => match bitmap_argb(bm) {
                Some(argb) => {
                    let (size, hot) = (bm.size(), meta.hotspot());
                    Sprite::Shape {
                        width: size.width,
                        height: size.height,
                        hot_x: hot.x,
                        hot_y: hot.y,
                        argb,
                    }
                }
                None => Sprite::Unchanged,
            },
        };
        let p = meta.position();
        for event in self.cursor.update((p.x, p.y), sprite) {
            self.emit(event);
        }
    }
}

/// A cursor bitmap as tightly packed ARGB8888 (BGRA bytes).
fn bitmap_argb(bm: &spa::buffer::meta::MetaBitmap) -> Option<Vec<u8>> {
    let data = bm.bitmap_data()?;
    let size = bm.size();
    let (w, h) = (size.width as usize, size.height as usize);
    let stride = bm.stride().unsigned_abs() as usize;
    let swap = match bm.format() {
        VideoFormat::BGRA | VideoFormat::BGRx => false,
        VideoFormat::RGBA | VideoFormat::RGBx => true,
        _ => return None,
    };
    if stride < w * 4 || data.len() < stride * h {
        return None;
    }
    let mut out = Vec::with_capacity(w * h * 4);
    for y in 0..h {
        for px in data[y * stride..y * stride + w * 4].chunks_exact(4) {
            out.extend_from_slice(&if swap {
                [px[2], px[1], px[0], px[3]]
            } else {
                [px[0], px[1], px[2], px[3]]
            });
        }
    }
    Some(out)
}

/// The capture thread. Reports whether it connected on `ready`, then runs
/// until told to stop or the stream fails.
pub(crate) fn run(
    options: Options,
    sink: EventSink,
    commands: pw::channel::Receiver<Cmd>,
    command_tx: pw::channel::Sender<Cmd>,
    stats: Arc<Stats>,
    ready: Sender<Result<(), String>>,
) {
    if let Err(e) = run_inner(options, sink, commands, command_tx, stats, &ready) {
        // A failure before the stream connected is the caller's error; the
        // channel is gone once it connected, which the send ignores.
        let _ = ready.send(Err(e));
    }
}

fn run_inner(
    options: Options,
    sink: EventSink,
    commands: pw::channel::Receiver<Cmd>,
    command_tx: pw::channel::Sender<Cmd>,
    stats: Arc<Stats>,
    ready: &Sender<Result<(), String>>,
) -> Result<(), String> {
    let err = |what: &str| {
        let what = what.to_string();
        move |e: &dyn std::fmt::Display| format!("{what}: {e}")
    };
    let socket = options.runtime_dir.join("pipewire-0");
    let sock = UnixStream::connect(&socket)
        .map_err(|e| format!("connect to PipeWire at {}: {e}", socket.display()))?;
    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| err("main loop")(&e))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| err("context")(&e))?;
    let core = context
        .connect_fd_rc(OwnedFd::from(sock), None)
        .map_err(|e| err("PipeWire core")(&e))?;
    let stream = pw::stream::StreamBox::new(
        &core,
        "gliff",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| err("stream")(&e))?;

    let node = options.node;
    let release_tx = command_tx;
    let state = Rc::new(RefCell::new(State {
        sink,
        frames: FrameSource::new(move |index, generation| {
            let _ = release_tx.send(Cmd::Release { index, generation });
        }),
        pacer: Pacer::new(0),
        options,
        stats,
        started: Instant::now(),
        mainloop: mainloop.clone(),
        ring: Vec::new(),
        files: Vec::new(),
        generation: 0,
        size: (0, 0),
        fourcc: 0,
        sequence: 0,
        cursor: CursorTracker::new(),
    }));

    let _commands = {
        let st = state.clone();
        commands.attach(mainloop.loop_(), move |cmd| st.borrow_mut().command(cmd))
    };

    let _listener = stream
        .add_local_listener_with_user_data((state.clone(), VideoInfoRaw::default()))
        .state_changed(|_, (st, _), _old, new| {
            if let pw::stream::StreamState::Error(e) = new {
                st.borrow_mut().fail(format!("PipeWire stream error: {e}"));
            }
        })
        .param_changed(|stream, (st, fmt), id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() || fmt.parse(param).is_err() {
                return;
            }
            let Some(fourcc) = drm_format(fmt.format()) else {
                st.borrow_mut()
                    .fail(format!("unsupported pixel format {:?}", fmt.format()));
                return;
            };
            let size = fmt.size();
            let configured = st
                .borrow_mut()
                .reconfigure((size.width, size.height), fourcc as u32);
            if let Err(e) = configured {
                st.borrow_mut().fail(e);
                return;
            }
            let (b, m) = (buffers_param(), cursor_meta_param());
            let mut params = [
                Pod::from_bytes(&b).expect("buffers pod"),
                Pod::from_bytes(&m).expect("meta pod"),
            ];
            if let Err(e) = stream.update_params(&mut params) {
                st.borrow_mut().fail(format!("update_params: {e}"));
            }
        })
        .process(|stream, (st, _)| {
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let mut s = st.borrow_mut();
            s.stats.buffers.fetch_add(1, Ordering::Relaxed);
            if let Some(meta) = buffer.find_meta::<MetaCursor>() {
                s.cursor(meta);
            }
            let Some(data) = buffer.datas_mut().first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (size, stride, offset) = (
                chunk.size() as usize,
                chunk.stride().max(0) as usize,
                chunk.offset() as usize,
            );
            if size == 0 {
                s.stats.empty.fetch_add(1, Ordering::Relaxed);
                return;
            }
            let Some(bytes) = data.data() else { return };
            let end = (offset + size).min(bytes.len());
            if offset < end {
                s.ingest(&bytes[offset..end], stride);
            }
        })
        .register()
        .map_err(|e| err("stream listener")(&e))?;

    let format = enum_format();
    let mut params = [Pod::from_bytes(&format).expect("format pod")];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| err("connect the stream")(&e))?;

    let _ = ready.send(Ok(()));
    mainloop.run();
    state.borrow_mut().emit(CaptureEvent::Stopped);
    Ok(())
}
