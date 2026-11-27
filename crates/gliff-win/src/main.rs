//! gliff for Windows: a window that shows a remote desktop served by
//! `gliff-server` over ssh, and sends the mouse and keyboard back.
//!
//! Run it from a terminal: ssh asks for a password there. Video is one
//! 4:2:0 H.264 stream decoded on the CPU.

mod grab;
mod keys;
mod net;
mod view;

use std::collections::HashSet;
use std::num::NonZeroU32;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use clap::Parser;
use gliff_proto::{Axis, ClientMsg};
use gliff_transport::SshTarget;
use grab::Grab;
use net::{Geometry, Slot, Ui, UiEvent};
use tokio::sync::mpsc::UnboundedSender;
use winit::application::ApplicationHandler;
use winit::dpi::{LogicalSize, PhysicalPosition};
use winit::event::{ElementState, MouseScrollDelta, TouchPhase, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::PhysicalKey;
use winit::window::{Window, WindowId};

/// A scroll wheel notch is this many logical pixels, as in the GTK viewer.
const LINE_PX: f64 = 15.0;
/// A resize is sent once the window has stopped changing for this long.
const RESIZE_SETTLE: Duration = Duration::from_millis(150);

#[derive(Parser)]
#[command(
    version,
    about = "Show a remote desktop served by gliff-server over ssh"
)]
struct Cli {
    /// `user@host`, or an ssh config alias.
    host: String,
    /// Path of `gliff-server` on the remote machine.
    #[arg(long, default_value = "gliff-server")]
    server_bin: String,
    /// An extra argument for gliff-server (repeatable), e.g. `--server-arg=--headless`.
    #[arg(long = "server-arg", allow_hyphen_values = true)]
    server_args: Vec<String>,
    /// An extra argument for ssh (repeatable), e.g. `--ssh-arg=-J --ssh-arg=jump`.
    #[arg(long = "ssh-arg", allow_hyphen_values = true)]
    ssh_args: Vec<String>,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "gliff_win=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let mut target = SshTarget::new(cli.host.clone());
    target.server_bin = cli.server_bin;
    target.server_args = cli.server_args;
    target.ssh_args = cli.ssh_args;

    let event_loop = EventLoop::<UiEvent>::with_user_event().build()?;
    // winit's raw keyboard registration makes Windows skip the shortcut hook
    // while the window has focus, and the viewer has no use for device events.
    event_loop.listen_device_events(winit::event_loop::DeviceEvents::Never);
    let (ui, slot, notified) = Ui::new(event_loop.create_proxy());
    let (input_tx, input_rx) = tokio::sync::mpsc::unbounded_channel();
    let grab = {
        let proxy = event_loop.create_proxy();
        Grab::install(input_tx.clone(), move || {
            let _ = proxy.send_event(UiEvent::GrabReleased);
        })
    };
    let worker = net::spawn(target, ui, input_rx)?;

    let mut app = App {
        host: cli.host,
        slot,
        notified,
        input: Some(input_tx),
        window: None,
        surface: None,
        picture: None,
        cursor: None,
        pressed: HashSet::new(),
        resize_due: None,
        last_resize: None,
        failure: None,
        grab,
        released: false,
        connected: false,
    };
    event_loop.run_app(&mut app)?;
    // The window is gone: the worker sees its input close, says Bye and ends.
    drop(app.input.take());
    // The hook holds a sender of its own, which would keep the session open.
    app.grab.close();
    let _ = worker.join();
    match app.failure {
        Some(reason) => anyhow::bail!(reason),
        None => Ok(()),
    }
}

type Surface = softbuffer::Surface<Arc<Window>, Arc<Window>>;

struct App {
    host: String,
    slot: Slot,
    notified: Arc<AtomicBool>,
    /// Closed (taken) when the window goes, which ends the session.
    input: Option<UnboundedSender<ClientMsg>>,
    window: Option<Arc<Window>>,
    surface: Option<Surface>,
    /// The picture on screen, with the geometry it was encoded under.
    picture: Option<net::Picture>,
    /// The pointer in window pixels, for the mapping to the remote.
    cursor: Option<PhysicalPosition<f64>>,
    /// Keys the remote believes are down, released when focus is lost.
    pressed: HashSet<u32>,
    resize_due: Option<Instant>,
    last_resize: Option<(u32, u32, u32)>,
    failure: Option<String>,
    grab: Grab,
    /// The user gave the keyboard back with the release hotkey; a click in
    /// the window takes it again.
    released: bool,
    /// The session is up, so keys have somewhere to go.
    connected: bool,
}

