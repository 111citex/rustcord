//! Animated nameplates. Discord's are WebM with an alpha channel, which the
//! video player decodes without its transparency (a smeared, opaque picture),
//! so ffmpeg turns each into an animated WebP once, kept on disk, and `img`
//! plays that with its transparency intact.

use std::collections::VecDeque;
use std::hash::{Hash, Hasher};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;

use gpui_kit::{App, Global, RenderImage};
use image::codecs::webp::WebPDecoder;
use image::{AnimationDecoder, Rgba};

/// Animations kept decoded; each is tens of MB.
const POOL: usize = 2;
/// Not hovered for this long, one goes. Its file stays on disk.
const REST_AFTER: std::time::Duration = std::time::Duration::from_secs(15);

#[derive(Default)]
struct Nameplates(VecDeque<(String, Option<Arc<RenderImage>>)>);

impl Global for Nameplates {}

fn path_for(url: &str) -> PathBuf {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    url.hash(&mut hasher);
    cord_core::paths::cache_dir().join("nameplates").join(format!("{:016x}.webp", hasher.finish()))
}

/// Converts with the libvpx decoders, the only ones that read WebM alpha.
fn convert(url: &str, out: &PathBuf) -> bool {
    let Some(dir) = out.parent() else { return false };
    if std::fs::create_dir_all(dir).is_err() {
        return false;
    }
    let tmp = out.with_extension("tmp.webp");
    let done = ["libvpx-vp9", "libvpx"].iter().any(|decoder| {
        Command::new("ffmpeg")
            .args(["-y", "-loglevel", "error", "-c:v", decoder, "-i", url])
            .args(["-vf", "fps=24,scale=528:-2:flags=bicubic", "-c:v", "libwebp_anim", "-lossless", "0", "-quality", "75", "-loop", "0"])
            .arg(&tmp)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    });
    done && std::fs::rename(&tmp, out).is_ok()
}

/// The file's frames, ready to draw. Decoding here, ahead of showing it, means
/// the still can stay up until the animation can replace it on the same frame.
fn decode(path: &PathBuf) -> Option<RenderImage> {
    let bytes = std::fs::read(path).ok()?;
    let mut decoder = WebPDecoder::new(Cursor::new(bytes)).ok()?;
    decoder.set_background_color(Rgba([0, 0, 0, 0])).ok()?;
    let mut frames = Vec::new();
    for frame in decoder.into_frames() {
        let mut frame = frame.ok()?;
        // GPUI's images are BGRA.
        for pixel in frame.buffer_mut().chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        frames.push(frame);
    }
    (!frames.is_empty()).then(|| RenderImage::new(frames))
}

/// Starts preparing the animated version of `url`, if nobody has yet.
pub fn warm(url: &str, cx: &mut App) {
    if !cx.has_global::<Nameplates>() {
        cx.set_global(Nameplates::default());
    }
    if cx.global::<Nameplates>().0.iter().any(|(known, _)| known == url) {
        return;
    }
    let list = &mut cx.global_mut::<Nameplates>().0;
    list.push_back((url.to_string(), None));
    crate::anim::note_used(url);
    let dropped = if list.len() > POOL { list.pop_front() } else { None };
    if let Some((_, Some(image))) = dropped {
        cx.drop_image(image, None);
    }
    let url = url.to_string();
    let job = cx.background_executor().spawn({
        let url = url.clone();
        async move {
            let out = path_for(&url);
            if !out.exists() && !convert(&url, &out) {
                return None;
            }
            decode(&out)
        }
    });
    cx.spawn(async move |cx| {
        let image = job.await;
        if image.is_none() {
            tracing::warn!(%url, "couldn't prepare a nameplate");
        }
        let _ = cx.update(|cx| {
            if let Some(entry) = cx.global_mut::<Nameplates>().0.iter_mut().find(|(known, _)| *known == url) {
                entry.1 = image.map(Arc::new);
            }
            cx.refresh_windows();
        });
    })
    .detach();
}

/// Lets go of animations that aren't showing.
pub fn rest(cx: &mut App) {
    let Some(list) = cx.try_global::<Nameplates>() else { return };
    let gone: Vec<String> = list.0.iter().filter(|(url, image)| image.is_some() && crate::anim::unused_for(url, REST_AFTER)).map(|(url, _)| url.clone()).collect();
    for url in gone {
        let list = &mut cx.global_mut::<Nameplates>().0;
        let Some(at) = list.iter().position(|(known, _)| *known == url) else { continue };
        if let Some((_, Some(image))) = list.remove(at) {
            cx.drop_image(image, None);
        }
    }
}

/// The animation for `url`, once it's ready to draw.
pub fn ready(url: &str, cx: &App) -> Option<Arc<RenderImage>> {
    crate::anim::note_used(url);
    cx.try_global::<Nameplates>()?.0.iter().find(|(known, _)| known == url)?.1.clone()
}

/// A nameplate's colour, from the name Discord gives it: (dark theme, light theme).
fn palette_rgb(name: &str) -> Option<(u32, u32)> {
    Some(match name {
        "crimson" => (0x900007, 0xE7040F),
        "berry" => (0x893A99, 0xB11FCF),
        "sky" => (0x0080B7, 0x56CCFF),
        "teal" => (0x086460, 0x7DEED7),
        "forest" => (0x2D5401, 0x6AA624),
        "bubble_gum" => (0xDC3E97, 0xF957B3),
        "violet" => (0x730BC8, 0x972FED),
        "cobalt" => (0x0131C2, 0x4278FF),
        "clover" => (0x047B20, 0x63CD5A),
        "lemon" => (0xF6CD12, 0xFED400),
        "white" => (0xFFFFFF, 0xFFFFFF),
        "black" => (0x000000, 0x000000),
        _ => return None,
    })
}

/// Goes over a nameplate's picture, as Discord's does: the picture only shows in full
/// at the right (Discord masks it; this covers it with the row's own colour), and the
/// whole row takes on the nameplate's colour, strongest at the right.
pub fn backdrop(palette: Option<&str>, row_bg: gpui_kit::Hsla, dark: bool, radius: f32) -> Vec<gpui_kit::AnyElement> {
    use gpui_kit::{IntoElement as _, Styled as _, div, linear_color_stop, linear_gradient, relative};
    let band = |left: f32, width: f32, from: gpui_kit::Hsla, to: gpui_kit::Hsla| {
        div().absolute().top_0().bottom_0().left(relative(left)).w(relative(width)).bg(linear_gradient(90., linear_color_stop(from, 0.), linear_color_stop(to, 1.)))
    };
    const COVERED: f32 = 0.7;
    let mut layers = vec![
        band(0., 0.686, row_bg.opacity(COVERED), row_bg.opacity(COVERED)).rounded_l(crate::ui::u(radius)).into_any_element(),
        band(0.686, 0.202, row_bg.opacity(COVERED), row_bg.opacity(0.)).into_any_element(),
    ];
    let color = palette.and_then(palette_rgb).map(|(on_dark, on_light)| gpui_kit::Hsla::from(gpui_kit::rgb(if dark { on_dark } else { on_light })));
    if let Some(color) = color {
        layers.push(band(0., 0.2, color.opacity(0.), color.opacity(0.08)).into_any_element());
        layers.push(band(0.2, 0.3, color.opacity(0.08), color.opacity(0.08)).into_any_element());
        layers.push(band(0.5, 0.5, color.opacity(0.08), color.opacity(0.2)).rounded_r(crate::ui::u(radius)).into_any_element());
    }
    layers
}
