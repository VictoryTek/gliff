//! A private screen: a Mutter virtual monitor that exists as long as this
//! value does.
//!
//! Mutter creates the monitor only once something is connected to the
//! stream, and sizes it from the format that consumer asks for. So the
//! output owns the whole chain: a session, its stream, the capture that
//! makes the monitor appear, and the idle inhibition that keeps the machine
//! awake meanwhile. Dropping it removes the monitor.

use std::fs::File;
use std::path::Path;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use gnome_ipc::power::{self, IdleInhibitor};
use gnome_ipc::{mutter, CursorMode, GraphicalSession, Monitor, Session, Stream, StreamSource};

use crate::{Capturer, EventSink, Options};

const STEP: Duration = Duration::from_millis(50);

/// Mutter reuses a connector name once its monitor is gone, so a monitor is
/// told apart by its serial too.
fn same_monitor(a: &Monitor, b: &Monitor) -> bool {
    a.connector == b.connector && a.serial == b.serial
}

/// Hold this while a new monitor is being identified. The monitor is named by
/// what is new in `DisplayConfig`, so two creations at once (two gliff
/// processes) could both take the first new one. A lock file in the runtime
/// directory orders them; it is released when the file is dropped, or when
/// the process dies.
fn lock_creation(runtime_dir: &Path) -> Result<File> {
    let path = runtime_dir.join("gliff-virtual-output.lock");
    let file = File::create(&path).with_context(|| format!("open {}", path.display()))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)
        .with_context(|| format!("lock {}", path.display()))?;
    Ok(file)
}

// Fields drop in order: the capture ends first, then the session (which
// removes the monitor), then the inhibition.
pub struct VirtualOutput {
    capturer: Capturer,
    session: Session,
    stream: Stream,
    connector: String,
    serial: String,
    _inhibitor: IdleInhibitor,
}

impl VirtualOutput {
    /// Create a virtual monitor of `size`, at `scale` if Mutter supports it. A blanked screen is woken first
    /// (Mutter refuses new sessions on a blanked screen) and the desktop is
    /// kept from going idle until the output is dropped. Capture events go to
    /// `sink` from the start, the first `Ready` included. `dmabuf` is the
    /// stream's [`Options::dmabuf`]: empty for shared memory.
    pub fn create(
        desktop: &GraphicalSession,
        size: (u32, u32),
        scale: f64,
        cursor: CursorMode,
        dmabuf: Vec<(drm_fourcc::DrmFourcc, Vec<u64>)>,
        sink: EventSink,
    ) -> Result<Self> {
        let conn = mutter::connect(&desktop.bus_address)?;
        power::wake(&conn).context("wake the screen")?;
        let inhibitor = IdleInhibitor::new(&desktop.bus_address, "Remote screen in use")?;
        let creating = lock_creation(&desktop.runtime_dir)?;
        let before = mutter::monitors(&conn)?;

        let mut session = Session::create(&desktop.bus_address)?;
        let stream = session.record(&StreamSource::Virtual, cursor)?;
        session.start()?;
        let node = stream.wait_node(Duration::from_secs(5))?;
        let mut options = Options::new(node, &desktop.runtime_dir, "virtual");
        options.size = Some(size);
        options.scale = Some(scale);
        options.dmabuf = dmabuf;
        let capturer = Capturer::start(options, sink)?;

        // The monitor appears once the stream has a format. Name it by what
        // is new, since the connector (`Meta-0`) is Mutter's choice.
        let deadline = Instant::now() + Duration::from_secs(5);
        let monitor = loop {
            let found = session
                .monitors()?
                .into_iter()
                .find(|m| !before.iter().any(|b| same_monitor(b, m)));
            match found {
                Some(m) => break m,
                None if Instant::now() < deadline => std::thread::sleep(STEP),
                None => {
                    // Dropping `session` and `capturer` ends the attempt.
                    let _ = session.stop();
                    bail!("no new monitor appeared after the stream started");
                }
            }
        };
        drop(creating);
        Ok(Self {
            capturer,
            session,
            stream,
            connector: monitor.connector,
            serial: monitor.serial,
            _inhibitor: inhibitor,
        })
    }

    /// Mutter's name for the monitor (`Meta-0`).
    pub fn connector(&self) -> &str {
        &self.connector
    }

    /// The monitor's serial, which tells it from an earlier monitor that had
    /// the same connector name.
    pub fn serial(&self) -> &str {
        &self.serial
    }

    /// The monitor as Mutter reports it now.
    pub fn monitor(&self) -> Result<Monitor> {
        self.session
            .monitors()?
            .into_iter()
            .find(|m| m.connector == self.connector && m.serial == self.serial)
            .ok_or_else(|| anyhow!("monitor {} is gone", self.connector))
    }

    /// The capture running on the monitor.
    pub fn capturer(&self) -> &Capturer {
        &self.capturer
    }

    /// The stream's D-Bus path, the first argument of
    /// `NotifyPointerMotionAbsolute`.
    pub fn stream_path(&self) -> &str {
        self.stream.path()
    }

    /// The session, for input.
    pub fn session(&self) -> &Session {
        &self.session
    }

    /// Resize the monitor and wait until Mutter reports the new size.
    ///
    /// `scale` is Mutter's preferred scale for the new mode: it takes the
    /// nearest scale the mode supports, none that would leave under 800x480
    /// logical pixels, so read the scale back from [`VirtualOutput::monitor`];
    /// it gets a moment to settle on `scale` before this returns. Seen on
    /// GNOME 50.6: the scale is applied when the monitor is created and when
    /// the size changes, but a new scale at the same size does nothing.
    pub fn resize(&self, width: u32, height: u32, scale: f64) -> Result<()> {
        self.capturer.resize(width, height, scale)?;
        let deadline = Instant::now() + Duration::from_secs(3);
        let mut sized = None;
        loop {
            let m = self.monitor()?;
            if (m.width, m.height) == (width, height) {
                let since = *sized.get_or_insert_with(Instant::now);
                if (m.scale - scale).abs() < 0.01 || since.elapsed() > Duration::from_secs(1) {
                    return Ok(());
                }
            } else if Instant::now() >= deadline {
                bail!(
                    "asked for {width}x{height}, Mutter still reports {}x{}",
                    m.width,
                    m.height
                );
            }
            std::thread::sleep(STEP);
        }
    }

    /// Remove the monitor and wait until Mutter no longer lists it.
    pub fn close(self) -> Result<()> {
        let (connector, serial) = (self.connector.clone(), self.serial.clone());
        let Self {
            capturer,
            mut session,
            ..
        } = self;
        drop(capturer);
        session.stop()?;
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if !session
                .monitors()?
                .iter()
                .any(|m| m.connector == connector && m.serial == serial)
            {
                return Ok(());
            }
            if Instant::now() >= deadline {
                bail!("monitor {connector} is still listed after the session ended");
            }
            std::thread::sleep(STEP);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rustix::fs::{flock, FlockOperation};

    #[test]
    fn creations_take_turns() {
        let dir = std::env::temp_dir().join(format!("gliff-lock-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = lock_creation(&dir).unwrap();
        // While the first holds the lock, another one cannot take it.
        let other = File::create(dir.join("gliff-virtual-output.lock")).unwrap();
        assert!(flock(&other, FlockOperation::NonBlockingLockExclusive).is_err());
        drop(first);
        assert!(flock(&other, FlockOperation::NonBlockingLockExclusive).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
