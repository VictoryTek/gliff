//! A fullscreen window that changes every frame, for testing a remote
//! session under load: every capture frame of the monitor it covers differs
//! from the one before, as in video, a game or fast scrolling.
//!
//!   gliff-gnome-animate [SECS [RECTS|solid:RRGGBB [CONNECTOR [HOW]]]]
//!                                 run for SECS seconds (default 60), drawing
//!                                 RECTS coloured rectangles a frame (default
//!                                 300), then print how many frames it drew
//!                                 and how many of them the compositor
//!                                 reported as presented.
//!                                 With solid:RRGGBB it paints that one colour
//!                                 and nothing else (the picture a decoding
//!                                 test can recognise).
//!                                 With CONNECTOR (a name or its start, such
//!                                 as Meta- for a private screen) it waits up
//!                                 to 30 s for that monitor and covers it,
//!                                 not the primary one. HOW is fullscreen
//!                                 (the default; the window is put back on
//!                                 the monitor when it leaves it), or
//!                                 maximized or floating: the window starts
//!                                 fullscreen there, is then let out of it,
//!                                 and is only watched. It counts once the
//!                                 window has been seen on the monitor for
//!                                 1.6 s after that; the end of the run says
//!                                 when that was and how often, and when, the
//!                                 window left the monitor after it
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
    let second = std::env::args().nth(2);
    let rects: u32 = second
        .as_deref()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let solid: Option<(f64, f64, f64)> = second
        .as_deref()
        .and_then(|s| s.strip_prefix("solid:"))
        .filter(|h| h.len() == 6 && h.bytes().all(|b| b.is_ascii_hexdigit()))
        .and_then(|h| u32::from_str_radix(h, 16).ok())
        .map(|v| {
            (
                f64::from((v >> 16) & 0xff) / 255.0,
                f64::from((v >> 8) & 0xff) / 255.0,
                f64::from(v & 0xff) / 255.0,
            )
        });
    if second.as_deref().is_some_and(|s| s.starts_with("solid:")) && solid.is_none() {
        println!("[FAIL] argument: solid: needs six hex digits, RRGGBB");
        std::process::exit(2);
    }
    let connector = std::env::args().nth(3);
    let how = std::env::args()
        .nth(4)
        .unwrap_or_else(|| "fullscreen".into());
    if !["fullscreen", "maximized", "floating"].contains(&how.as_str()) {
        println!("[FAIL] argument: HOW is fullscreen, maximized or floating");
        std::process::exit(2);
    }
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
            if let Some((r, g, b)) = solid {
                cr.set_source_rgb(r, g, b);
                let _ = cr.paint();
                return;
            }
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
            if solid.is_none() {
                area.queue_draw();
            }
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
    // A private screen changes mode under the window (resizes, scales), and
    // Mutter has been seen to move the window off it. With a CONNECTOR, put it
    // back, and count how often that was needed.
    let moved_back = Rc::new(Cell::new(0u32));
    // For a window that is let out of fullscreen: how often it left the
    // monitor, and where it was last seen.
    let left = Rc::new(Cell::new(0u32));
    let last_on = Rc::new(RefCell::new(String::new()));
    // When the window had settled on the monitor, and when it first left it
    // after that (seconds from the start).
    let armed_at = Rc::new(Cell::new(None::<f64>));
    // Every change of the monitor the window is on after that, with the time.
    let trail = Rc::new(RefCell::new(Vec::<(f64, String)>::new()));
    let first_left_at = Rc::new(Cell::new(None::<f64>));
    {
        let main_loop = main_loop.clone();
        let (window, moved_back, connector) =
            (window.clone(), moved_back.clone(), connector.clone());
        let (left, last_on, how) = (left.clone(), last_on.clone(), how.clone());
        let (armed_at, first_left_at) = (armed_at.clone(), first_left_at.clone());
        let trail = trail.clone();
        let released = Cell::new(false);
        let was_on = Cell::new(true);
        let settled = Cell::new(0u32);
        gtk4::glib::timeout_add_local(Duration::from_millis(200), move || {
            if started.elapsed() >= Duration::from_secs(secs) {
                main_loop.quit();
                return gtk4::glib::ControlFlow::Break;
            }
            if how != "fullscreen"
                && !released.get()
                && started.elapsed() >= Duration::from_millis(1500)
            {
                released.set(true);
                window.unfullscreen();
                if how == "maximized" {
                    window.maximize();
                }
            }
            if let (Some(want), Some(display)) =
                (connector.as_deref(), gtk4::gdk::Display::default())
            {
                let monitors = display.monitors();
                let target = (0..monitors.n_items())
                    .filter_map(|i| monitors.item(i)?.downcast::<gtk4::gdk::Monitor>().ok())
                    .find(|m| m.connector().is_some_and(|c| c.starts_with(want)));
                let on = window
                    .surface()
                    .and_then(|surface| display.monitor_at_surface(&surface))
                    .and_then(|m| m.connector());
                if let Some(target) = target {
                    let here = on.as_deref() == target.connector().as_deref();
                    if how == "fullscreen" {
                        if !here {
                            window.fullscreen_on_monitor(&target);
                            moved_back.set(moved_back.get() + 1);
                        }
                    } else if released.get() {
                        // A missing monitor (the surface is between monitors)
                        // is no sample; the last known place is kept.
                        if let Some(name) = on.as_deref() {
                            *last_on.borrow_mut() = name.to_string();
                            if armed_at.get().is_none() {
                                // Settled: eight samples in a row on the monitor.
                                settled.set(if here { settled.get() + 1 } else { 0 });
                                if settled.get() >= 8 {
                                    armed_at.set(Some(started.elapsed().as_secs_f64()));
                                }
                            } else {
                                let mut t = trail.borrow_mut();
                                if t.last().is_none_or(|(_, n)| n != name) {
                                    t.push((started.elapsed().as_secs_f64(), name.to_string()));
                                }
                                drop(t);
                                if was_on.get() && !here {
                                    left.set(left.get() + 1);
                                    if first_left_at.get().is_none() {
                                        first_left_at.set(Some(started.elapsed().as_secs_f64()));
                                    }
                                }
                                was_on.set(here);
                            }
                        }
                    }
                }
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
    if connector.is_some() && how == "fullscreen" {
        println!("moved back onto the monitor {} time(s)", moved_back.get());
    } else if connector.is_some() {
        match armed_at.get() {
            None => println!(
                "the {how} window never settled on the monitor after being let out of fullscreen; last seen on {:?}",
                last_on.borrow()
            ),
            Some(t0) => println!(
                "the {how} window settled on the monitor at {t0:.1} s; after that it left it {} time(s){}; last seen on {:?}",
                left.get(),
                first_left_at
                    .get()
                    .map_or(String::new(), |t| format!(", the first time at {t:.1} s")),
                last_on.borrow()
            ),
        }
        let trail = trail.borrow();
        if !trail.is_empty() {
            let steps: Vec<String> = trail
                .iter()
                .map(|(t, n)| format!("{n} at {t:.1} s"))
                .collect();
            println!("monitor changes after settling: {}", steps.join(", then "));
        }
    }
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
