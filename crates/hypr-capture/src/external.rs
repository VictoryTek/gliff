//! Frames for capture sources other than Hyprland's (the GNOME backend).
//!
//! This is a child module of the crate root so it can build the types whose
//! fields are private there, without changing a line of the Hyprland code.

use std::fs::File;
use std::sync::Arc;

use calloop::channel::{channel, Event};
use calloop::EventLoop;

use crate::{thread, Backing, CaptureBuffer, CapturedFrame, DmabufInfo, Plane, Rect};

/// `DRM_FORMAT_MOD_INVALID`, what a buffer the GPU cannot import reports.
const MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

impl CaptureBuffer {
    /// A CPU-readable buffer backed by `file`, a memfd holding `stride *
    /// height` bytes of 32-bit pixels. Like a wl_shm buffer from Hyprland, it
    /// can only be read on the CPU.
    pub fn from_memfd(
        index: usize,
        generation: u64,
        (width, height): (u32, u32),
        fourcc: u32,
        stride: u32,
        file: File,
    ) -> std::io::Result<Self> {
        let fd = file.try_clone()?.into();
        Ok(Self {
            index,
            generation,
            info: DmabufInfo {
                fd,
                width,
                height,
                fourcc,
                modifier: MOD_INVALID,
                planes: vec![Plane { offset: 0, stride }],
            },
            backing: Backing::Shm(file),
        })
    }
}

impl CaptureBuffer {
    /// A buffer that is a dmabuf some other source owns, described by the
    /// source: `fd` is our own handle to it. The GPU imports it with
    /// `modifier`, `offset` and `stride`; the CPU cannot map it (`is_shm` is
    /// false and `read_bgra` fails).
    #[allow(clippy::too_many_arguments)]
    pub fn from_dmabuf(
        index: usize,
        generation: u64,
        (width, height): (u32, u32),
        fourcc: u32,
        modifier: u64,
        offset: u32,
        stride: u32,
        fd: std::os::fd::OwnedFd,
    ) -> Self {
        Self {
            index,
            generation,
            info: DmabufInfo {
                fd,
                width,
                height,
                fourcc,
                modifier,
                planes: vec![Plane { offset, stride }],
            },
            backing: Backing::External,
        }
    }
}

/// Makes [`CapturedFrame`]s whose release is reported to a callback.
///
/// Dropping a frame calls `on_release(index, generation)` of its buffer, from
/// a thread of this source's own. The thread ends once the source and every
/// frame it made are gone.
pub struct FrameSource {
    tx: calloop::channel::Sender<thread::Cmd>,
}

impl FrameSource {
    pub fn new(on_release: impl Fn(usize, u64) + Send + 'static) -> Self {
        let (tx, rx) = channel::<thread::Cmd>();
        std::thread::spawn(move || {
            let Ok(mut lp) = EventLoop::<()>::try_new() else {
                return;
            };
            let signal = lp.get_signal();
            let added = lp.handle().insert_source(rx, move |ev, _, _| match ev {
                Event::Msg(thread::Cmd::Release { index, generation }) => {
                    on_release(index, generation)
                }
                Event::Closed => signal.stop(),
                Event::Msg(_) => {}
            });
            if added.is_ok() {
                let _ = lp.run(None, &mut (), |_| {});
            }
        });
        Self { tx }
    }

    pub fn frame(
        &self,
        buffer: Arc<CaptureBuffer>,
        damage: Vec<Rect>,
        presentation_ns: u64,
        sequence: u64,
    ) -> CapturedFrame {
        CapturedFrame {
            buffer,
            damage,
            presentation_ns,
            sequence,
            transform: 0,
            release: Some(self.tx.clone()),
        }
    }
}
