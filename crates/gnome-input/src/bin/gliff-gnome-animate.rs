//! A fullscreen window that changes every frame, for testing a remote
//! session under load: every capture frame of the monitor it covers differs
//! from the one before, as in video, a game or fast scrolling.
//!
//!   gliff-gnome-animate [SECS [RECTS]]
//!                                 run for SECS seconds (default 60), drawing
//!                                 RECTS coloured rectangles a frame (default
//!                                 300), then print how many frames it drew
//!
//! It draws the rectangles and a sweeping bar per frame, so the picture is
//! busy enough to need real bit rate from the encoder. More rectangles cost
//! the test window itself more time: with few, it keeps up with the display.

use std::cell::Cell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gtk4::prelude::*;

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
    if gtk4::init().is_err() {
        println!("[FAIL] gtk: cannot open a display");
        std::process::exit(1);
    }
    let frame = Rc::new(Cell::new(0u32));
    let area = gtk4::DrawingArea::new();
    {
        let frame = frame.clone();
        area.set_draw_func(move |_, cr, w, h| {
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
        let frame = frame.clone();
        move |area, _| {
            frame.set(frame.get().wrapping_add(1));
            area.queue_draw();
            gtk4::glib::ControlFlow::Continue
        }
    });
    let window = gtk4::Window::new();
    window.set_title(Some("gliff animate"));
    window.set_child(Some(&area));
    window.fullscreen();
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
    println!(
        "drew {n} frames in {:.1} s ({:.1} per second)",
        started.elapsed().as_secs_f64(),
        f64::from(n) / started.elapsed().as_secs_f64()
    );
}
