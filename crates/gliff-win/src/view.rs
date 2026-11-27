//! Where the remote picture sits in the window, and the two maps that
//! follow from it: window pixels to the remote's pointer space, and the
//! picture's pixels onto the window's.

use gliff_sw::BgraFrame;

/// The rectangle of the window the picture fills, in window pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Fit {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// The largest centred rectangle with the aspect of `view` that fits in a
/// window of `area` pixels. `None` when either has no area.
pub fn fit(view: (u32, u32), area: (u32, u32)) -> Option<Fit> {
    if view.0 == 0 || view.1 == 0 || area.0 == 0 || area.1 == 0 {
        return None;
    }
    let (vw, vh) = (f64::from(view.0), f64::from(view.1));
    let (aw, ah) = (f64::from(area.0), f64::from(area.1));
    let factor = (aw / vw).min(ah / vh);
    let (w, h) = (vw * factor, vh * factor);
    Some(Fit {
        x: (aw - w) / 2.0,
        y: (ah - h) / 2.0,
        w,
        h,
    })
}

/// Map a window point to the remote's logical pointer space: the picture is
/// the whole stream stretched into `fit`, and the server expects stream
/// pixels divided by the effective scale. A point outside the picture is
/// clamped to its edge.
pub fn to_remote(fit: &Fit, stream: (u32, u32), scale_milli: u32, x: f64, y: f64) -> (f64, f64) {
    let fx = ((x - fit.x) / fit.w).clamp(0.0, 1.0);
    let fy = ((y - fit.y) / fit.h).clamp(0.0, 1.0);
    let scale = (f64::from(scale_milli) / 1000.0).max(0.01);
    (
        fx * f64::from(stream.0) / scale,
        fy * f64::from(stream.1) / scale,
    )
}

/// Paint `frame` into `fit` of a `width` x `height` window buffer of `0RGB`
/// pixels, nearest neighbour, and black everywhere else.
pub fn draw(buf: &mut [u32], width: usize, height: usize, frame: Option<&BgraFrame>, fit: &Fit) {
    buf.fill(0);
    let Some(frame) = frame else {
        return;
    };
    let (fw, fh) = (frame.width as usize, frame.height as usize);
    if fw == 0 || fh == 0 || frame.pixels.len() < fw * fh * 4 || buf.len() < width * height {
        return;
    }
    let x0 = (fit.x.round() as usize).min(width);
    let y0 = (fit.y.round() as usize).min(height);
    let x1 = ((fit.x + fit.w).round() as usize).min(width);
    let y1 = ((fit.y + fit.h).round() as usize).min(height);
    let (dw, dh) = (x1.saturating_sub(x0), y1.saturating_sub(y0));
    if dw == 0 || dh == 0 {
        return;
    }
    let px = |sx: usize, sy: usize| -> u32 {
        let i = (sy * fw + sx) * 4;
        let b = &frame.pixels[i..i + 4];
        u32::from_le_bytes([b[0], b[1], b[2], 0])
    };
    if (dw, dh) == (fw, fh) {
        for dy in 0..dh {
            let row = &mut buf[(y0 + dy) * width + x0..][..dw];
            for (dx, out) in row.iter_mut().enumerate() {
                *out = px(dx, dy);
            }
        }
        return;
    }
    let xmap: Vec<usize> = (0..dw).map(|dx| dx * fw / dw).collect();
    for dy in 0..dh {
        let sy = dy * fh / dh;
        let row = &mut buf[(y0 + dy) * width + x0..][..dw];
        for (out, &sx) in row.iter_mut().zip(&xmap) {
            *out = px(sx, sy);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wide_window_leaves_bars_at_the_sides() {
        let f = fit((1920, 1080), (2000, 1080)).unwrap();
        assert_eq!((f.x, f.y, f.w, f.h), (40.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn a_tall_window_leaves_bars_above_and_below() {
        let f = fit((1920, 1080), (960, 700)).unwrap();
        assert_eq!((f.w, f.h), (960.0, 540.0));
        assert_eq!((f.x, f.y), (0.0, 80.0));
    }

    #[test]
    fn an_empty_size_has_no_fit() {
        assert!(fit((0, 1080), (100, 100)).is_none());
        assert!(fit((1920, 1080), (0, 100)).is_none());
    }

    #[test]
    fn the_corners_of_the_picture_are_the_corners_of_the_remote() {
        let f = fit((1920, 1080), (960, 540)).unwrap();
        assert_eq!(to_remote(&f, (1920, 1080), 1000, 0.0, 0.0), (0.0, 0.0));
        assert_eq!(
            to_remote(&f, (1920, 1080), 1000, 960.0, 540.0),
            (1920.0, 1080.0)
        );
        assert_eq!(
            to_remote(&f, (1920, 1080), 1000, 480.0, 270.0),
            (960.0, 540.0)
        );
    }

    #[test]
    fn the_remote_scale_divides_the_pointer_space() {
        let f = fit((3840, 2160), (1920, 1080)).unwrap();
        assert_eq!(
            to_remote(&f, (3840, 2160), 2000, 1920.0, 1080.0),
            (1920.0, 1080.0)
        );
    }

    #[test]
    fn a_point_in_a_bar_is_clamped_to_the_edge() {
        let f = fit((100, 100), (200, 100)).unwrap();
        assert_eq!(to_remote(&f, (100, 100), 1000, 0.0, 50.0), (0.0, 50.0));
        assert_eq!(to_remote(&f, (100, 100), 1000, 199.0, 50.0), (100.0, 50.0));
    }

    fn frame(w: u32, h: u32, f: impl Fn(u32, u32) -> [u8; 4]) -> BgraFrame {
        let mut pixels = Vec::new();
        for y in 0..h {
            for x in 0..w {
                pixels.extend_from_slice(&f(x, y));
            }
        }
        BgraFrame {
            width: w,
            height: h,
            pixels,
        }
    }

    #[test]
    fn an_exact_fit_copies_the_pixels() {
        let fr = frame(2, 2, |x, y| [x as u8, y as u8, 7, 255]);
        let f = fit((2, 2), (2, 2)).unwrap();
        let mut buf = vec![0u32; 4];
        draw(&mut buf, 2, 2, Some(&fr), &f);
        // BGRA bytes [b, g, r, a] become 0x00RRGGBB.
        assert_eq!(buf, [0x07_0000, 0x07_0001, 0x07_0100, 0x07_0101]);
    }

    #[test]
    fn a_smaller_picture_is_enlarged_and_barred() {
        // One red pixel (BGRA) into a 4x2 window: the 1x1 picture fills a 2x2
        // block centred horizontally, black on either side.
        let fr = frame(1, 1, |_, _| [0, 0, 255, 255]);
        let f = fit((1, 1), (4, 2)).unwrap();
        let mut buf = vec![1u32; 8];
        draw(&mut buf, 4, 2, Some(&fr), &f);
        let red = 0x00FF_0000;
        assert_eq!(buf, [0, red, red, 0, 0, red, red, 0]);
    }

    #[test]
    fn no_frame_is_a_black_window() {
        let f = fit((2, 2), (2, 2)).unwrap();
        let mut buf = vec![9u32; 4];
        draw(&mut buf, 2, 2, None, &f);
        assert_eq!(buf, [0; 4]);
    }
}
