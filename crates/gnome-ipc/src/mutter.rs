//! Mutter's `RemoteDesktop`, `ScreenCast` and `DisplayConfig` interfaces,
//! over zbus's blocking API.
//!
//! A [`Session`] owns its own bus connection. Mutter closes a session when
//! the connection that created it goes away, so dropping the `Session`
//! (which also stops it) cannot leave a capture running.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

const RD: &str = "org.gnome.Mutter.RemoteDesktop";
const RD_PATH: &str = "/org/gnome/Mutter/RemoteDesktop";
const RD_SESSION: &str = "org.gnome.Mutter.RemoteDesktop.Session";
const SC: &str = "org.gnome.Mutter.ScreenCast";
const SC_PATH: &str = "/org/gnome/Mutter/ScreenCast";
const SC_SESSION: &str = "org.gnome.Mutter.ScreenCast.Session";
const SC_STREAM: &str = "org.gnome.Mutter.ScreenCast.Stream";
const DC: &str = "org.gnome.Mutter.DisplayConfig";
const DC_PATH: &str = "/org/gnome/Mutter/DisplayConfig";

/// How a stream carries the mouse cursor (Mutter's `cursor-mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorMode {
    /// Not in the stream at all.
    Hidden = 0,
    /// Painted into the frames.
    Embedded = 1,
    /// Not painted; position and image travel as stream metadata.
    Metadata = 2,
}

/// Connect to the session bus at `address`.
pub fn connect(address: &str) -> Result<Connection> {
    zbus::blocking::connection::Builder::address(address)
        .with_context(|| format!("bad bus address {address}"))?
        .build()
        .with_context(|| format!("connect to the session bus at {address}"))
}

pub(crate) fn proxy<'a>(
    conn: &Connection,
    dest: &'static str,
    path: impl TryInto<zbus::zvariant::ObjectPath<'a>, Error = impl Into<zbus::Error>>,
    iface: &'static str,
) -> Result<Proxy<'a>> {
    Ok(Proxy::new(conn, dest, path, iface)?)
}

/// The `Version` of both Mutter interfaces: `(RemoteDesktop, ScreenCast)`.
/// An error here usually means the session is not GNOME.
pub fn versions(conn: &Connection) -> Result<(i32, i32)> {
    let rd = proxy(conn, RD, RD_PATH, RD)?
        .get_property::<i32>("Version")
        .context("RemoteDesktop service (is this a GNOME session?)")?;
    let sc = proxy(conn, SC, SC_PATH, SC)?
        .get_property::<i32>("Version")
        .context("ScreenCast service (is this a GNOME session?)")?;
    Ok((rd, sc))
}

/// One monitor, as `DisplayConfig` reports it.
#[derive(Debug, Clone, PartialEq)]
pub struct Monitor {
    /// The connector name (`DP-1`), which `RecordMonitor` takes.
    pub connector: String,
    /// Size of the current mode.
    pub width: u32,
    pub height: u32,
    pub refresh_hz: f64,
    pub scale: f64,
    /// Position in logical pixels.
    pub x: i32,
    pub y: i32,
    pub primary: bool,
}

type Mode = (
    String,
    i32,
    i32,
    f64,
    f64,
    Vec<f64>,
    HashMap<String, OwnedValue>,
);
type Mon = (
    (String, String, String, String),
    Vec<Mode>,
    HashMap<String, OwnedValue>,
);
type Logical = (
    i32,
    i32,
    f64,
    u32,
    bool,
    Vec<(String, String, String, String)>,
    HashMap<String, OwnedValue>,
);
type State = (u32, Vec<Mon>, Vec<Logical>, HashMap<String, OwnedValue>);

/// Every enabled monitor. Monitors that are not part of a logical monitor
/// (switched off) are left out.
pub fn monitors(conn: &Connection) -> Result<Vec<Monitor>> {
    let (_serial, mons, logical, _props): State = proxy(conn, DC, DC_PATH, DC)?
        .call("GetCurrentState", &())
        .context("DisplayConfig.GetCurrentState")?;
    Ok(build_monitors(&mons, &logical))
}

