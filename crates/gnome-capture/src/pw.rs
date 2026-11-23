//! The PipeWire side: negotiate a raw video format, copy frames into the
//! ring and feed the [`Pacer`].

use std::cell::RefCell;
use std::fs::File;
use std::io::Cursor;
use std::os::fd::{BorrowedFd, OwnedFd};
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
use pw::spa::param::video::{VideoFlags, VideoFormat, VideoInfoRaw};
use pw::spa::pod::{Object, Pod, Property, PropertyFlags, Value};
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

fn video_format(f: DrmFourcc) -> Option<VideoFormat> {
    match f {
        DrmFourcc::Xrgb8888 => Some(VideoFormat::BGRx),
        DrmFourcc::Argb8888 => Some(VideoFormat::BGRA),
        DrmFourcc::Xbgr8888 => Some(VideoFormat::RGBx),
        DrmFourcc::Abgr8888 => Some(VideoFormat::RGBA),
        _ => None,
    }
}

/// `DRM_FORMAT_MOD_INVALID`: what a buffer the GPU cannot import reports.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

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

/// A PipeWire tag telling Mutter which scale a virtual monitor should have
/// (`org.gnome.preferred-scale`). Mutter takes the nearest scale the mode
/// supports and reports the one it chose back as the `org.gnome.scale` tag.
fn scale_tag(scale: f64) -> Vec<u8> {
    serialize(Object {
        type_: spa_sys::SPA_TYPE_OBJECT_ParamTag,
        id: spa_sys::SPA_PARAM_Tag,
        properties: vec![
            Property::new(
                spa_sys::SPA_PARAM_TAG_direction,
                Value::Id(spa::utils::Id(spa_sys::SPA_DIRECTION_INPUT)),
            ),
            Property::new(
                spa_sys::SPA_PARAM_TAG_info,
                // A count, then key and value for each item.
                Value::Struct(vec![
                    Value::Int(1),
                    Value::String("org.gnome.preferred-scale".into()),
                    Value::String(format!("{scale}")),
                ]),
            ),
        ],
    })
}

/// The 32-bit layouts the consumer accepts, in the order it prefers.
const SHM_FORMATS: [VideoFormat; 4] = [
    VideoFormat::BGRx,
    VideoFormat::BGRA,
    VideoFormat::RGBx,
    VideoFormat::RGBA,
];

/// What a format alternative says about modifiers.
enum Mods<'a> {
    /// No modifier property: the shared-memory form.
    None,
    /// A list the producer may pick from. The property is mandatory and
    /// "don't fixate", which is how PipeWire's dmabuf negotiation asks the
    /// producer to choose (or hand the choice back to us).
    Offer(&'a [u64]),
    /// This one modifier.
    Fixed(u64),
}

