//! Capture events for a sink that does not exist yet.
//!
//! A virtual output starts capturing when it is created (Mutter makes the
//! monitor only then), but the server hands over its sink later, in
//! `start_capture`. Until then the events are kept, only the newest of each
//! kind that matters, and replayed when the sink arrives.

use gliff_backend::{CaptureEvent, CaptureSink};

#[derive(Default)]
pub struct Pending {
    ready: Option<CaptureEvent>,
    shape: Option<CaptureEvent>,
    pos: Option<CaptureEvent>,
    /// `Error` or `Stopped`: the capture is over.
    end: Option<CaptureEvent>,
}

pub enum Forward {
    Waiting(Box<Pending>),
    Attached(CaptureSink),
}

impl Default for Forward {
    fn default() -> Self {
        Forward::Waiting(Box::default())
    }
}

impl Forward {
    pub fn event(&mut self, event: CaptureEvent) {
        match self {
            Forward::Attached(sink) => sink(event),
            Forward::Waiting(p) => match event {
                CaptureEvent::Ready { .. } => p.ready = Some(event),
                CaptureEvent::CursorShape { .. } => p.shape = Some(event),
                CaptureEvent::CursorPos { .. } => p.pos = Some(event),
                CaptureEvent::Error(_) | CaptureEvent::Stopped => p.end = Some(event),
                // Frames only come when asked for, and nothing has asked.
                CaptureEvent::Frame(_) => {}
            },
        }
    }

    /// Send `sink` what it missed, then everything from now on.
    pub fn attach(&mut self, mut sink: CaptureSink) {
        if let Forward::Waiting(p) = std::mem::take(self) {
            for event in [p.ready, p.shape, p.pos, p.end].into_iter().flatten() {
                sink(event);
            }
        }
        *self = Forward::Attached(sink);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn shape(width: u32) -> CaptureEvent {
        CaptureEvent::CursorShape {
            width,
            height: 24,
            hot_x: 0,
            hot_y: 0,
            argb: Vec::new(),
        }
    }

    fn pos(x: i32) -> CaptureEvent {
        CaptureEvent::CursorPos {
            x,
            y: 0,
            visible: true,
        }
    }

    fn label(e: &CaptureEvent) -> String {
        match e {
            CaptureEvent::CursorShape { width, .. } => format!("shape {width}"),
            CaptureEvent::CursorPos { x, .. } => format!("pos {x}"),
            CaptureEvent::Error(m) => format!("error {m}"),
            CaptureEvent::Stopped => "stopped".into(),
            _ => "other".into(),
        }
    }

    fn collect() -> (Arc<Mutex<Vec<String>>>, CaptureSink) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        (seen, Box::new(move |e| log.lock().unwrap().push(label(&e))))
    }

    #[test]
    fn a_late_sink_gets_the_newest_of_each_kind_then_live_events() {
        let mut f = Forward::default();
        f.event(shape(24));
        f.event(pos(1));
        f.event(shape(32));
        f.event(pos(2));
        f.event(pos(3));
        let (seen, sink) = collect();
        f.attach(sink);
        f.event(pos(4));
        assert_eq!(
            *seen.lock().unwrap(),
            ["shape 32", "pos 3", "pos 4"].map(String::from)
        );
    }

    #[test]
    fn an_ended_capture_is_still_reported_after_attaching() {
        let mut f = Forward::default();
        f.event(CaptureEvent::Error("gone".into()));
        let (seen, sink) = collect();
        f.attach(sink);
        assert_eq!(*seen.lock().unwrap(), ["error gone".to_string()]);
    }
}