impl App {
    fn send(&self, msg: ClientMsg) {
        if let Some(tx) = &self.input {
            let _ = tx.send(msg);
        }
    }

    fn geometry(&self) -> Option<Geometry> {
        self.picture.as_ref().map(|p| p.geometry)
    }

    fn area(&self) -> (u32, u32) {
        self.window.as_ref().map_or((0, 0), |w| {
            let s = w.inner_size();
            (s.width, s.height)
        })
    }

    /// The window point as a remote pointer position, once a picture says
    /// how the stream is laid out.
    fn to_remote(&self, p: PhysicalPosition<f64>) -> Option<(f64, f64)> {
        let g = self.geometry()?;
        let fit = view::fit(g.view, self.area())?;
        Some(view::to_remote(&fit, g.stream, g.scale_milli, p.x, p.y))
    }

    fn redraw(&mut self) {
        let (Some(window), Some(surface)) = (&self.window, &mut self.surface) else {
            return;
        };
        let size = window.inner_size();
        let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) else {
            return;
        };
        if surface.resize(w, h).is_err() {
            return;
        }
        let Ok(mut buf) = surface.buffer_mut() else {
            return;
        };
        let (bw, bh) = (size.width as usize, size.height as usize);
        let fit = self
            .picture
            .as_ref()
            .and_then(|p| view::fit(p.geometry.view, (size.width, size.height)));
        match (&self.picture, fit) {
            (Some(p), Some(fit)) => view::draw(&mut buf, bw, bh, Some(&p.bgra), &fit),
            _ => buf.fill(0),
        }
        window.pre_present_notify();
        let _ = buf.present();
    }

    /// Start grabbing the keyboard if the session is up, the user has not
    /// given the keyboard back, and the window has the focus.
    fn grab_if_ready(&self) {
        if self.connected && !self.released && self.window.as_ref().is_some_and(|w| w.has_focus()) {
            self.grab.set_active(true);
        }
    }

    fn title(&self) -> String {
        if self.released {
            format!("gliff - {} (keys released, click to capture)", self.host)
        } else {
            format!("gliff - {}", self.host)
        }
    }

    fn update_title(&self) {
        if let Some(w) = &self.window {
            w.set_title(&self.title());
        }
    }

    fn release_keys(&mut self) {
        for keycode in std::mem::take(&mut self.pressed) {
            self.send(ClientMsg::Key {
                keycode,
                pressed: false,
            });
        }
    }

    fn request_resize(&mut self) {
        let Some(window) = &self.window else {
            return;
        };
        let size = window.inner_size();
        let scale = window.scale_factor();
        let key = (size.width, size.height, (scale * 1000.0) as u32);
        if size.width < 64 || size.height < 64 || self.last_resize == Some(key) {
            return;
        }
        self.last_resize = Some(key);
        self.send(ClientMsg::Resize {
            width: size.width,
            height: size.height,
            scale: scale as f32,
        });
    }
}