/// One format alternative. With `fixed`, the size is that one size, which asks
/// a virtual monitor to take it; without, any size, which lets the producer
/// choose.
fn format_pod(
    fixed: Option<(u32, u32)>,
    unlimited: bool,
    formats: &[VideoFormat],
    mods: Mods,
) -> Vec<u8> {
    use spa::param::format::{FormatProperties as F, MediaSubtype, MediaType};
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Id, Rectangle};

    let mut properties = vec![
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
    ];
    match mods {
        Mods::None => {}
        Mods::Offer(list) => properties.push(Property {
            key: F::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY
                | PropertyFlags::from_bits_retain(spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE),
            value: Value::Choice(ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: list[0] as i64,
                    alternatives: list.iter().map(|&m| m as i64).collect(),
                },
            ))),
        }),
        Mods::Fixed(m) => properties.push(Property {
            key: F::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY,
            value: Value::Long(m as i64),
        }),
    }
    properties.push(Property::new(
        F::VideoSize.as_raw(),
        match fixed {
            Some((width, height)) => Value::Rectangle(Rectangle { width, height }),
            None => Value::Choice(ChoiceValue::Rectangle(Choice(
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
        },
    ));
    properties.push(Property::new(
        F::VideoFramerate.as_raw(),
        Value::Choice(ChoiceValue::Fraction(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: Fraction { num: 60, denom: 1 },
                min: Fraction { num: 0, denom: 1 },
                max: Fraction { num: 240, denom: 1 },
            },
        ))),
    ));
    // Mutter skips a frame that comes sooner than 1/max_framerate after the
    // last one, and for a monitor it offers a maximum equal to the monitor's
    // refresh rate (0 to 60 here), whose default it keeps when we offer
    // nothing. On a 60 Hz monitor, timestamp jitter then drops about a third
    // of the frames (seen: 38 to 40 of 59 a second). Zero means no limit.
    // Needs a Mutter whose monitor offer starts at 0 (50.6 does; 46 starts at
    // 1, where a fixed 0 would leave nothing in common and capture would
    // not start). This backend is only tested on 50.6.
    //
    // Only for a stream of an existing monitor. For a virtual monitor the
    // maximum is the refresh rate Mutter gives the new monitor
    // (`create_mode_info` in its virtual stream source): 0 made a 0 Hz
    // monitor, with empty buffers all the time.
    if unlimited {
        properties.push(Property::new(
            F::VideoMaxFramerate.as_raw(),
            Value::Fraction(Fraction { num: 0, denom: 1 }),
        ));
    }
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties,
    })
}

/// The shared-memory form of every layout we accept.
fn enum_format(fixed: Option<(u32, u32)>, unlimited: bool) -> Vec<u8> {
    format_pod(fixed, unlimited, &SHM_FORMATS, Mods::None)
}

/// Everything we accept, best first: each layout as a dmabuf with the
/// modifiers the consumer can import, then shared memory.
fn enum_formats(
    fixed: Option<(u32, u32)>,
    unlimited: bool,
    dmabuf: &[(DrmFourcc, Vec<u64>)],
) -> Vec<Vec<u8>> {
    let mut pods: Vec<Vec<u8>> = dmabuf
        .iter()
        .filter(|(_, mods)| !mods.is_empty())
        .filter_map(|(fourcc, mods)| {
            let format = video_format(*fourcc)?;
            Some(format_pod(fixed, unlimited, &[format], Mods::Offer(mods)))
        })
        .collect();
    pods.push(enum_format(fixed, unlimited));
    pods
}

/// The format with the producer's modifier choice settled: what a producer
/// that hands the choice back (`MODIFIER_FIXATION_REQUIRED`) is answered with.
/// It picks the first modifier of `param` that the consumer offered.
fn fixate(
    param: &Pod,
    format: VideoFormat,
    size: (u32, u32),
    offered: &[u64],
    unlimited: bool,
) -> Result<Vec<u8>, String> {
    use spa::param::format::FormatProperties as F;
    use spa::pod::deserialize::PodDeserializer;
    use spa::pod::ChoiceValue;
    use spa::utils::ChoiceEnum;

    let (_, value) = PodDeserializer::deserialize_from::<Value>(param.as_bytes())
        .map_err(|e| format!("read the format: {e:?}"))?;
    let Value::Object(object) = value else {
        return Err("the format is not an object".into());
    };
    let modifier = object
        .properties
        .iter()
        .find(|p| p.key == F::VideoModifier.as_raw())
        .ok_or("the format has no modifier")?;
    let candidates: Vec<u64> = match &modifier.value {
        Value::Long(m) => vec![*m as u64],
        Value::Choice(ChoiceValue::Long(choice)) => match &choice.1 {
            ChoiceEnum::Enum {
                default,
                alternatives,
            } => std::iter::once(*default)
                .chain(alternatives.iter().copied())
                .map(|m| m as u64)
                .collect(),
            ChoiceEnum::None(m) => vec![*m as u64],
            _ => return Err("an unexpected kind of modifier choice".into()),
        },
        _ => return Err("an unexpected kind of modifier value".into()),
    };
    let chosen = candidates
        .iter()
        .copied()
        .find(|m| offered.contains(m))
        .ok_or_else(|| {
            format!("Mutter offers no modifier we can import (it offered {candidates:x?})")
        })?;
    Ok(format_pod(
        Some(size),
        unlimited,
        &[format],
        Mods::Fixed(chosen),
    ))
}