fn build_monitors(mons: &[Mon], logical: &[Logical]) -> Vec<Monitor> {
    let mut out = Vec::new();
    for (x, y, scale, _transform, primary, members, _) in logical {
        for (connector, ..) in members {
            let Some((_, modes, _)) = mons.iter().find(|m| &m.0 .0 == connector) else {
                continue;
            };
            let is_current = |m: &&Mode| {
                m.6.get("is-current")
                    .and_then(|v| bool::try_from(v).ok())
                    .unwrap_or(false)
            };
            let Some(mode) = modes.iter().find(is_current) else {
                continue;
            };
            out.push(Monitor {
                connector: connector.clone(),
                width: mode.1.max(0) as u32,
                height: mode.2.max(0) as u32,
                refresh_hz: mode.3,
                scale: *scale,
                x: *x,
                y: *y,
                primary: *primary,
            });
        }
    }
    out
}

/// What a stream captures.
#[derive(Debug, Clone)]
pub enum StreamSource {
    /// A real monitor, by connector name.
    Monitor(String),
    /// A new virtual monitor that exists as long as the session.
    Virtual,
}

/// A recorded source. The PipeWire node appears once the session starts.
pub struct Stream {
    path: OwnedObjectPath,
    // In a mutex so a `Stream` can be shared between threads.
    node: Mutex<Receiver<u32>>,
}

impl Stream {
    /// D-Bus path of the stream; the first argument of
    /// `NotifyPointerMotionAbsolute`.
    pub fn path(&self) -> &str {
        self.path.as_str()
    }

    /// The PipeWire node id, once `Mutter` has started the stream.
    pub fn wait_node(&self, timeout: Duration) -> Result<u32> {
        self.node
            .lock()
            .map_err(|_| anyhow!("stream state poisoned"))?
            .recv_timeout(timeout)
            .map_err(|_| anyhow!("no PipeWireStreamAdded signal within {timeout:?}"))
    }
}

/// Closes the bus connection when dropped. The signal listener threads hold
/// clones of the connection, so merely dropping ours would leave the
/// session open in Mutter and the listeners blocked.
pub(crate) struct Closing(Connection);

impl Closing {
    pub(crate) fn new(conn: Connection) -> Self {
        Closing(conn)
    }
}

impl std::ops::Deref for Closing {
    type Target = Connection;
    fn deref(&self) -> &Connection {
        &self.0
    }
}

impl Drop for Closing {
    fn drop(&mut self) {
        let _ = self.0.clone().close();
    }
}

/// A RemoteDesktop session with a ScreenCast session linked to it, so
/// starting one starts both.
pub struct Session {
    conn: Closing,
    pub(crate) rd: Proxy<'static>,
    sc: Option<Proxy<'static>>,
    id: String,
    closed: Arc<AtomicBool>,
    stopped: bool,
}

impl Session {
    /// Create the pair of sessions on a connection of its own.
    pub fn create(address: &str) -> Result<Session> {
        Self::create_with(address, true)
    }

    /// Create a RemoteDesktop session with no ScreenCast session: nothing to
    /// record, but input and the clipboard work. Use it for the clipboard, so
    /// that it lives and ends on its own.
    pub fn create_remote_only(address: &str) -> Result<Session> {
        Self::create_with(address, false)
    }

    fn create_with(address: &str, screencast: bool) -> Result<Session> {
        let conn = Closing(connect(address)?);
        let rd_root = proxy(&conn, RD, RD_PATH, RD)?;
        let rd_path: OwnedObjectPath = rd_root
            .call("CreateSession", &())
            .context("RemoteDesktop.CreateSession")?;
        let rd = proxy(&conn, RD, rd_path.into_inner(), RD_SESSION)?;
        let id: String = rd.get_property("SessionId")?;

        let closed = Arc::new(AtomicBool::new(false));
        let mut signal = rd.receive_signal("Closed")?;
        let flag = closed.clone();
        std::thread::spawn(move || {
            if signal.next().is_some() {
                flag.store(true, Ordering::SeqCst);
            }
        });

        let sc = if screencast {
            let sc_root = proxy(&conn, SC, SC_PATH, SC)?;
            let mut props: HashMap<&str, Value> = HashMap::new();
            props.insert("remote-desktop-session-id", Value::from(id.as_str()));
            let sc_path: OwnedObjectPath = sc_root
                .call("CreateSession", &(props,))
                .context("ScreenCast.CreateSession")?;
            Some(proxy(&conn, SC, sc_path.into_inner(), SC_SESSION)?)
        } else {
            None
        };
        Ok(Session {
            conn,
            rd,
            sc,
            id,
            closed,
            stopped: false,
        })
    }

