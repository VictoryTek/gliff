//! A scripted fake gliff-server for testing the **Session** with no ssh child
//! and no window. [`start`] runs `session` over an in-memory stream pair; the
//! test then plays `ServerMsg`s through the [`FakeServer`], reads the
//! `ClientMsg`s the session sends back, and watches what reaches the window
//! through the [`FakeWindow`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use gliff_proto::{
    features, ChromaMode, ClientMsg, Codec, OutputInfo, ServerMsg, SessionInfo, PROTOCOL_VERSION,
};
use gliff_transport::Framed;
use tokio::io::{DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender};
use tokio::task::JoinHandle;

use super::{session, ClipLink, Picture, Slot, Ui, UiEvent};
use crate::clip::{FromWin, ToWin, WinTx};

/// How long a test waits for something the session should do at once.
const WAIT: Duration = Duration::from_secs(5);
/// Room in the in-memory pipe: more than any frame a test sends.
const PIPE: usize = 4 << 20;

/// The server's end of the stream pair.
pub struct FakeServer {
    reader: Framed<ReadHalf<DuplexStream>>,
    writer: Framed<WriteHalf<DuplexStream>>,
}

impl FakeServer {
    /// The next message the session sent. Its payload, if any, is skipped.
    pub async fn recv(&mut self) -> ClientMsg {
        tokio::time::timeout(WAIT, self.reader.read_msg::<ClientMsg>())
            .await
            .expect("the session sent nothing")
            .expect("read from the session")
    }

    /// Send any message with no payload.
    pub async fn send(&mut self, msg: ServerMsg) {
        self.send_with_payload(msg, &[]).await;
    }

    /// Send a message followed by its payload bytes.
    pub async fn send_with_payload(&mut self, msg: ServerMsg, payload: &[u8]) {
        let payloads: &[&[u8]] = if payload.is_empty() { &[] } else { &[payload] };
        self.writer
            .write_msg_with_payloads(&msg, payloads)
            .await
            .expect("write to the session");
    }

    /// Answer a `Hello` the way gliff-server does: `HelloAck`, then the
    /// `StreamConfig` of a Single420 H.264 stream of this size.
    pub async fn accept(&mut self, width: u32, height: u32) {
        self.send(hello_ack(false)).await;
        self.send(stream_config(width, height)).await;
    }

    /// Send one encoded picture as a `VideoFrame`.
    pub async fn send_frame(&mut self, frame_id: u64, keyframe: bool, data: &[u8]) {
        let msg = ServerMsg::VideoFrame {
            frame_id,
            pts_us: frame_id * 16_667,
            keyframe,
            damage: vec![],
            data_len: data.len() as u32,
            aux_len: 0,
        };
        self.send_with_payload(msg, data).await;
    }

    /// Drop the stream, as a dead link or an exiting server does.
    pub fn drop_stream(self) {}
}

/// The `HelloAck` of a server showing its `DP-1`, or a **Private screen**
/// when `headless` (the protocol's name for it).
pub fn hello_ack(headless: bool) -> ServerMsg {
    ServerMsg::HelloAck {
        version: PROTOCOL_VERSION,
        session: SessionInfo {
            headless,
            output: "DP-1".into(),
        },
        outputs: vec![OutputInfo {
            name: "DP-1".into(),
            width: 1920,
            height: 1080,
            scale_milli: 1000,
        }],
        features: features(),
    }
}

/// The `StreamConfig` of a Single420 H.264 stream of this size, unscaled.
pub fn stream_config(width: u32, height: u32) -> ServerMsg {
    ServerMsg::StreamConfig {
        codec: Codec::H264,
        chroma: ChromaMode::Single420,
        pipeline: None,
        width,
        height,
        scale_milli: 1000,
        extradata: vec![],
        aux_extradata: None,
        view_width: width,
        view_height: height,
        fps_cap: 60,
    }
}

/// What the session reaches on the window's side, and the window's ways in.
pub struct FakeWindow {
    events: UnboundedReceiver<UiEvent>,
    slot: Slot,
    /// Set while a `UiEvent::Frame` is on its way; the window clears it when
    /// it takes the picture.
    notified: Arc<AtomicBool>,
    /// The window's input; `None` once the window has closed.
    input: Option<UnboundedSender<ClientMsg>>,
    /// The clipboard thread's end, held open as the real thread holds it.
    _clip_events: UnboundedSender<FromWin>,
    /// What the session asked of the Windows clipboard thread.
    to_win: std::sync::mpsc::Receiver<ToWin>,
}

impl FakeWindow {
    /// The next event the session sent the window.
    pub async fn event(&mut self) -> UiEvent {
        tokio::time::timeout(WAIT, self.events.recv())
            .await
            .expect("no event reached the window")
            .expect("the session's event channel closed")
    }

    /// The picture waiting in the slot, taken as the window takes it.
    pub fn take_picture(&self) -> Option<Picture> {
        self.notified.store(false, Ordering::Release);
        self.slot.lock().unwrap_or_else(|e| e.into_inner()).take()
    }

    /// The next request the session made of the Windows clipboard thread.
    pub fn clipboard_request(&self) -> ToWin {
        self.to_win
            .recv_timeout(WAIT)
            .expect("nothing reached the clipboard thread")
    }

    /// Send input as the window does.
    pub fn input(&self, msg: ClientMsg) {
        self.input
            .as_ref()
            .expect("the window is closed")
            .send(msg)
            .expect("the session stopped taking input");
    }

    /// Close the window: its input channel goes away.
    pub fn close(&mut self) {
        self.input = None;
    }
}

/// Start a session against a fake server. Call it inside a `LocalSet`: the
/// session's clipboard engine spawns local tasks.
pub fn start() -> (FakeServer, FakeWindow, JoinHandle<Result<()>>) {
    let (client, server) = tokio::io::duplex(PIPE);
    let (client_rd, client_wr) = tokio::io::split(client);
    let (server_rd, server_wr) = tokio::io::split(server);

    let (events_tx, events) = unbounded_channel();
    let (ui, slot, notified, _cursor) = Ui::recording(events_tx);
    let (input_tx, input_rx) = unbounded_channel();
    let (clip_events_tx, clip_events_rx) = unbounded_channel();
    let (win, to_win) = WinTx::detached();
    let clip = ClipLink {
        events: clip_events_rx,
        win,
    };
    let task = tokio::task::spawn_local(async move {
        session(client_rd, client_wr, &ui, input_rx, clip).await
    });

    let server = FakeServer {
        reader: Framed::new(server_rd),
        writer: Framed::new(server_wr),
    };
    let window = FakeWindow {
        events,
        slot,
        notified,
        input: Some(input_tx),
        _clip_events: clip_events_tx,
        to_win,
    };
    (server, window, task)
}

/// The session's result once it ends.
pub async fn ended(task: JoinHandle<Result<()>>) -> Result<()> {
    tokio::time::timeout(WAIT, task)
        .await
        .expect("the session did not end")
        .expect("the session panicked")
}
