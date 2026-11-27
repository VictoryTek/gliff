//! The remote's cursor image, turned into something the window can show.

/// The largest cursor side accepted, as in the GTK viewer.
pub const MAX_SIDE: u32 = 1024;

/// A remote cursor image: `bgra` holds `width` x `height` straight-alpha BGRA
/// pixels, or is empty when the server sent one this viewer cannot show.
pub struct CursorShape {
    pub width: u32,
    pub height: u32,
    pub hot_x: i32,
    pub hot_y: i32,
    pub bgra: Vec<u8>,
}

/// Whether a shape's payload is exactly its tightly packed pixels, within the
/// size the viewer accepts.
pub fn payload_fits(width: u32, height: u32, len: u32) -> bool {
    (1..=MAX_SIDE).contains(&width)
        && (1..=MAX_SIDE).contains(&height)
        && u64::from(len) == u64::from(width) * u64::from(height) * 4
}

/// An RGBA image and hotspot for the window, or None when the shape is empty,
/// malformed, invisible or one flat colour (a server's way of saying "no
/// cursor"), so the caller falls back to the default or hides it.
pub struct Rgba {
    pub pixels: Vec<u8>,
    pub width: u16,
    pub height: u16,
    pub hot_x: u16,
    pub hot_y: u16,
}

pub fn to_rgba(shape: &CursorShape) -> Option<Rgba> {
    if !payload_fits(shape.width, shape.height, shape.bgra.len().try_into().ok()?) {
        return None;
    }
    // A hotspot outside the image is refused.
    let hot_x = u16::try_from(shape.hot_x)
        .ok()
        .filter(|&x| u32::from(x) < shape.width)?;
    let hot_y = u16::try_from(shape.hot_y)
        .ok()
        .filter(|&y| u32::from(y) < shape.height)?;
    let mut pixels = shape.bgra.clone();
    let first = [pixels[0], pixels[1], pixels[2], pixels[3]];
    let any_alpha = pixels.chunks_exact(4).any(|p| p[3] != 0);
    let uniform = pixels.chunks_exact(4).all(|p| p == first);
    if !any_alpha || uniform {
        return None;
    }
    for p in pixels.chunks_exact_mut(4) {
        p.swap(0, 2);
    }
    Some(Rgba {
        pixels,
        width: shape.width as u16,
        height: shape.height as u16,
        hot_x,
        hot_y,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shape(w: u32, h: u32, hot: (i32, i32), bgra: Vec<u8>) -> CursorShape {
        CursorShape {
            width: w,
            height: h,
            hot_x: hot.0,
            hot_y: hot.1,
            bgra,
        }
    }

    #[test]
    fn a_payload_must_be_exactly_the_pixels_within_the_size_limit() {
        assert!(payload_fits(24, 24, 24 * 24 * 4));
        assert!(!payload_fits(24, 24, 24 * 24 * 4 + 4));
        assert!(!payload_fits(0, 24, 0));
        assert!(!payload_fits(MAX_SIDE + 1, 1, (MAX_SIDE + 1) * 4));
        assert!(!payload_fits(u32::MAX, u32::MAX, 4));
    }

    #[test]
    fn colours_are_swapped_to_rgba_and_the_hotspot_kept() {
        // Two pixels: opaque blue (B=255) and transparent.
        let s = shape(2, 1, (1, 0), vec![255, 0, 0, 255, 0, 0, 0, 0]);
        let r = to_rgba(&s).unwrap();
        assert_eq!(r.pixels, vec![0, 0, 255, 255, 0, 0, 0, 0]);
        assert_eq!((r.width, r.height, r.hot_x, r.hot_y), (2, 1, 1, 0));
    }

    #[test]
    fn an_unshowable_shape_gives_nothing() {
        let px = vec![255, 0, 0, 255, 0, 0, 0, 0];
        assert!(
            to_rgba(&shape(2, 1, (2, 0), px.clone())).is_none(),
            "hotspot outside"
        );
        assert!(
            to_rgba(&shape(2, 1, (-1, 0), px.clone())).is_none(),
            "negative hotspot"
        );
        assert!(
            to_rgba(&shape(2, 1, (0, 0), px[..4].to_vec())).is_none(),
            "short payload"
        );
        assert!(
            to_rgba(&shape(2, 1, (0, 0), vec![9, 9, 9, 0, 1, 1, 1, 0])).is_none(),
            "invisible"
        );
        assert!(
            to_rgba(&shape(2, 1, (0, 0), vec![5, 5, 5, 255, 5, 5, 5, 255])).is_none(),
            "flat"
        );
        assert!(to_rgba(&shape(2, 1, (0, 0), Vec::new())).is_none(), "empty");
    }
}