/// Buffers in shared memory: the CPU path reads them.
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

/// dmabuf buffers, and a pool big enough that the consumer can hold `held`
/// frames, one more can be kept for a re-send and one waits for a request,
/// with some left for the producer to draw into.
fn dmabuf_buffers_param(held: usize) -> Vec<u8> {
    use spa::pod::ChoiceValue;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};
    let held = held as i32;
    serialize(Object {
        type_: spa::utils::SpaTypes::ObjectParamBuffers.as_raw(),
        id: spa::param::ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(
                spa_sys::SPA_PARAM_BUFFERS_buffers,
                Value::Choice(ChoiceValue::Int(Choice(
                    ChoiceFlags::empty(),
                    ChoiceEnum::Range {
                        default: held + 4,
                        min: held + 3,
                        max: 16,
                    },
                ))),
            ),
            Property::new(
                spa_sys::SPA_PARAM_BUFFERS_dataType,
                Value::Int(1 << spa_sys::SPA_DATA_DmaBuf),
            ),
        ],
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
    /// The stream, to hand dmabuf buffers back to the producer.
    stream: Option<pw::stream::StreamRc>,
    /// The pool of dmabuf buffers while Mutter's choice is a dmabuf.
    dmabuf: Option<Pool>,
}

/// Mutter's dmabuf buffers, one slot per buffer, in the order they were
/// added. A slot of the pool is a slot of the pacer.
struct Pool {
    modifier: u64,
    slots: Vec<Slot>,
}

struct Slot {
    /// The PipeWire buffer; null once PipeWire removed it.
    raw: *mut pw::sys::pw_buffer,
    /// What frames carry, made when the first frame shows the stride.
    buffer: Option<Arc<CaptureBuffer>>,
    /// We dequeued it and have not queued it again.
    outstanding: bool,
}

impl State {
    fn emit(&mut self, event: CaptureEvent) {
        (self.sink)(event);
    }

    fn fail(&mut self, message: String) {
        self.emit(CaptureEvent::Error(message));
        self.mainloop.quit();
    }

    /// A new format: a fresh ring (a pool that fills as PipeWire adds its
    /// buffers, with a modifier), and the consumer learns the size.
    fn reconfigure(
        &mut self,
        size: (u32, u32),
        fourcc: u32,
        modifier: Option<u64>,
    ) -> Result<(), String> {
        let (w, h) = size;
        if let Some(limit) = self.options.max_size {
            if w > limit.side || h > limit.side || u64::from(w) * u64::from(h) > limit.pixels {
                return Err(format!(
                    "the stream is {w}x{h}, past the {}-pixel-side, {}-pixel limit",
                    limit.side, limit.pixels
                ));
            }
        }
        self.generation += 1;
        if let Some(modifier) = modifier {
            // Buffers we held from the old pool are PipeWire's to remove;
            // frames already sent keep their own handle to the memory.
            self.ring.clear();
            self.files.clear();
            self.dmabuf = Some(Pool {
                modifier,
                slots: Vec::new(),
            });
            self.pacer.reset(0);
            self.announce(size, fourcc, modifier);
            return Ok(());
        }
        self.dmabuf = None;
        let stride = w.checked_mul(4).ok_or("stream too wide")?;
        let len = u64::from(stride) * u64::from(h);
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
        self.announce(size, fourcc, MOD_INVALID);
        Ok(())
    }

