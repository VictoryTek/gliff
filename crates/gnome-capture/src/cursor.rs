//! Turns PipeWire cursor metadata into cursor events.
//!
//! Mutter puts the same cursor id (1) on every cursor update, whether it
//! moves the pointer, changes its image or hides it, so the id says nothing
//! about what changed. A bitmap in the metadata is a new image; an empty
//! bitmap hides the cursor; no bitmap leaves the image as it was.
//!
//! Mutter sends the image once, when the cursor first shows up on the stream,
//! and again only when it changes. A consumer that misses that one buffer (or
//! that connects while the cursor is elsewhere, as on a new virtual monitor)
//! would be told where the cursor is but never what it looks like, so until a
//! real image arrives the tracker supplies a plain arrow.

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

/// A plain arrow, `X` black, `o` white, `.` clear. The hotspot is the tip.
const ARROW: [&str; 19] = [
    "X...........",
    "XX..........",
    "XoX.........",
    "XooX........",
    "XoooX.......",
    "XooooX......",
    "XoooooX.....",
    "XooooooX....",
    "XoooooooX...",
    "XooooooooX..",
    "XoooooXXXXXX",
    "XooXooX.....",
    "XoX.XooX....",
    "XX..XooX....",
    "X....XooX...",
    ".....XooX...",
    "......XooX..",
    "......XooX..",
    ".......XX...",
];

fn arrow() -> CaptureEvent {
    let width = ARROW[0].len() as u32;
    let mut argb = Vec::with_capacity(ARROW.len() * ARROW[0].len() * 4);
    for row in ARROW {
        for c in row.bytes() {
            // BGRA bytes, as Mutter's bitmaps are.
            argb.extend_from_slice(match c {
                b'X' => &[0, 0, 0, 255],
                b'o' => &[255, 255, 255, 255],
                _ => &[0, 0, 0, 0],
            });
        }
    }
    CaptureEvent::CursorShape {
        width,
        height: ARROW.len() as u32,
        hot_x: 0,
        hot_y: 0,
        argb,
    }
}

#[derive(Debug)]
pub(crate) struct CursorTracker {
    /// Whether the consumer has been given any image.
    shape_known: bool,
    visible: bool,
    /// The last position and visibility the consumer was told.
    reported: Option<((i32, i32), bool)>,
}

impl CursorTracker {
    pub(crate) fn new() -> Self {
        Self {
            shape_known: false,
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
                self.shape_known = true;
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
            if self.visible && !self.shape_known {
                self.shape_known = true;
                events.push(arrow());
            }
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
        assert_eq!(
            kinds(&c.update((1, 2), Sprite::Unchanged)),
            ["shape12", "pos1,2,true"],
            "no image yet: the plain arrow stands in"
        );
        assert_eq!(
            kinds(&c.update((1, 2), shape(24))),
            ["shape24"],
            "same position, new image"
        );
    }

    #[test]
    fn the_arrow_is_only_a_stand_in() {
        let mut c = CursorTracker::new();
        c.update((0, 0), shape(24));
        assert_eq!(
            kinds(&c.update((3, 4), Sprite::Unchanged)),
            ["pos3,4,true"],
            "a real image was already sent"
        );
        let mut hidden = CursorTracker::new();
        assert_eq!(
            kinds(&hidden.update((0, 0), Sprite::Hidden)),
            ["pos0,0,false"],
            "nothing to draw, so no stand-in"
        );
    }

    #[test]
    fn the_arrow_is_a_well_formed_bitmap() {
        let CaptureEvent::CursorShape {
            width,
            height,
            argb,
            ..
        } = arrow()
        else {
            panic!("not a shape")
        };
        assert!(ARROW.iter().all(|r| r.len() == width as usize));
        assert_eq!(argb.len(), (width * height * 4) as usize);
        assert_eq!(&argb[..4], &[0, 0, 0, 255], "the tip is opaque black");
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
