//! Text clipboard between Windows and the remote.
//!
//! Nothing is pushed. When the Windows clipboard changes to text the session
//! offers it; the remote asks for the bytes only when something there pastes.
//! The other way round, a remote offer puts a *delayed* `CF_UNICODETEXT` on
//! the Windows clipboard and Windows asks us to render it (`WM_RENDERFORMAT`)
//! only when an application pastes, which is when the text is fetched.
//!
//! Two halves, joined by channels: a [`Bridge`] on the session's thread runs
//! the shared transfer engine, and on Windows a thread of its own owns a
//! hidden window that sees clipboard changes and renders on demand. Files and
//! images are not forwarded yet; an offer without text withdraws ours.

// Only Windows has a clipboard thread to talk to; elsewhere the crate just
// has to build.
#![cfg_attr(not(windows), allow(dead_code))]

use std::cell::Cell;
use std::io::Cursor;
use std::rc::Rc;
#[cfg(windows)]
use std::sync::atomic::AtomicIsize;
use std::sync::mpsc::{channel, Sender};
#[cfg(windows)]
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use gliff_proto::clipboard::{forwardable_mimes, is_text_mime, MAX_ITEM, TEXT_MIME, WINDOW};
use gliff_proto::{ClipboardItem, ClipboardMsg};
use gliff_transport::clipboard::{Event, ReadSource, Transfers};
use tokio::sync::{mpsc, oneshot};

/// How long the Windows side waits for the local text to be read.
const READ_WAIT: Duration = Duration::from_secs(10);

/// From the Windows clipboard thread to the session.
pub enum FromWin {
    /// The clipboard changed to something not ours: text we may offer, or
    /// nothing we forward.
    Local { text: bool },
    /// An application pastes the remote's text: fetch it and answer.
    Paste {
        serial: u32,
        reply: Sender<Result<String, String>>,
    },
}

/// From the session to the Windows clipboard thread.
pub enum ToWin {
    /// The remote's selection changed: put (or withdraw) delayed text.
    Remote { serial: u32, text: bool },
    /// Read the text now on the Windows clipboard, for the remote's request.
    ReadText {
        reply: oneshot::Sender<Option<String>>,
    },
    /// Report the clipboard as it is, once the session is up.
    Resync,
    /// Leave the clipboard clean and end the thread.
    Quit,
}

/// The session's way to the clipboard thread.
#[derive(Clone)]
pub struct WinTx {
    tx: Sender<ToWin>,
    #[cfg(windows)]
    wake: Arc<AtomicIsize>,
}

impl WinTx {
    /// A sender with no thread behind it, and the receiving end, for tests.
    #[cfg(test)]
    pub fn detached() -> (Self, std::sync::mpsc::Receiver<ToWin>) {
        let (tx, rx) = channel();
        (
            Self {
                tx,
                #[cfg(windows)]
                wake: Arc::new(AtomicIsize::new(0)),
            },
            rx,
        )
    }

    /// False when the thread is gone.
    pub fn send(&self, cmd: ToWin) -> bool {
        let sent = self.tx.send(cmd).is_ok();
        #[cfg(windows)]
        if sent {
            win::wake(&self.wake);
        }
        sent
    }
}

