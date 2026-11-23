//! A fullscreen window that changes every frame, for testing a remote
//! session under load: every capture frame of the monitor it covers differs
//! from the one before, as in video, a game or fast scrolling.
//!
//!   gliff-gnome-animate [SECS [RECTS [CONNECTOR]]]
//!                                 run for SECS seconds (default 60), drawing
//!                                 RECTS coloured rectangles a frame (default
//!                                 300), then print how many frames it drew
//!                                 and how many of them the compositor
//!                                 reported as presented
//!                                 With CONNECTOR (a name or its start, such
//!                                 as Meta- for a private screen) it waits up
//!                                 to 30 s for that monitor and covers it,
//!                                 not the primary one
//!
//! It draws the rectangles and a sweeping bar per frame, so the picture is
//! busy enough to need real bit rate from the encoder. More rectangles cost
//! the test window itself more time: with few, it keeps up with the display.

use std::cell::{Cell, RefCell};
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;

/// What the frame clock says about frames after they were drawn. On Wayland
/// the presentation time comes from the compositor's feedback for each
/// committed frame, so distinct times count what it really put on screen.
#[derive(Default)]
struct Presented {
    /// The next frame counter to look at.
    next: i64,
    /// Frames with a presentation time that differs from the one before.
    shown: u32,
    /// Frames complete but without a presentation time.
    untimed: u32,
    /// Frames not yet complete when they were looked at (three behind the
    /// clock), and not looked at again.
    open: u32,
    last: i64,
    /// Microseconds between consecutive distinct presentation times.
    gaps: Vec<i64>,
    /// Microseconds between refreshes, as the clock last reported.
    refresh: i64,
}

impl Presented {
    /// Look at every frame at least three behind the clock's current one.
    fn update(&mut self, clock: &gtk4::gdk::FrameClock) {
        let upto = clock.frame_counter() - 3;
        if self.next == 0 {
            self.next = upto.max(1);
        }
        while self.next <= upto {
            if let Some(t) = clock.timings(self.next) {
                if !t.is_complete() {
                    self.open += 1;
                } else if t.presentation_time() == 0 {
                    self.untimed += 1;
                } else if t.presentation_time() != self.last {
                    if self.last != 0 {
                        self.gaps.push(t.presentation_time() - self.last);
                    }
                    self.last = t.presentation_time();
                    self.shown += 1;
                    self.refresh = t.refresh_interval();
                }
            }
            self.next += 1;
        }
    }

    fn median_gap_ms(&self) -> f64 {
        let mut g = self.gaps.clone();
        g.sort_unstable();
        g.get(g.len() / 2).map_or(0.0, |&u| u as f64 / 1000.0)
    }
}

/// A cheap deterministic stream of numbers for the picture.
fn next(x: &mut u32) -> f64 {
    *x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    f64::from(*x >> 8) / f64::from(1u32 << 24)
}

fn main() {
    let secs: u64 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(60);
    let rects: u32 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let connector = std::env::args().nth(3);
    if gtk4::init().is_err() {
        println!("[FAIL] gtk: cannot open a display");
        std::process::exit(1);
    }
    // A monitor that does not exist yet (a private screen appears once a
    // session starts): wait for it, letting GDK see the change.
    let target = connector.as_deref().and_then(|want| {
        let display = gtk4::gdk::Display::default()?;
        let until = Instant::now() + Duration::from_secs(30);
        loop {
            let monitors = display.monitors();
            let found = (0..monitors.n_items())
                .filter_map(|i| monitors.item(i)?.downcast::<gtk4::gdk::Monitor>().ok())
                .find(|m| m.connector().is_some_and(|c| c.starts_with(want)));
            if found.is_some() || Instant::now() >= until {
                return found;
            }
            while gtk4::glib::MainContext::default().iteration(false) {}
            std::thread::sleep(Duration::from_millis(100));
        }
    });
    if let (Some(name), None) = (&connector, &target) {
        println!("[FAIL] monitor: none named {name:?} appeared");
        std::process::exit(1);
    }
    let frame = Rc::new(Cell::new(0u32));
    let paints = Rc::new(Cell::new(0u32));
    let presented = Rc::new(RefCell::new(Presented::default()));
    let area = gtk4::DrawingArea::new();
    {
        let (frame, paints) = (frame.clone(), paints.clone());
        area.set_draw_func(move |_, cr, w, h| {
            paints.set(paints.get() + 1);
            let n = frame.get();
            let (w, h) = (f64::from(w), f64::from(h));
            cr.set_source_rgb(0.08, 0.08, 0.1);
            let _ = cr.paint();
            let mut seed = n.wrapping_mul(2_654_435_761);
            for _ in 0..rects {
                cr.set_source_rgb(next(&mut seed), next(&mut seed), next(&mut seed));
                cr.rectangle(
                    next(&mut seed) * w,
                    next(&mut seed) * h,
                    10.0 + next(&mut seed) * 120.0,
                    10.0 + next(&mut seed) * 80.0,
                );
                let _ = cr.fill();
            }
            // A bar sweeping across, so there is always large coherent motion.
            let x = (f64::from(n % 240) / 240.0) * w;
            cr.set_source_rgb(1.0, 1.0, 1.0);
            cr.rectangle(x, 0.0, 40.0, h);
            let _ = cr.fill();
        });
    }
    area.add_tick_callback({
        let (frame, presented) = (frame.clone(), presented.clone());
        move |area, clock| {
            frame.set(frame.get().wrapping_add(1));
            presented.borrow_mut().update(clock);
            area.queue_draw();
            gtk4::glib::ControlFlow::Continue
        }
    });
    let window = gtk4::Window::new();
    window.set_title(Some("gliff animate"));
    window.set_child(Some(&area));
    match &target {
        Some(monitor) => window.fullscreen_on_monitor(monitor),
        None => window.fullscreen(),
    }
    window.present();

    let started = Instant::now();
    let main_loop = gtk4::glib::MainLoop::new(None, false);
    {
        let main_loop = main_loop.clone();
        gtk4::glib::timeout_add_local(Duration::from_millis(200), move || {
            if started.elapsed() >= Duration::from_secs(secs) {
                main_loop.quit();
                return gtk4::glib::ControlFlow::Break;
            }
            gtk4::glib::ControlFlow::Continue
        });
    }
    main_loop.run();
    let n = frame.get();
    window.close();
    let secs = started.elapsed().as_secs_f64();
    println!(
        "drew {n} frames in {secs:.1} s ({:.1} per second)",
        f64::from(n) / secs
    );
    let p = presented.borrow();
    println!(
        "painted {} times; the compositor presented {} ({:.1} per second), {} had no presentation time, {} incomplete when looked at; median gap {:.1} ms (refresh interval {:.1} ms)",
        paints.get(),
        p.shown,
        f64::from(p.shown) / secs,
        p.untimed,
        p.open,
        p.median_gap_ms(),
        p.refresh as f64 / 1000.0
    );
}
