//! Turns PipeWire cursor metadata into cursor events.
//!
//! Mutter puts the same cursor id (1) on every cursor update, whether it
//! moves the pointer, changes its image or hides it, so the id says nothing
//! about what changed. A bitmap in the metadata is a new image; an empty
//! bitmap hides the cursor; no bitmap leaves the image as it was.

use hypr_capture::CaptureEvent;

/// What the metadata said about the cursor image.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Sprite {
    Unchanged,
    Hidden,
    Shape {
        width: u32,
        height: u32,
        hot_x: i32,
        hot_y: i32,
        /// Packed ARGB8888 (BGRA bytes).
        argb: Vec<u8>,
    },
}

#[derive(Debug)]
pub(crate) struct CursorTracker {
    visible: bool,
    /// The last position and visibility the consumer was told.
    reported: Option<((i32, i32), bool)>,
}

impl CursorTracker {
    pub(crate) fn new() -> Self {
        Self {
            visible: true,
            reported: None,
        }
    }

    /// One metadata update: events for whatever changed.
    pub(crate) fn update(&mut self, position: (i32, i32), sprite: Sprite) -> Vec<CaptureEvent> {
        let mut events = Vec::new();
        match sprite {
            Sprite::Unchanged => {}
            Sprite::Hidden => self.visible = false,
            Sprite::Shape {
                width,
                height,
                hot_x,
                hot_y,
                argb,
            } => {
                self.visible = true;
                events.push(CaptureEvent::CursorShape {
                    width,
                    height,
                    hot_x,
                    hot_y,
                    argb,
                });
            }
        }
        if self.reported != Some((position, self.visible)) {
            self.reported = Some((position, self.visible));
            events.push(CaptureEvent::CursorPos {
                x: position.0,
                y: position.1,
                visible: self.visible,
            });
        }
        events
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(w: u32) -> Sprite {
        Sprite::Shape {
            width: w,
            height: w,
            hot_x: 0,
            hot_y: 0,
            argb: vec![0; (w * w * 4) as usize],
        }
    }

    fn kinds(events: &[CaptureEvent]) -> Vec<String> {
        events
            .iter()
            .map(|e| match e {
                CaptureEvent::CursorShape { width, .. } => format!("shape{width}"),
                CaptureEvent::CursorPos { x, y, visible } => format!("pos{x},{y},{visible}"),
                _ => "other".into(),
            })
            .collect()
    }

    #[test]
    fn a_position_only_first_update_still_lets_a_later_shape_through() {
        let mut c = CursorTracker::new();
        assert_eq!(kinds(&c.update((1, 2), Sprite::Unchanged)), ["pos1,2,true"]);
        assert_eq!(
            kinds(&c.update((1, 2), shape(24))),
            ["shape24"],
            "same position, new image"
        );
    }

    #[test]
    fn every_bitmap_is_a_new_shape_even_with_the_same_cursor_id() {
        let mut c = CursorTracker::new();
        c.update((0, 0), shape(24));
        assert_eq!(kinds(&c.update((0, 0), shape(32))), ["shape32"]);
    }

    #[test]
    fn an_empty_bitmap_hides_and_a_shape_at_the_same_spot_shows_again() {
        let mut c = CursorTracker::new();
        c.update((5, 5), shape(24));
        assert_eq!(kinds(&c.update((5, 5), Sprite::Hidden)), ["pos5,5,false"]);
        assert!(
            c.update((5, 5), Sprite::Unchanged).is_empty(),
            "nothing changed"
        );
        assert_eq!(
            kinds(&c.update((5, 5), shape(24))),
            ["shape24", "pos5,5,true"]
        );
    }

    #[test]
    fn moving_reports_the_new_position_once() {
        let mut c = CursorTracker::new();
        c.update((0, 0), shape(24));
        assert_eq!(kinds(&c.update((3, 4), Sprite::Unchanged)), ["pos3,4,true"]);
        assert!(c.update((3, 4), Sprite::Unchanged).is_empty());
    }
}