/// The clipboard thread, to be shut down when the viewer ends.
pub struct ClipHandle {
    pub win: WinTx,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ClipHandle {
    pub fn shutdown(mut self) {
        let _ = self.win.send(ToWin::Quit);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// Start the clipboard thread; changes and pastes come back on `events`.
pub fn start(events: mpsc::UnboundedSender<FromWin>) -> ClipHandle {
    let (tx, rx) = channel();
    #[cfg(windows)]
    {
        let wake = Arc::new(AtomicIsize::new(0));
        let thread = win::spawn(rx, events, wake.clone());
        ClipHandle {
            win: WinTx { tx, wake },
            thread,
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (rx, events);
        ClipHandle {
            win: WinTx { tx },
            thread: None,
        }
    }
}

/// The session's half: runs the transfer engine for the clipboard thread.
pub struct Bridge {
    transfers: Transfers,
    win: WinTx,
    /// The serial of the offer we made for the Windows clipboard, while it
    /// still holds what we offered. Serial zero is a valid wire value, so it
    /// is an Option.
    local_serial: Rc<Cell<Option<u32>>>,
    /// Kept across revocations so a new offer never repeats a serial the
    /// remote still holds.
    last_serial: Cell<u32>,
    /// The serial of the remote's current text offer, if it has one.
    remote_serial: Cell<Option<u32>>,
}

impl Bridge {
    pub fn new(transfers: Transfers, win: WinTx) -> Self {
        Self {
            transfers,
            win,
            local_serial: Rc::new(Cell::new(None)),
            last_serial: Cell::new(0),
            remote_serial: Cell::new(None),
        }
    }

    /// Ask the clipboard thread to report what is on the clipboard now.
    pub fn resync(&self) {
        self.win.send(ToWin::Resync);
    }

    /// Nothing offered earlier stays readable: refuse new requests against
    /// it and stop what is being served.
    fn revoke_local(&self) {
        self.local_serial.set(None);
        self.transfers.abort_serving();
    }

    pub fn on_peer(&self, msg: ClipboardMsg, payload: Bytes) {
        match self.transfers.on_msg(msg, payload) {
            Some(Event::Offer {
                serial,
                mime_types,
                files,
            }) => {
                // The remote's selection now holds the clipboard, so ours is
                // no longer on offer.
                self.revoke_local();
                let text = mime_types.iter().any(|m| is_text_mime(m));
                if !files.is_empty() && !text {
                    tracing::info!("the remote copied files; only text is forwarded for now");
                }
                self.remote_serial.set(text.then_some(serial));
                self.win.send(ToWin::Remote { serial, text });
            }
            Some(Event::Request { id, serial, item }) => self.on_request(id, serial, item),
            None => {}
        }
    }

    fn on_request(&self, id: u32, serial: u32, item: ClipboardItem) {
        let wants_text = matches!(&item, ClipboardItem::Mime(m) if is_text_mime(m));
        if self.local_serial.get() != Some(serial) || !wants_text {
            tracing::debug!(id, serial, "clipboard request refused");
            self.transfers.refuse(id);
            return;
        }
        let (reply, rx) = oneshot::channel();
        if !self.win.send(ToWin::ReadText { reply }) {
            self.transfers.refuse(id);
            return;
        }
        let transfers = self.transfers.clone();
        let current = self.local_serial.clone();
        tokio::task::spawn_local(async move {
            let text = tokio::time::timeout(READ_WAIT, rx)
                .await
                .ok()
                .and_then(Result::ok)
                .flatten();
            // The clipboard may have changed while it was read.
            if current.get() != Some(serial) {
                transfers.refuse(id);
                return;
            }
            match text {
                Some(t) if t.len() as u64 <= MAX_ITEM => {
                    transfers.serve(id, ReadSource(Cursor::new(t.into_bytes())));
                }
                _ => transfers.refuse(id),
            }
        });
    }

    pub fn on_win(&self, event: FromWin) {
        match event {
            FromWin::Local { text } => {
                self.revoke_local();
                let serial = self.last_serial.get().wrapping_add(1);
                self.last_serial.set(serial);
                if text {
                    self.local_serial.set(Some(serial));
                }
                // An empty offer withdraws the previous one.
                let mime_types = if text {
                    forwardable_mimes(&[TEXT_MIME.to_string()])
                } else {
                    Vec::new()
                };
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    let _ = transfers
                        .send(ClipboardMsg::Offer {
                            serial,
                            mime_types,
                            files: Vec::new(),
                        })
                        .await;
                });
            }
            FromWin::Paste { serial, reply } => {
                if self.remote_serial.get() != Some(serial) {
                    let _ = reply.send(Err("the offer was replaced".into()));
                    return;
                }
                let transfers = self.transfers.clone();
                tokio::task::spawn_local(async move {
                    let (sink, mut chunks) = mpsc::channel::<Bytes>(WINDOW as usize + 2);
                    let collect = async {
                        let mut all = Vec::new();
                        while let Some(c) = chunks.recv().await {
                            all.extend_from_slice(&c);
                        }
                        all
                    };
                    let fetch = transfers.fetch(
                        ClipboardItem::Mime(TEXT_MIME.to_string()),
                        serial,
                        sink,
                        Some(MAX_ITEM),
                    );
                    let (fetched, bytes) = tokio::join!(fetch, collect);
                    let result = fetched.map_err(|e| e.to_string()).and_then(|_| {
                        String::from_utf8(bytes).map_err(|_| "the text is not UTF-8".to_string())
                    });
                    let _ = reply.send(result);
                });
            }
        }
    }
}

// The wire carries "\n" line ends and UTF-8; Windows text is "\r\n" UTF-16
// with a terminating NUL.

/// Windows text (UTF-16, maybe NUL-terminated) as wire text.
#[cfg_attr(not(windows), allow(dead_code))]
fn from_windows_text(units: &[u16]) -> String {
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end]).replace("\r\n", "\n")
}

