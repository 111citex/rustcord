//! Smooth gradients. GPUI's are 8 bits a channel with nothing to hide the steps,
//! which on a dark, slow gradient shows as bands. Browsers dither; so do these: a
//! picture of the gradient with a pixel's worth of noise, cached by size and colours.

use std::cell::RefCell;
use std::sync::Arc;

use gpui_kit::{Corners, IntoElement, ParentElement, RenderImage, Styled, canvas, div};
use image::{Frame, RgbaImage};

const KEPT: usize = 4;

type Key = ([u8; 3], [u8; 3], u32, u32);

thread_local! {
    static PICTURES: RefCell<Vec<(Key, Arc<RenderImage>)>> = const { RefCell::new(Vec::new()) };
}

/// Noise in [-0.5, 0.5) that depends only on the pixel.
fn noise(x: u32, y: u32) -> f32 {
    // Interleaved gradient noise: looks like no pattern at all, and needs no state.
    let v = 52.982_918 * (0.067_110_56 * x as f32 + 0.005_837_15 * y as f32).fract();
    v.fract() - 0.5
}

/// BGRA pixels of a top-to-bottom gradient.
fn pixels(top: [u8; 3], bottom: [u8; 3], width: u32, height: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let t = y as f32 / height.saturating_sub(1).max(1) as f32;
        for x in 0..width {
            let n = noise(x, y);
            let channel = |c: usize| {
                let value = f32::from(top[c]) + (f32::from(bottom[c]) - f32::from(top[c])) * t;
                (value + n).round().clamp(0., 255.) as u8
            };
            out.extend_from_slice(&[channel(2), channel(1), channel(0), 255]);
        }
    }
    out
}

fn bytes(color: gpui_kit::Hsla) -> [u8; 3] {
    let c = color.to_rgb();
    [(c.r * 255.).round() as u8, (c.g * 255.).round() as u8, (c.b * 255.).round() as u8]
}

/// Fills its parent (which must be `relative`) with the gradient, under what follows.
pub fn fill(top: gpui_kit::Hsla, bottom: gpui_kit::Hsla, radius: f32) -> impl IntoElement {
    let (top, bottom) = (bytes(top), bytes(bottom));
    div().absolute().top_0().left_0().size_full().child(
        canvas(|_, _, _| {}, move |bounds, _, window, _| {
            let scale = window.scale_factor();
            let width = (f32::from(bounds.size.width) * scale).ceil() as u32;
            let height = (f32::from(bounds.size.height) * scale).ceil() as u32;
            if width == 0 || height == 0 {
                return;
            }
            let key = (top, bottom, width, height);
            let known = PICTURES.with_borrow(|p| p.iter().find(|(k, _)| *k == key).map(|(_, i)| i.clone()));
            let image = known.or_else(|| {
                let buffer = RgbaImage::from_raw(width, height, pixels(top, bottom, width, height))?;
                let image = Arc::new(RenderImage::new([Frame::new(buffer)]));
                let dropped = PICTURES.with_borrow_mut(|p| {
                    p.push((key, image.clone()));
                    (p.len() > KEPT).then(|| p.remove(0).1)
                });
                if let Some(old) = dropped {
                    let _ = window.drop_image(old);
                }
                Some(image)
            });
            let Some(image) = image else { return };
            let corners = Corners::all(window.rem_size() * (radius / 16.));
            let _ = window.paint_image(bounds, bounds, corners, image, 0, false);
        })
        .size_full(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stays_within_a_step_of_the_true_colour() {
        let (w, h) = (16, 64);
        let px = pixels([10, 20, 30], [50, 60, 70], w, h);
        assert_eq!(px.len(), (w * h * 4) as usize);
        for y in 0..h {
            let expected = 10. + 40. * y as f32 / (h - 1) as f32;
            for x in 0..w {
                let r = f32::from(px[((y * w + x) * 4 + 2) as usize]);
                assert!((r - expected).abs() <= 1.01, "{r} vs {expected}");
            }
        }
        assert!(px.chunks(4).all(|p| p[3] == 255));
    }

    #[test]
    fn dithers_instead_of_banding() {
        // A row between two levels mixes both, rather than being all one.
        let px = pixels([10, 10, 10], [11, 11, 11], 64, 3);
        let row: Vec<u8> = px[(64 * 4)..(128 * 4)].chunks(4).map(|p| p[0]).collect();
        assert!(row.contains(&10) && row.contains(&11));
    }
}