    /// Tell the consumer the stream's size, layout and modifier.
    fn announce(&mut self, size: (u32, u32), fourcc: u32, modifier: u64) {
        let (w, h) = size;
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
            modifier,
        });
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
        let buffer = match &self.dmabuf {
            Some(pool) => pool.slots[slot]
                .buffer
                .clone()
                .expect("a sent slot has a frame"),
            None => self.ring[slot].clone(),
        };
        let frame = self.frames.frame(
            buffer,
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
            // Handled where the stream is; see `run_inner`.
            Cmd::Resize { .. } | Cmd::UseShm => {}
        }
        self.reclaim();
    }

    /// PipeWire added a buffer to the pool.
    fn add_buffer(&mut self, raw: *mut pw::sys::pw_buffer) {
        if let Some(pool) = &mut self.dmabuf {
            pool.slots.push(Slot {
                raw,
                buffer: None,
                outstanding: false,
            });
            self.pacer.add_slot();
        }
    }

    /// PipeWire took a buffer out of the pool; it is not ours to queue again.
    fn remove_buffer(&mut self, raw: *mut pw::sys::pw_buffer) {
        if let Some(slot) = self
            .dmabuf
            .iter_mut()
            .flat_map(|p| p.slots.iter_mut())
            .find(|s| s.raw == raw)
        {
            slot.raw = std::ptr::null_mut();
            slot.outstanding = false;
        }
    }

    /// Give every buffer nobody needs any more back to the producer.
    fn reclaim(&mut self) {
        let (Some(pool), Some(stream)) = (&mut self.dmabuf, &self.stream) else {
            return;
        };
        for (i, slot) in pool.slots.iter_mut().enumerate() {
            if slot.outstanding && self.pacer.is_free(i) {
                slot.outstanding = false;
                // SAFETY: `raw` came from this stream's `dequeue_raw_buffer`
                // and `remove_buffer` has not reported it gone (it would
                // have cleared `outstanding`).
                unsafe { stream.queue_raw_buffer(slot.raw) };
            }
        }
    }

    /// Take every buffer Mutter has ready. Each one with a picture is a new
    /// frame for the pacer, which keeps the buffer until it is no longer the
    /// unsent frame, the re-send frame or held by the consumer.
    fn process_dmabuf(&mut self, stream: &pw::stream::Stream) {
        loop {
            // SAFETY: the stream is the one this state belongs to, and the
            // result is only used until it is queued again.
            let raw = unsafe { stream.dequeue_raw_buffer() };
            if raw.is_null() {
                break;
            }
            self.stats.buffers.fetch_add(1, Ordering::Relaxed);
            if self.options.dmabuf.is_empty() {
                // Shared memory was asked for ([`Cmd::UseShm`]); these are the
                // old buffers, still arriving while the format changes, and
                // the consumer cannot read them.
                // SAFETY: just dequeued from `stream`.
                unsafe { stream.queue_raw_buffer(raw) };
                continue;
            }
            self.take(raw, stream);
        }
        self.reclaim();
    }

    fn take(&mut self, raw: *mut pw::sys::pw_buffer, stream: &pw::stream::Stream) {
        let Some(index) = self
            .dmabuf
            .as_ref()
            .and_then(|p| p.slots.iter().position(|s| s.raw == raw))
        else {
            // Not a buffer of this pool (one PipeWire is about to remove).
            // SAFETY: just dequeued from `stream`.
            unsafe { stream.queue_raw_buffer(raw) };
            return;
        };
        // SAFETY: a dequeued buffer's `spa_buffer` and its metadata stay
        // valid until the buffer is queued again, which is after this.
        let (data, cursor) = unsafe {
            let spa_buffer = (*raw).buffer;
            (dmabuf_data(spa_buffer), cursor_meta(spa_buffer))
        };
        if let Some(meta) = cursor {
            self.cursor(meta);
        }
        let (generation, size, fourcc) = (self.generation, self.size, self.fourcc);
        let Some(pool) = &mut self.dmabuf else { return };
        let modifier = pool.modifier;
        let slot = &mut pool.slots[index];
        slot.outstanding = true;
        let Some(data) = data else {
            self.stats.empty.fetch_add(1, Ordering::Relaxed);
            return;
        };
        let known = slot.buffer.as_ref().map(|b| b.info.planes[0]);
        let same = known.is_some_and(|p| p.stride == data.stride && p.offset == data.offset);
        if known.is_some() && !same {
            // The import cache is keyed by slot; a buffer that changed its
            // layout under us cannot be told apart from the old one.
            tracing::warn!("a dmabuf changed its stride or offset; frame dropped");
            self.stats.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        }
        if known.is_none() {
            // SAFETY: the descriptor belongs to the buffer we hold.
            let fd = unsafe { BorrowedFd::borrow_raw(data.fd) };
            let Ok(fd) = fd.try_clone_to_owned() else {
                self.stats.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            };
            slot.buffer = Some(Arc::new(CaptureBuffer::from_dmabuf(
                index,
                generation,
                size,
                fourcc,
                modifier,
                data.offset,
                data.stride,
                fd,
            )));
        }
        if let Some(s) = self.pacer.filled(index) {
            self.send(s);
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

/// Where a dmabuf frame is, as its first data block describes it.
struct DmabufData {
    fd: i32,
    offset: u32,
    stride: u32,
}

/// The dmabuf of a buffer that carries a picture. A buffer with no picture (a
/// cursor-only update: marked corrupted, or no size and no stride) gives
/// `None`. The chunk size is not required: a dmabuf may leave it 0.
///
/// # Safety
/// `buffer` is null or a valid `spa_buffer` of a dequeued buffer.
unsafe fn dmabuf_data(buffer: *mut spa_sys::spa_buffer) -> Option<DmabufData> {
    if buffer.is_null() {
        return None;
    }
    // SAFETY: the caller's promise; the datas and chunk pointers are checked.
    unsafe {
        let b = &*buffer;
        if b.n_datas == 0 || b.datas.is_null() {
            return None;
        }
        let d = &*b.datas;
        if d.type_ != spa_sys::SPA_DATA_DmaBuf || d.chunk.is_null() {
            return None;
        }
        let c = &*d.chunk;
        let corrupted = c.flags & spa_sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0;
        if corrupted || c.stride <= 0 || d.fd < 0 {
            return None;
        }
        Some(DmabufData {
            fd: d.fd as i32,
            offset: c.offset,
            stride: c.stride as u32,
        })
    }
}

/// The cursor metadata of a buffer, if it has any.
///
/// # Safety
/// `buffer` is null or a valid `spa_buffer`, and the result is not used
/// after the buffer is queued again.
unsafe fn cursor_meta<'a>(buffer: *mut spa_sys::spa_buffer) -> Option<&'a MetaCursor> {
    if buffer.is_null() {
        return None;
    }
    // SAFETY: the caller's promise; `MetaCursor` is a transparent wrapper
    // of `spa_meta_cursor`, which is what is asked for.
    unsafe {
        let p = spa_sys::spa_buffer_find_meta_data(
            buffer,
            spa_sys::SPA_META_Cursor,
            std::mem::size_of::<spa_sys::spa_meta_cursor>(),
        );
        (!p.is_null()).then(|| &*(p as *const MetaCursor))
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
    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "gliff",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| err("stream")(&e))?;

    let node = options.node;
    let node_size = options.size;
    let node_scale = options.scale;
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
        stream: Some(stream.clone()),
        dmabuf: None,
    }));

    let _commands = {
        let st = state.clone();
        let stream = stream.clone();
        commands.attach(mainloop.loop_(), move |cmd| match cmd {
            // Asking for a new fixed size makes the producer renegotiate, and
            // `param_changed` below then builds the new ring.
            Cmd::Resize {
                width,
                height,
                scale,
            } => {
                // The scale goes first: Mutter reads it when the format
                // arrives.
                let tag = scale_tag(scale);
                let formats = {
                    let o = &st.borrow().options;
                    enum_formats(Some((width, height)), o.unlimited_rate, &o.dmabuf)
                };
                let mut params = vec![Pod::from_bytes(&tag).expect("tag pod")];
                params.extend(
                    formats
                        .iter()
                        .map(|f| Pod::from_bytes(f).expect("format pod")),
                );
                if let Err(e) = stream.update_params(&mut params) {
                    st.borrow_mut().fail(format!("resize: {e}"));
                }
            }
            // Offer shared memory only from now on; `param_changed` then
            // builds the memfd ring.
            Cmd::UseShm => {
                let size = {
                    let mut s = st.borrow_mut();
                    s.options.dmabuf.clear();
                    // Already shared memory: nothing to renegotiate.
                    if s.dmabuf.is_none() && s.size != (0, 0) {
                        return;
                    }
                    // Frames of the old buffers must not reach a consumer
                    // that cannot read them.
                    if s.dmabuf.is_some() {
                        s.pacer.forget_frames();
                        s.reclaim();
                    }
                    (s.size != (0, 0)).then_some(s.size)
                };
                let format = enum_format(size, st.borrow().options.unlimited_rate);
                let mut params = [Pod::from_bytes(&format).expect("format pod")];
                if let Err(e) = stream.update_params(&mut params) {
                    st.borrow_mut().fail(format!("use shared memory: {e}"));
                }
            }
            cmd => st.borrow_mut().command(cmd),
        })
    };

    let _listener = stream
        .add_local_listener_with_user_data((state.clone(), VideoInfoRaw::default()))
        .state_changed(|_, (st, _), _old, new| {
            if let pw::stream::StreamState::Error(e) = new {
                st.borrow_mut().fail(format!("PipeWire stream error: {e}"));
            }
        })
        .param_changed(|stream, (st, _), id, param| {
            let Some(param) = param else { return };
            let mut fmt = VideoInfoRaw::default();
            if id != spa::param::ParamType::Format.as_raw() || fmt.parse(param).is_err() {
                return;
            }
            let Some(fourcc) = drm_format(fmt.format()) else {
                st.borrow_mut()
                    .fail(format!("unsupported pixel format {:?}", fmt.format()));
                return;
            };
            let size = fmt.size();
            let flags = fmt.flags();
            if flags.contains(VideoFlags::MODIFIER_FIXATION_REQUIRED) {
                // The producer hands the modifier choice back: answer with
                // one we offered, and wait for the fixed format.
                let offered: Vec<u64> = st
                    .borrow()
                    .options
                    .dmabuf
                    .iter()
                    .filter(|(f, _)| *f == fourcc)
                    .flat_map(|(_, m)| m.iter().copied())
                    .collect();
                st.borrow().stats.fixations.fetch_add(1, Ordering::Relaxed);
                let unlimited = st.borrow().options.unlimited_rate;
                match fixate(
                    param,
                    fmt.format(),
                    (size.width, size.height),
                    &offered,
                    unlimited,
                ) {
                    Ok(pod) => {
                        let mut params = [Pod::from_bytes(&pod).expect("format pod")];
                        if let Err(e) = stream.update_params(&mut params) {
                            st.borrow_mut().fail(format!("fix the modifier: {e}"));
                        }
                    }
                    Err(e) => st.borrow_mut().fail(e),
                }
                return;
            }
            let modifier = flags.contains(VideoFlags::MODIFIER).then(|| fmt.modifier());
            let configured =
                st.borrow_mut()
                    .reconfigure((size.width, size.height), fourcc as u32, modifier);
            if let Err(e) = configured {
                st.borrow_mut().fail(e);
                return;
            }
            let held = st.borrow().options.buffers.max(1);
            let b = if modifier.is_some() {
                dmabuf_buffers_param(held)
            } else {
                buffers_param()
            };
            let m = cursor_meta_param();
            let mut params = [
                Pod::from_bytes(&b).expect("buffers pod"),
                Pod::from_bytes(&m).expect("meta pod"),
            ];
            if let Err(e) = stream.update_params(&mut params) {
                st.borrow_mut().fail(format!("update_params: {e}"));
            }
        })
        .add_buffer(|_, (st, _), raw| st.borrow_mut().add_buffer(raw))
        .remove_buffer(|_, (st, _), raw| st.borrow_mut().remove_buffer(raw))
        .process(|stream, (st, _)| {
            if st.borrow().dmabuf.is_some() {
                st.borrow_mut().process_dmabuf(stream);
                return;
            }
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

    let formats = {
        let o = &state.borrow().options;
        enum_formats(node_size, o.unlimited_rate, &o.dmabuf)
    };
    let tag = node_scale.map(scale_tag);
    let mut params: Vec<&Pod> = formats
        .iter()
        .map(|f| Pod::from_bytes(f).expect("format pod"))
        .collect();
    if let Some(tag) = &tag {
        params.insert(0, Pod::from_bytes(tag).expect("tag pod"));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use spa::pod::deserialize::PodDeserializer;
    use spa::utils::{Fraction, Rectangle};

    /// The `VideoSize` property of a serialized format.
    fn video_size(bytes: &[u8]) -> Value {
        let (_, value) = PodDeserializer::deserialize_from::<Value>(bytes).expect("parse");
        let Value::Object(object) = value else {
            panic!("not an object")
        };
        object
            .properties
            .into_iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoSize.as_raw())
            .expect("a size")
            .value
    }

    fn object(bytes: &[u8]) -> Object {
        let (_, value) = PodDeserializer::deserialize_from::<Value>(bytes).expect("parse");
        let Value::Object(object) = value else {
            panic!("not an object")
        };
        object
    }

    fn modifier_property(bytes: &[u8]) -> Option<Property> {
        object(bytes)
            .properties
            .into_iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoModifier.as_raw())
    }

    const TILED: [u64; 3] = [
        0x0300_0000_0060_6010,
        0x0300_0000_0060_6011,
        0x0300_0000_00e0_8010,
    ];

    #[test]
    fn offered_modifiers_are_mandatory_and_left_to_the_producer() {
        let p = modifier_property(&format_pod(
            None,
            false,
            &[VideoFormat::BGRx],
            Mods::Offer(&TILED),
        ))
        .expect("a modifier property");
        assert!(p.flags.contains(PropertyFlags::MANDATORY));
        assert!(p.flags.contains(PropertyFlags::from_bits_retain(
            spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE
        )));
        let Value::Choice(spa::pod::ChoiceValue::Long(choice)) = p.value else {
            panic!("not a long choice")
        };
        let spa::utils::ChoiceEnum::Enum {
            default,
            alternatives,
        } = choice.1
        else {
            panic!("not an enum")
        };
        assert_eq!(default as u64, TILED[0]);
        assert_eq!(
            alternatives.iter().map(|&m| m as u64).collect::<Vec<_>>(),
            TILED
        );
    }

    #[test]
    fn the_shared_memory_form_has_no_modifier() {
        assert!(modifier_property(&enum_format(None, false)).is_none());
    }

    #[test]
    fn dmabuf_alternatives_come_first_and_a_format_without_modifiers_is_left_out() {
        let offer = [
            (DrmFourcc::Xrgb8888, TILED.to_vec()),
            (DrmFourcc::Argb8888, Vec::new()),
        ];
        let pods = enum_formats(None, false, &offer);
        assert_eq!(pods.len(), 2, "one dmabuf alternative, then shared memory");
        assert!(modifier_property(&pods[0]).is_some());
        assert!(modifier_property(&pods[1]).is_none());
        assert_eq!(enum_formats(None, false, &[]).len(), 1);
    }

    #[test]
    fn fixating_takes_the_first_modifier_we_offered() {
        // What Mutter hands back: its own list, in its own order.
        let theirs = [TILED[2], 0x1234, TILED[1]];
        let producer = format_pod(
            Some((1920, 1080)),
            false,
            &[VideoFormat::BGRx],
            Mods::Offer(&theirs),
        );
        let param = Pod::from_bytes(&producer).unwrap();
        let ours = [TILED[1], TILED[0]];
        let fixed = fixate(param, VideoFormat::BGRx, (1920, 1080), &ours, false).expect("a choice");
        let p = modifier_property(&fixed).unwrap();
        assert!(matches!(p.value, Value::Long(m) if m as u64 == TILED[1]));
        assert!(!p.flags.contains(PropertyFlags::from_bits_retain(
            spa_sys::SPA_POD_PROP_FLAG_DONT_FIXATE
        )));
        // Nothing in common is an error that says what Mutter offered.
        let err = fixate(param, VideoFormat::BGRx, (1920, 1080), &[1, 2], false).unwrap_err();
        assert!(err.contains("offers no modifier we can import"), "{err}");
    }

    fn max_framerate(bytes: &[u8]) -> Option<Value> {
        object(bytes)
            .properties
            .into_iter()
            .find(|p| p.key == spa::param::format::FormatProperties::VideoMaxFramerate.as_raw())
            .map(|p| p.value)
    }

    #[test]
    fn a_monitor_stream_asks_for_no_frame_rate_limit_in_every_form() {
        // Mutter limits a monitor stream to the maximum it settles on, which
        // is its own offer (the refresh rate) unless we name one; zero is
        // the only value it does not limit by. Shared memory, dmabuf, the
        // fixed size and the fixated answer alike.
        let offer = [(DrmFourcc::Xrgb8888, TILED.to_vec())];
        let mut forms = enum_formats(None, true, &offer);
        forms.push(enum_format(Some((1280, 720)), true));
        let producer = format_pod(
            Some((1920, 1080)),
            true,
            &[VideoFormat::BGRx],
            Mods::Offer(&TILED),
        );
        forms.push(
            fixate(
                Pod::from_bytes(&producer).unwrap(),
                VideoFormat::BGRx,
                (1920, 1080),
                &TILED,
                true,
            )
            .unwrap(),
        );
        assert_eq!(forms.len(), 4);
        for form in forms {
            let max = max_framerate(&form).expect("a max frame rate");
            assert!(
                matches!(max, Value::Fraction(Fraction { num: 0, denom: 1 })),
                "{max:?}"
            );
        }
    }

    #[test]
    fn a_virtual_monitor_stream_names_no_maximum() {
        // For a virtual monitor the maximum becomes the new monitor's
        // refresh rate: naming 0 made a 0 Hz monitor.
        let offer = [(DrmFourcc::Xrgb8888, TILED.to_vec())];
        let mut forms = enum_formats(None, false, &offer);
        forms.push(enum_format(Some((1280, 720)), false));
        let producer = format_pod(
            Some((1920, 1080)),
            false,
            &[VideoFormat::BGRx],
            Mods::Offer(&TILED),
        );
        forms.push(
            fixate(
                Pod::from_bytes(&producer).unwrap(),
                VideoFormat::BGRx,
                (1920, 1080),
                &TILED,
                false,
            )
            .unwrap(),
        );
        for form in forms {
            assert!(max_framerate(&form).is_none());
        }
    }

    #[test]
    fn a_fixed_format_names_one_size_and_the_default_takes_any() {
        assert!(matches!(
            video_size(&enum_format(Some((1280, 720)), false)),
            Value::Rectangle(Rectangle {
                width: 1280,
                height: 720
            })
        ));
        assert!(matches!(
            video_size(&enum_format(None, false)),
            Value::Choice(_)
        ));
    }
}