    /// The session's input calls. Usable once the session has started.
    pub fn input(&self) -> crate::InputPort {
        crate::InputPort {
            rd: self.rd.clone(),
        }
    }

    /// Every enabled monitor, on this session's own connection.
    pub fn monitors(&self) -> Result<Vec<Monitor>> {
        monitors(&self.conn)
    }

    /// Mutter's session id (what the linked ScreenCast session names).
    pub fn id(&self) -> &str {
        &self.id
    }

    /// True once Mutter has closed the session (the screen locked, say).
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::SeqCst)
    }

    /// Record a source. Call before [`Session::start`].
    pub fn record(&self, source: &StreamSource, cursor: CursorMode) -> Result<Stream> {
        let mut props: HashMap<&str, Value> = HashMap::new();
        props.insert("cursor-mode", Value::from(cursor as u32));
        let sc = self
            .sc
            .as_ref()
            .context("this session was created without ScreenCast")?;
        let path: OwnedObjectPath = match source {
            StreamSource::Monitor(connector) => sc
                .call("RecordMonitor", &(connector.as_str(), props))
                .with_context(|| format!("ScreenCast.RecordMonitor({connector})"))?,
            StreamSource::Virtual => sc
                .call("RecordVirtual", &(props,))
                .context("ScreenCast.RecordVirtual")?,
        };
        let stream = proxy(&self.conn, SC, path.clone().into_inner(), SC_STREAM)?;
        // Subscribe before Start: the signal is sent while Start runs.
        let mut added = stream.receive_signal("PipeWireStreamAdded")?;
        let (tx, node) = mpsc::channel();
        std::thread::spawn(move || {
            if let Some(msg) = added.next() {
                if let Ok(id) = msg.body().deserialize::<u32>() {
                    let _ = tx.send(id);
                }
            }
        });
        Ok(Stream {
            path,
            node: Mutex::new(node),
        })
    }

    /// Start the sessions. Starting the RemoteDesktop session starts the
    /// linked ScreenCast session, which announces its streams.
    pub fn start(&self) -> Result<()> {
        self.rd
            .call::<_, _, ()>("Start", &())
            .context("RemoteDesktop.Session.Start")
    }

    /// End the session. Dropping it does this too.
    pub fn stop(&mut self) -> Result<()> {
        if std::mem::replace(&mut self.stopped, true) {
            return Ok(());
        }
        self.rd
            .call::<_, _, ()>("Stop", &())
            .context("RemoteDesktop.Session.Stop")
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(w: i32, h: i32, current: bool) -> Mode {
        let mut p = HashMap::new();
        if current {
            p.insert("is-current".to_string(), OwnedValue::from(true));
        }
        (format!("{w}x{h}@60"), w, h, 60.0, 1.0, vec![1.0], p)
    }

    fn mon(connector: &str, modes: Vec<Mode>) -> Mon {
        (
            (connector.into(), "V".into(), "P".into(), "S".into()),
            modes,
            HashMap::new(),
        )
    }

    fn logical(x: i32, primary: bool, connector: &str) -> Logical {
        (
            x,
            0,
            2.0,
            0,
            primary,
            vec![(connector.into(), "V".into(), "P".into(), "S".into())],
            HashMap::new(),
        )
    }

    #[test]
    fn monitors_take_the_current_mode() {
        let mons = vec![
            mon("DP-1", vec![mode(1280, 720, false), mode(1920, 1080, true)]),
            mon("HDMI-1", vec![mode(800, 600, true)]),
        ];
        let l = vec![logical(0, true, "DP-1"), logical(960, false, "HDMI-1")];
        let m = build_monitors(&mons, &l);
        assert_eq!(m.len(), 2);
        assert_eq!(
            (m[0].connector.as_str(), m[0].width, m[0].height),
            ("DP-1", 1920, 1080)
        );
        assert!(m[0].primary && !m[1].primary);
        assert_eq!((m[1].x, m[1].scale), (960, 2.0));
    }

    #[test]
    fn a_monitor_without_a_current_mode_or_logical_entry_is_left_out() {
        let mons = vec![
            mon("DP-1", vec![mode(1920, 1080, false)]),
            mon("DP-2", vec![mode(1920, 1080, true)]),
        ];
        // DP-1 has no current mode; DP-2 is off (no logical monitor).
        let l = vec![logical(0, true, "DP-1")];
        assert!(build_monitors(&mons, &l).is_empty());
    }
}
