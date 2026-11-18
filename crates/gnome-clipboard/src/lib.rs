//! The clipboard of a GNOME desktop as gliff's clipboard bridge sees it.
//!
//! It moves no data itself, like the Hyprland bridge: it reports which mime
//! types the desktop's selection offers, hands out a pipe to read one of them,
//! advertises mime types on behalf of the remote, and hands the owner a pipe
//! for every paste.
//!
//! It uses a RemoteDesktop session of its own (no screen cast), so it starts
//! and ends independently of capture and input. Dropping it ends the session.

use std::fs::File;
use std::io;
use std::os::fd::OwnedFd;
use std::sync::mpsc::Receiver;
use std::thread::JoinHandle;

use anyhow::{Context, Result};
use gnome_ipc::{ClipboardPort, ClipboardSignal, Session};
pub use hypr_input::{ClipboardEvent, ClipboardSink};
use nix::fcntl::OFlag;
use nix::unistd::pipe2;
use rustix::fs::{fcntl_getfl, fcntl_setfl, OFlags};

/// Marks a selection as set by [`Clipboard::offer`]. It carries no data. A
/// clipboard manager that republishes our selection keeps the marker, so its
/// copy of the remote's item is not reported as new either (the same trick the
/// Hyprland bridge uses).
pub const OWN_SOURCE_MIME: &str = "application/x-gliff-gnome-source";

fn is_ours(mime_types: &[String]) -> bool {
    mime_types.iter().any(|m| m == OWN_SOURCE_MIME)
}

/// Make a descriptor Mutter handed out blocking, so a plain `read` or `write`
/// waits for data instead of failing with `EAGAIN`.
pub fn into_blocking(fd: OwnedFd) -> Result<OwnedFd> {
    let flags = fcntl_getfl(&fd).context("read descriptor flags")?;
    fcntl_setfl(&fd, flags - OFlags::NONBLOCK).context("make the descriptor blocking")?;
    Ok(fd)
}

/// Copy a paste's data from `from` (the pipe the consumer writes) to `to`
/// (what Mutter gave us) until the consumer closes its end. True when all of
/// it was written.
fn pump(from: OwnedFd, to: OwnedFd) -> bool {
    io::copy(&mut File::from(from), &mut File::from(to)).is_ok()
}

/// Handle to the clipboard. Dropping it ends it.
pub struct Clipboard {
    session: Option<Session>,
    port: ClipboardPort,
    thread: Option<JoinHandle<()>>,
}

impl Clipboard {
    /// Create and start the session at the session bus `address`. Events
    /// arrive on `sink` from a thread of the clipboard's own.
    pub fn start(address: &str, sink: ClipboardSink) -> Result<Self> {
        let session = Session::create_remote_only(address)?;
        let (port, signals) = session.enable_clipboard()?;
        session.start()?;
        let thread = std::thread::Builder::new()
            .name("gnome-clip".into())
            .spawn({
                let port = port.clone();
                move || run(port, signals, sink)
            })?;
        Ok(Self {
            session: Some(session),
            port,
            thread: Some(thread),
        })
    }

    /// Become the desktop's selection, offering `mime_types`; each paste
    /// arrives as [`ClipboardEvent::Paste`]. An empty list withdraws our
    /// selection if we still hold it (Mutter decides: it unsets only a
    /// selection that is still ours).
    pub fn offer(&self, mime_types: Vec<String>) {
        let mut types = mime_types;
        if !types.is_empty() {
            types.push(OWN_SOURCE_MIME.to_string());
        }
        if let Err(e) = self.port.set_selection(&types) {
            tracing::warn!(error = %e, "clipboard offer failed");
        }
    }

