//! Voice tile backgrounds: each person's tile takes the main colour of their
//! avatar. Worked out once per avatar, off the UI thread.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cord_core::Core;
use gpui_kit::App;

thread_local! {
    /// By avatar URL. `None`: still loading, or it couldn't be read.
    static COLORS: RefCell<HashMap<String, Option<u32>>> = RefCell::default();
}

/// The picture's main colour, as 0xRRGGBB. Close shades count as one; see-through
/// and pure white pixels don't count; and a pixel with colour in it counts for
/// more than a grey one, so a bright hat on a black background gives the hat.
pub fn dominant(pixels: impl Iterator<Item = [u8; 4]>) -> Option<u32> {
    // Per 4-bit-a-channel shade: how many pixels, and the sum of their channels.
    let mut shades: HashMap<u16, (u32, [u32; 3], f32)> = HashMap::new();
    for [r, g, b, a] in pixels {
        if a < 128 || (r > 250 && g > 250 && b > 250) {
            continue;
        }
        let key = (u16::from(r >> 4) << 8) | (u16::from(g >> 4) << 4) | u16::from(b >> 4);
        let shade = shades.entry(key).or_default();
        let (most, least) = (r.max(g).max(b), r.min(g).min(b));
        let saturation = if most == 0 { 0. } else { f32::from(most - least) / f32::from(most) };
        shade.2 += 1. + 4. * saturation;
        shade.0 += 1;
        shade.1[0] += u32::from(r);
        shade.1[1] += u32::from(g);
        shade.1[2] += u32::from(b);
    }
    // Ties go to the lower key, so the same picture always gives the same colour.
    let (_, (count, sum, _)) = shades.into_iter().max_by(|(a_key, a), (b_key, b)| a.2.total_cmp(&b.2).then(b_key.cmp(a_key)))?;
    Some(((sum[0] / count) << 16) | ((sum[1] / count) << 8) | (sum[2] / count))
}

fn from_file(bytes: &[u8]) -> Option<u32> {
    let picture = image::load_from_memory(bytes).ok()?.thumbnail(32, 32).to_rgba8();
    dominant(picture.pixels().map(|p| p.0))
}

/// The colour for the avatar at `url`, once known; asks for it the first time.
pub fn color(url: &str, core: &Arc<Core>, cx: &App) -> Option<u32> {
    if let Some(known) = COLORS.with_borrow(|colors| colors.get(url).copied()) {
        return known;
    }
    COLORS.with_borrow_mut(|colors| colors.insert(url.to_string(), None));
    let (url, core) = (url.to_string(), core.clone());
    cx.spawn(async move |cx| {
        let Ok(bytes) = core.cdn_bytes(url.clone()).await else { return };
        let Some(color) = cx.background_executor().spawn(async move { from_file(&bytes) }).await else { return };
        COLORS.with_borrow_mut(|colors| colors.insert(url, Some(color)));
        let _ = cx.update(|cx| crate::ui::redraw_chat(cx));
    })
    .detach();
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_biggest_colour_wins() {
        let pixels = [[250, 120, 40, 255]; 5].into_iter().chain([[10, 10, 10, 255]; 3]);
        assert_eq!(dominant(pixels), Some(0xfa7828));
        // Colour beats a bigger black background, and white never wins.
        let hat = [[230, 100, 20, 255]; 3].into_iter().chain([[5, 5, 5, 255]; 8]);
        assert_eq!(dominant(hat), Some(0xe66414));
        assert_eq!(dominant([[255, 255, 255, 255]; 4].into_iter()), None);
    }

    #[test]
    fn close_shades_count_together_and_average() {
        let pixels = [[16, 32, 48, 255], [18, 34, 50, 255], [200, 0, 0, 255]].into_iter();
        assert_eq!(dominant(pixels), Some(0x112131));
    }

    #[test]
    fn see_through_pixels_are_ignored() {
        let pixels = [[0, 0, 255, 0]; 9].into_iter().chain([[0, 255, 0, 255]]);
        assert_eq!(dominant(pixels), Some(0x00ff00));
        assert_eq!(dominant([[1, 2, 3, 0]].into_iter()), None);
    }
}