/// Wire text as NUL-terminated Windows text.
#[cfg_attr(not(windows), allow(dead_code))]
fn to_windows_text(text: &str) -> Vec<u16> {
    let mut out: Vec<u16> = Vec::with_capacity(text.len() + 1);
    let mut prev_cr = false;
    for u in text.encode_utf16() {
        if u == u16::from(b'\n') && !prev_cr {
            out.push(u16::from(b'\r'));
        }
        prev_cr = u == u16::from(b'\r');
        out.push(u);
    }
    out.push(0);
    out
}

#[cfg(windows)]
mod win {
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use std::sync::mpsc::Receiver;
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc::UnboundedSender;
    use windows_sys::Win32::Foundation::{GlobalFree, HWND, LPARAM, LRESULT, WPARAM};
    use windows_sys::Win32::System::DataExchange::{
        AddClipboardFormatListener, CloseClipboard, EmptyClipboard, GetClipboardData,
        GetClipboardOwner, GetClipboardSequenceNumber, IsClipboardFormatAvailable, OpenClipboard,
        RegisterClipboardFormatW, RemoveClipboardFormatListener, SetClipboardData,
    };
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalSize, GlobalUnlock, GMEM_MOVEABLE,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, GetMessageW, PostMessageW, PostQuitMessage,
        RegisterClassW, HWND_MESSAGE, MSG, WM_APP, WM_CLIPBOARDUPDATE, WM_RENDERFORMAT, WNDCLASSW,
    };

    use super::{from_windows_text, to_windows_text, FromWin, ToWin};

    const CF_UNICODETEXT: u32 = 13;
    /// How long a paste waits for the remote's text before it gives up.
    const PASTE_WAIT: Duration = Duration::from_secs(10);

    struct State {
        rx: Receiver<ToWin>,
        events: UnboundedSender<FromWin>,
        /// The serial of the remote text now on offer on the clipboard.
        remote_serial: Option<u32>,
        /// Password managers mark what they copy with this format.
        exclude: u32,
        /// The clipboard sequence number of the text last offered, or 0 when
        /// what is on it is not on offer. A read of any other state is
        /// refused, so a copy made since (a password, say) is never served.
        offered_seq: u32,
    }

    thread_local! {
        // Only the clipboard thread's window procedure touches this. Never
        // hold the borrow across a Win32 call: some of them send messages
        // straight back to the window procedure.
        static STATE: RefCell<Option<State>> = const { RefCell::new(None) };
    }

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain([0]).collect()
    }

    /// Wake the thread so it drains its command queue.
    pub fn wake(hwnd: &AtomicIsize) {
        let h = hwnd.load(Ordering::Acquire);
        if h != 0 {
            // SAFETY: posting a message to a window handle is safe even if
            // the window is gone; the call then just fails.
            unsafe { PostMessageW(h as HWND, WM_APP, 0, 0) };
        }
    }

    pub fn spawn(
        rx: Receiver<ToWin>,
        events: UnboundedSender<FromWin>,
        wake: Arc<AtomicIsize>,
    ) -> Option<std::thread::JoinHandle<()>> {
        let started = std::thread::Builder::new()
            .name("gliff-clipboard".into())
            .spawn(move || run(rx, events, wake));
        match started {
            Ok(t) => Some(t),
            Err(e) => {
                tracing::warn!(error = %e, "cannot start the clipboard thread; no clipboard");
                None
            }
        }
    }

    fn run(rx: Receiver<ToWin>, events: UnboundedSender<FromWin>, wake: Arc<AtomicIsize>) {
        let class = wide("gliff-clipboard");
        // SAFETY: plain Win32 calls with valid arguments: `class` outlives
        // them, the window procedure is a function of this module, and `msg`
        // is a C struct that GetMessageW fills in.
        unsafe {
            let hinstance = GetModuleHandleW(std::ptr::null());
            let wc = WNDCLASSW {
                style: 0,
                lpfnWndProc: Some(wndproc),
                cbClsExtra: 0,
                cbWndExtra: 0,
                hInstance: hinstance,
                hIcon: std::ptr::null_mut(),
                hCursor: std::ptr::null_mut(),
                hbrBackground: std::ptr::null_mut(),
                lpszMenuName: std::ptr::null(),
                lpszClassName: class.as_ptr(),
            };
            RegisterClassW(&wc);
            let hwnd = CreateWindowExW(
                0,
                class.as_ptr(),
                class.as_ptr(),
                0,
                0,
                0,
                0,
                0,
                HWND_MESSAGE,
                std::ptr::null_mut(),
                hinstance,
                std::ptr::null(),
            );
            if hwnd.is_null() || AddClipboardFormatListener(hwnd) == 0 {
                tracing::warn!("cannot watch the clipboard; no clipboard");
                return;
            }
            let exclude = RegisterClipboardFormatW(
                wide("ExcludeClipboardContentFromMonitorProcessing").as_ptr(),
            );
            STATE.with(|s| {
                *s.borrow_mut() = Some(State {
                    rx,
                    events,
                    remote_serial: None,
                    exclude,
                    offered_seq: 0,
                });
            });
            wake.store(hwnd as isize, Ordering::Release);
            // Commands queued before the window existed.
            pump(hwnd);
            let mut msg: MSG = std::mem::zeroed();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                windows_sys::Win32::UI::WindowsAndMessaging::DispatchMessageW(&msg);
            }
            wake.store(0, Ordering::Release);
            RemoveClipboardFormatListener(hwnd);
            DestroyWindow(hwnd);
        }
        STATE.with(|s| *s.borrow_mut() = None);
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        // A panic must not unwind into Windows.
        let handled = std::panic::catch_unwind(|| match msg {
            WM_APP => {
                pump(hwnd);
                true
            }
            WM_CLIPBOARDUPDATE => {
                report_local(hwnd);
                true
            }
            WM_RENDERFORMAT => {
                if wparam as u32 == CF_UNICODETEXT {
                    render_text();
                }
                true
            }
            _ => false,
        });
        if matches!(handled, Ok(true)) {
            return 0;
        }
        // SAFETY: the arguments are the ones Windows gave us.
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    fn next_command() -> Option<ToWin> {
        STATE.with(|s| {
            let s = s.borrow();
            s.as_ref()?.rx.try_recv().ok()
        })
    }

    fn pump(hwnd: HWND) {
        while let Some(cmd) = next_command() {
            match cmd {
                ToWin::Remote { serial, text } => {
                    STATE.with(|s| {
                        if let Some(s) = s.borrow_mut().as_mut() {
                            s.remote_serial = text.then_some(serial);
                        }
                    });
                    if text {
                        offer_delayed_text(hwnd);
                    } else {
                        clear_if_ours(hwnd);
                    }
                }
                ToWin::ReadText { reply } => {
                    let _ = reply.send(read_text(hwnd));
                }
                ToWin::Resync => report_local(hwnd),
                ToWin::Quit => {
                    clear_if_ours(hwnd);
                    // SAFETY: ends this thread's message loop.
                    unsafe { PostQuitMessage(0) };
                    return;
                }
            }
        }
    }

    /// Open the clipboard, retrying: another program may hold it briefly.
    fn open(hwnd: HWND) -> bool {
        for _ in 0..10 {
            // SAFETY: `hwnd` is our window.
            if unsafe { OpenClipboard(hwnd) } != 0 {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    fn owner_is_us(hwnd: HWND) -> bool {
        // SAFETY: no arguments; returns a handle or null.
        unsafe { GetClipboardOwner() == hwnd }
    }

    /// Tell the session what the clipboard holds, unless it is ours.
    fn report_local(hwnd: HWND) {
        if owner_is_us(hwnd) {
            return;
        }
        let exclude = STATE.with(|s| s.borrow().as_ref().map_or(0, |s| s.exclude));
        // The sequence number comes first: a copy made after it changes the
        // number, so the formats checked next can never be those of newer
        // content that this number would then vouch for.
        // SAFETY: takes no arguments.
        let seq = unsafe { GetClipboardSequenceNumber() };
        // SAFETY: format queries take no pointers.
        let text = unsafe {
            IsClipboardFormatAvailable(CF_UNICODETEXT) != 0
                && !(exclude != 0 && IsClipboardFormatAvailable(exclude) != 0)
        };
        STATE.with(|s| {
            if let Some(s) = s.borrow_mut().as_mut() {
                s.offered_seq = if text { seq } else { 0 };
                let _ = s.events.send(FromWin::Local { text });
            }
        });
    }

    /// Own the clipboard with text that is rendered only when pasted.
    fn offer_delayed_text(hwnd: HWND) {
        if !open(hwnd) {
            tracing::warn!("clipboard busy; the remote's text was not offered");
            return;
        }
        // SAFETY: we have the clipboard open; a null handle promises the
        // data later, in WM_RENDERFORMAT.
        unsafe {
            EmptyClipboard();
            SetClipboardData(CF_UNICODETEXT, std::ptr::null_mut());
            CloseClipboard();
        }
    }

    /// Empty the clipboard if what is on it is only our promise.
    fn clear_if_ours(hwnd: HWND) {
        if owner_is_us(hwnd) && open(hwnd) {
            // SAFETY: the clipboard is open.
            unsafe {
                EmptyClipboard();
                CloseClipboard();
            }
        }
    }

    /// The text on the clipboard, or None if there is none or it is ours.
    fn read_text(hwnd: HWND) -> Option<String> {
        if owner_is_us(hwnd) || !open(hwnd) {
            return None;
        }
        let offered = STATE.with(|s| s.borrow().as_ref().map_or(0, |s| s.offered_seq));
        // SAFETY: takes no arguments.
        if offered == 0 || offered != unsafe { GetClipboardSequenceNumber() } {
            // SAFETY: the clipboard is open.
            unsafe { CloseClipboard() };
            return None;
        }
        // SAFETY: the clipboard is open, so the handle stays valid until it
        // is closed; the lock gives a pointer to GlobalSize bytes, read as
        // UTF-16 units no further than that.
        let text = unsafe {
            let handle = GetClipboardData(CF_UNICODETEXT);
            let mut text = None;
            if !handle.is_null() {
                let ptr = GlobalLock(handle);
                if !ptr.is_null() {
                    let units = GlobalSize(handle) / 2;
                    text = Some(from_windows_text(std::slice::from_raw_parts(
                        ptr as *const u16,
                        units,
                    )));
                    GlobalUnlock(handle);
                }
            }
            CloseClipboard();
            text
        };
        text
    }

    /// An application pastes: get the remote's text and hand it over. The
    /// requester has the clipboard open, so there is no OpenClipboard here.
    fn render_text() {
        let Some((serial, events)) = STATE.with(|s| {
            let s = s.borrow();
            let s = s.as_ref()?;
            Some((s.remote_serial?, s.events.clone()))
        }) else {
            return;
        };
        let (reply, answer) = std::sync::mpsc::channel();
        if events.send(FromWin::Paste { serial, reply }).is_err() {
            return;
        }
        let Ok(Ok(text)) = answer.recv_timeout(PASTE_WAIT) else {
            return;
        };
        let units = to_windows_text(&text);
        let bytes = units.len() * 2;
        // SAFETY: a moveable block of exactly `bytes` bytes is filled under
        // its lock and then handed to the clipboard, which owns it on
        // success; on failure we free it.
        unsafe {
            let block = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if block.is_null() {
                return;
            }
            let ptr = GlobalLock(block);
            if ptr.is_null() {
                GlobalFree(block);
                return;
            }
            std::ptr::copy_nonoverlapping(units.as_ptr() as *const u8, ptr as *mut u8, bytes);
            GlobalUnlock(block);
            if SetClipboardData(CF_UNICODETEXT, block).is_null() {
                GlobalFree(block);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gliff_proto::clipboard::TEXT_MIMES;
    use gliff_transport::clipboard::{outbound_channel, Side};
    use tokio::task::LocalSet;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn text_crosses_with_the_right_line_ends() {
        let units: Vec<u16> = "a\r\nb\r\n\0junk".encode_utf16().collect();
        assert_eq!(from_windows_text(&units), "a\nb\n");
        let back = to_windows_text("a\nb\r\nc\u{e9}\u{1F600}");
        assert_eq!(*back.last().unwrap(), 0);
        let s = String::from_utf16(&back[..back.len() - 1]).unwrap();
        assert_eq!(s, "a\r\nb\r\nc\u{e9}\u{1F600}");
        assert_eq!(to_windows_text(""), vec![0]);
    }

    #[test]
    fn a_remote_text_offer_reaches_the_clipboard_thread_and_replaces_ours() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, _out_rx) = outbound_channel();
            let (win, from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            bridge.on_win(FromWin::Local { text: true });
            assert_eq!(bridge.local_serial.get(), Some(1));
            bridge.on_peer(
                ClipboardMsg::Offer {
                    serial: 9,
                    mime_types: vec!["text/plain".into()],
                    files: vec![],
                },
                Bytes::new(),
            );
            assert_eq!(bridge.local_serial.get(), None);
            assert_eq!(bridge.remote_serial.get(), Some(9));
            assert!(matches!(
                from_bridge.try_recv(),
                Ok(ToWin::Remote {
                    serial: 9,
                    text: true
                })
            ));
        });
    }

    #[test]
    fn an_offer_without_text_withdraws_the_remote_text() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, _out_rx) = outbound_channel();
            let (win, from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            bridge.on_peer(
                ClipboardMsg::Offer {
                    serial: 3,
                    mime_types: vec!["image/png".into()],
                    files: vec![],
                },
                Bytes::new(),
            );
            assert_eq!(bridge.remote_serial.get(), None);
            assert!(matches!(
                from_bridge.try_recv(),
                Ok(ToWin::Remote {
                    serial: 3,
                    text: false
                })
            ));
        });
    }

    #[test]
    fn a_local_change_is_offered_and_a_non_text_change_withdraws_it() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, mut out_rx) = outbound_channel();
            let (win, _from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            bridge.on_win(FromWin::Local { text: true });
            let (m, _) = out_rx.recv().await.unwrap();
            match m {
                ClipboardMsg::Offer {
                    serial,
                    mime_types,
                    files,
                } => {
                    assert_eq!(serial, 1);
                    assert!(mime_types.iter().any(|m| m == TEXT_MIME));
                    assert!(mime_types.len() == TEXT_MIMES.len() && files.is_empty());
                }
                other => panic!("{other:?}"),
            }
            bridge.on_win(FromWin::Local { text: false });
            let (m, _) = out_rx.recv().await.unwrap();
            assert_eq!(
                m,
                ClipboardMsg::Offer {
                    serial: 2,
                    mime_types: vec![],
                    files: vec![]
                }
            );
            assert_eq!(bridge.local_serial.get(), None);
        });
    }

    #[test]
    fn a_request_against_a_replaced_or_non_text_offer_is_refused() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, mut out_rx) = outbound_channel();
            let (win, from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            bridge.on_win(FromWin::Local { text: true });
            let _offer = out_rx.recv().await.unwrap();
            // Stale serial.
            bridge.on_peer(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 99,
                    item: ClipboardItem::Mime(TEXT_MIME.into()),
                },
                Bytes::new(),
            );
            // Right serial, wrong kind of item.
            bridge.on_peer(
                ClipboardMsg::Request {
                    id: 4,
                    serial: 1,
                    item: ClipboardItem::File(0),
                },
                Bytes::new(),
            );
            for id in [2, 4] {
                let (m, _) = out_rx.recv().await.unwrap();
                assert_eq!(m, ClipboardMsg::Abort { id });
            }
            assert!(from_bridge.try_recv().is_err());
        });
    }

    #[test]
    fn a_request_is_served_from_the_text_the_clipboard_thread_reads() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, mut out_rx) = outbound_channel();
            let (win, from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            bridge.on_win(FromWin::Local { text: true });
            let _offer = out_rx.recv().await.unwrap();
            bridge.on_peer(
                ClipboardMsg::Request {
                    id: 2,
                    serial: 1,
                    item: ClipboardItem::Mime("text/plain".into()),
                },
                Bytes::new(),
            );
            let Ok(ToWin::ReadText { reply }) = from_bridge.try_recv() else {
                panic!("no read request");
            };
            reply.send(Some("h\u{e9}llo".to_string())).unwrap();
            let (m, payload) = out_rx.recv().await.unwrap();
            assert!(matches!(
                m,
                ClipboardMsg::Data {
                    id: 2,
                    offset: 0,
                    ..
                }
            ));
            assert_eq!(&payload[..], "h\u{e9}llo".as_bytes());
        });
    }

    #[test]
    fn a_paste_fetches_the_remote_text() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, mut out_rx) = outbound_channel();
            let (win, _from_bridge) = WinTx::detached();
            let transfers = Transfers::new(out, Side::Client);
            let bridge = Bridge::new(transfers.clone(), win);
            bridge.on_peer(
                ClipboardMsg::Offer {
                    serial: 5,
                    mime_types: vec!["text/plain;charset=utf-8".into()],
                    files: vec![],
                },
                Bytes::new(),
            );
            let (reply, answer) = channel();
            bridge.on_win(FromWin::Paste { serial: 5, reply });
            let (m, _) = out_rx.recv().await.unwrap();
            let ClipboardMsg::Request { id, serial, item } = m else {
                panic!("no request");
            };
            assert_eq!(serial, 5);
            assert_eq!(item, ClipboardItem::Mime(TEXT_MIME.into()));
            let body = Bytes::from_static("pasted \u{e9}".as_bytes());
            transfers.on_msg(
                ClipboardMsg::Data {
                    id,
                    offset: 0,
                    data_len: body.len() as u32,
                    done: true,
                },
                body,
            );
            // Let the fetch finish, then read what it answered.
            for _ in 0..50 {
                if let Ok(r) = answer.try_recv() {
                    assert_eq!(r, Ok("pasted \u{e9}".to_string()));
                    return;
                }
                tokio::task::yield_now().await;
            }
            panic!("no answer");
        });
    }

    #[test]
    fn a_paste_against_a_replaced_offer_fails() {
        let rt = rt();
        LocalSet::new().block_on(&rt, async {
            let (out, _out_rx) = outbound_channel();
            let (win, _from_bridge) = WinTx::detached();
            let bridge = Bridge::new(Transfers::new(out, Side::Client), win);
            let (reply, answer) = channel();
            bridge.on_win(FromWin::Paste { serial: 1, reply });
            assert!(answer.try_recv().unwrap().is_err());
        });
    }
}