impl ApplicationHandler<UiEvent> for App {
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.window.is_some() {
            return;
        }
        let attributes = Window::default_attributes()
            .with_title(self.title())
            .with_inner_size(LogicalSize::new(1280.0, 720.0));
        let window = match event_loop.create_window(attributes) {
            Ok(w) => Arc::new(w),
            Err(e) => {
                self.failure = Some(format!("cannot open a window: {e}"));
                event_loop.exit();
                return;
            }
        };
        let surface = softbuffer::Context::new(window.clone())
            .and_then(|context| softbuffer::Surface::new(&context, window.clone()));
        match surface {
            Ok(s) => self.surface = Some(s),
            Err(e) => {
                self.failure = Some(format!("cannot draw into the window: {e}"));
                event_loop.exit();
                return;
            }
        }
        self.window = Some(window);
    }

    fn user_event(&mut self, event_loop: &ActiveEventLoop, event: UiEvent) {
        match event {
            UiEvent::Frame => {
                self.notified.store(false, Ordering::Release);
                let next = self.slot.lock().unwrap_or_else(|e| e.into_inner()).take();
                if let Some(p) = next {
                    self.picture = Some(p);
                    if let Some(w) = &self.window {
                        w.request_redraw();
                    }
                }
            }
            UiEvent::Connected { headless } => {
                tracing::info!(headless, "session up");
                // Tell the server the window's size, as the GTK viewer does.
                self.last_resize = None;
                self.request_resize();
                // Not before now: while ssh may still ask for a password the
                // user needs Alt+Tab to reach it, and a key sent before the
                // session is up goes nowhere.
                self.connected = true;
                self.grab_if_ready();
            }
            UiEvent::GrabReleased => {
                self.released = true;
                self.update_title();
                // A focus event queued before this may have turned the hook
                // back on.
                self.grab.set_active(false);
                // Keys now reach this window as ordinary input; they stay
                // local, and the ones already sent are let go.
                self.release_keys();
            }
            UiEvent::Closed(reason) => {
                if let Some(reason) = &reason {
                    tracing::error!("{reason}");
                }
                self.failure = reason;
                event_loop.exit();
            }
        }
    }

    fn window_event(&mut self, event_loop: &ActiveEventLoop, _: WindowId, event: WindowEvent) {
        match event {
            WindowEvent::CloseRequested => {
                self.send(ClientMsg::Bye);
                event_loop.exit();
            }
            WindowEvent::RedrawRequested => self.redraw(),
            WindowEvent::Resized(_) | WindowEvent::ScaleFactorChanged { .. } => {
                self.resize_due = Some(Instant::now() + RESIZE_SETTLE);
                if let Some(w) = &self.window {
                    w.request_redraw();
                }
            }
            WindowEvent::Focused(true) => {
                tracing::debug!(
                    connected = self.connected,
                    released = self.released,
                    has_focus = self.window.as_ref().is_some_and(|w| w.has_focus()),
                    "window focused"
                );
                self.grab_if_ready();
            }
            WindowEvent::Focused(false) => {
                tracing::debug!("window unfocused");
                self.grab.set_active(false);
                self.release_keys();
            }
            WindowEvent::CursorMoved { position, .. } => {
                self.cursor = Some(position);
                if let Some((x, y)) = self.to_remote(position) {
                    self.send(ClientMsg::PointerMotion { x, y });
                }
            }
            WindowEvent::MouseInput { state, button, .. } => {
                if state == ElementState::Pressed && self.released {
                    self.released = false;
                    self.update_title();
                    self.grab_if_ready();
                }
                let Some(button) = keys::evdev_button(button) else {
                    return;
                };
                // The pointer may not have moved since the window opened.
                if let Some((x, y)) = self.cursor.and_then(|p| self.to_remote(p)) {
                    self.send(ClientMsg::PointerMotion { x, y });
                }
                self.send(ClientMsg::PointerButton {
                    button,
                    pressed: state == ElementState::Pressed,
                });
            }
            WindowEvent::MouseWheel { delta, phase, .. } => self.scroll(delta, phase),
            WindowEvent::KeyboardInput {
                event,
                is_synthetic,
                ..
            } => {
                // Presses winit invents when focus returns are not the
                // user's; a held key repeats on the remote by itself.
                tracing::debug!(
                    key = ?event.physical_key,
                    state = ?event.state,
                    is_synthetic,
                    repeat = event.repeat,
                    released = self.released,
                    "window key"
                );
                if is_synthetic || event.repeat || self.released {
                    return;
                }
                let PhysicalKey::Code(code) = event.physical_key else {
                    return;
                };
                let Some(keycode) = keys::evdev_key(code) else {
                    return;
                };
                let pressed = event.state == ElementState::Pressed;
                let changed = if pressed {
                    self.pressed.insert(keycode)
                } else {
                    self.pressed.remove(&keycode)
                };
                if changed {
                    self.send(ClientMsg::Key { keycode, pressed });
                }
            }
            _ => {}
        }
    }

    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        match self.resize_due {
            Some(due) if Instant::now() >= due => {
                self.resize_due = None;
                self.request_resize();
                event_loop.set_control_flow(ControlFlow::Wait);
            }
            Some(due) => event_loop.set_control_flow(ControlFlow::WaitUntil(due)),
            None => event_loop.set_control_flow(ControlFlow::Wait),
        }
    }
}

impl App {
    /// winit reports a wheel up as a positive `y`; the protocol, like GTK,
    /// counts scrolling down as positive.
    fn scroll(&mut self, delta: MouseScrollDelta, phase: TouchPhase) {
        let (dx, dy, discrete) = match delta {
            MouseScrollDelta::LineDelta(x, y) => (
                f64::from(x) * LINE_PX,
                -f64::from(y) * LINE_PX,
                Some((x, -y)),
            ),
            MouseScrollDelta::PixelDelta(p) => (p.x, -p.y, None),
        };
        let stop = phase == TouchPhase::Ended;
        for (axis, value, step) in [
            (Axis::Vertical, dy, discrete.map(|d| d.1)),
            (Axis::Horizontal, dx, discrete.map(|d| d.0)),
        ] {
            if value == 0.0 && !stop {
                continue;
            }
            self.send(ClientMsg::PointerAxis {
                axis,
                value,
                discrete: step.map(|s| s.signum() as i32),
                stop,
            });
        }
    }
}