    /// Ask the selection's owner for `mime_type`; the returned pipe yields the
    /// bytes and then EOF.
    pub fn receive(&self, mime_type: String) -> Result<OwnedFd> {
        into_blocking(self.port.selection_read(&mime_type)?)
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // Closing the session ends the signal channel, which ends the thread.
        self.session.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn run(port: ClipboardPort, signals: Receiver<ClipboardSignal>, mut sink: ClipboardSink) {
    while let Ok(signal) = signals.recv() {
        match signal {
            ClipboardSignal::OwnerChanged {
                mime_types,
                is_owner,
            } => {
                // Our own selection, or a copy of it a clipboard manager
                // republished, is never reported back as new.
                if !is_owner && !is_ours(&mime_types) {
                    sink(ClipboardEvent::Selection { mime_types });
                }
            }
            ClipboardSignal::Transfer { mime_type, serial } => {
                if let Err(e) = paste(&port, &mut sink, mime_type, serial) {
                    tracing::warn!(error = %e, "clipboard paste failed");
                    let _ = port.selection_write_done(serial, false);
                }
            }
        }
    }
}

/// An application pastes: the consumer writes into a pipe of ours, and a
/// thread carries it to Mutter and reports the transfer done once the
/// consumer closes its end.
fn paste(
    port: &ClipboardPort,
    sink: &mut ClipboardSink,
    mime_type: String,
    serial: u32,
) -> Result<()> {
    let to_mutter = into_blocking(port.selection_write(serial)?)?;
    if mime_type == OWN_SOURCE_MIME {
        // The marker has no data: finish the transfer empty.
        drop(to_mutter);
        return port.selection_write_done(serial, true);
    }
    let (reader, writer) = pipe2(OFlag::O_CLOEXEC).context("create a pipe")?;
    let port = port.clone();
    std::thread::Builder::new()
        .name("gnome-clip-paste".into())
        .spawn(move || {
            let ok = pump(reader, to_mutter);
            let _ = port.selection_write_done(serial, ok);
        })?;
    sink(ClipboardEvent::Paste {
        mime_type,
        fd: writer,
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};

    fn mimes(m: &[&str]) -> Vec<String> {
        m.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn a_selection_carrying_our_marker_is_ours() {
        assert!(is_ours(&mimes(&["text/plain", OWN_SOURCE_MIME])));
        assert!(!is_ours(&mimes(&["text/plain"])));
        assert!(!is_ours(&[]));
    }

    #[test]
    fn pump_carries_everything_until_the_writer_closes() {
        let (r1, w1) = pipe2(OFlag::O_CLOEXEC).unwrap();
        let (r2, w2) = pipe2(OFlag::O_CLOEXEC).unwrap();
        let payload = vec![7u8; 300_000];
        let writer = {
            let payload = payload.clone();
            std::thread::spawn(move || {
                File::from(w1).write_all(&payload).unwrap();
            })
        };
        let reader = std::thread::spawn(move || {
            let mut out = Vec::new();
            File::from(r2).read_to_end(&mut out).unwrap();
            out
        });
        assert!(pump(r1, w2));
        writer.join().unwrap();
        assert_eq!(reader.join().unwrap(), payload);
    }

    #[test]
    fn pump_reports_a_reader_that_went_away() {
        let (r1, w1) = pipe2(OFlag::O_CLOEXEC).unwrap();
        let (r2, w2) = pipe2(OFlag::O_CLOEXEC).unwrap();
        drop(r2);
        let writer = std::thread::spawn(move || {
            let _ = File::from(w1).write_all(&[1u8; 64]);
        });
        assert!(!pump(r1, w2));
        writer.join().unwrap();
    }

    #[test]
    fn into_blocking_clears_nonblock() {
        let (r, _w) = pipe2(OFlag::O_CLOEXEC | OFlag::O_NONBLOCK).unwrap();
        assert!(fcntl_getfl(&r).unwrap().contains(OFlags::NONBLOCK));
        let r = into_blocking(r).unwrap();
        assert!(!fcntl_getfl(&r).unwrap().contains(OFlags::NONBLOCK));
    }
}
