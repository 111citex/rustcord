//! Lottie stickers (Discord's built-in ones): JSON animations GPUI can't draw,
//! so rlottie renders each to frames once, which `img` then plays. The chat
//! list warms them for the rows it's about to draw, like GIFs.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;

use cord_core::model::Message;
use cord_core::{Core, Id};
use gpui_kit::{App, Global, RenderImage};
use image::{Delay, Frame, RgbaImage};

/// Formats a sticker can be; this module handles this one.
pub const LOTTIE: u8 = 3;
/// Animated PNG: Discord's CDN only sends the animation with `passthrough`, and
/// GPUI's decoder would only read its first frame anyway.
pub const APNG: u8 = 2;

/// Formats drawn from frames made here, rather than by the image loader.
pub fn is_rendered(format: u8) -> bool {
    format == LOTTIE || format == APNG
}

pub fn render_apng(bytes: &[u8]) -> Option<RenderImage> {
    use image::AnimationDecoder;
    let decoder = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes)).ok()?;
    let mut frames = Vec::new();
    for frame in decoder.apng().ok()?.into_frames().take(600) {
        let frame = frame.ok()?;
        let delay = frame.delay();
        let (left, top) = (frame.left(), frame.top());
        let mut buffer = frame.into_buffer();
        // GPUI's images are BGRA.
        for px in buffer.pixels_mut() {
            px.0.swap(0, 2);
        }
        frames.push(Frame::from_parts(buffer, left, top, delay));
    }
    (!frames.is_empty()).then(|| RenderImage::new(frames))
}
const SIZE: usize = 240;
const MAX_FPS: f64 = 30.;
/// Rendered stickers kept; each is a few MB.
const POOL: usize = 8;

enum State {
    Loading,
    Ready(Arc<RenderImage>),
    Failed,
}

#[derive(Default)]
struct Stickers(VecDeque<(Id, State)>);

impl Global for Stickers {}

fn render(json: String, key: String) -> Option<RenderImage> {
    let mut animation = rlottie::Animation::from_data(json, key, "")?;
    let total = animation.totalframe();
    let fps = animation.framerate();
    if total == 0 || fps <= 0. {
        return None;
    }
    let step = (fps / MAX_FPS).ceil().max(1.) as usize;
    let delay = Delay::from_saturating_duration(Duration::from_secs_f64(step as f64 / fps));
    let mut surface = rlottie::Surface::new(rlottie::Size::new(SIZE, SIZE));
    let mut frames = Vec::with_capacity(total / step + 1);
    for n in (0..total).step_by(step) {
        animation.render(n, &mut surface);
        // rlottie's BGRA is premultiplied; GPUI's images aren't.
        let mut pixels = surface.data_as_bytes().to_vec();
        for px in pixels.chunks_exact_mut(4) {
            let a = u32::from(px[3]);
            if a > 0 && a < 255 {
                for c in &mut px[..3] {
                    *c = (u32::from(*c) * 255 / a).min(255) as u8;
                }
            }
        }
        let buffer = RgbaImage::from_raw(SIZE as u32, SIZE as u32, pixels)?;
        frames.push(Frame::from_parts(buffer, 0, 0, delay));
    }
    Some(RenderImage::new(frames))
}

/// Starts rendering the Lottie stickers on `message` that aren't known yet.
pub fn warm(message: &Message, core: &Arc<Core>, cx: &mut App) {
    for sticker in message.sticker_items.iter().filter(|s| is_rendered(s.format_type)) {
        let id = sticker.id;
        let format = sticker.format_type;
        if !cx.has_global::<Stickers>() {
            cx.set_global(Stickers::default());
        }
        if cx.global::<Stickers>().0.iter().any(|(known, _)| *known == id) {
            continue;
        }
        let list = &mut cx.global_mut::<Stickers>().0;
        list.push_back((id, State::Loading));
        crate::anim::note_used(&id.to_string());
        let dropped = if list.len() > POOL { list.pop_front() } else { None };
        if let Some((_, State::Ready(image))) = dropped {
            cx.drop_image(image, None);
        }
        let core = core.clone();
        cx.spawn(async move |cx| {
            let job = if format == APNG {
                let url = format!("https://media.discordapp.net/stickers/{id}.png?size=320&passthrough=true");
                let bytes = crate::image_disk::cdn_bytes(&core, url, cx).await;
                cx.background_executor().spawn(async move { render_apng(&bytes?) })
            } else {
                let json = core.sticker_lottie(id).await.ok();
                cx.background_executor().spawn(async move { render(json?, id.to_string()) })
            };
            let state = match job.await {
                Some(image) => State::Ready(Arc::new(image)),
                None => State::Failed,
            };
            let _ = cx.update(|cx| {
                if let Some(entry) = cx.global_mut::<Stickers>().0.iter_mut().find(|(known, _)| *known == id) {
                    entry.1 = state;
                }
                crate::ui::redraw_chat(cx);
            });
        })
        .detach();
    }
}

/// Lets go of stickers that aren't showing.
pub fn rest(cx: &mut App) {
    const REST_AFTER: std::time::Duration = std::time::Duration::from_secs(15);
    let Some(list) = cx.try_global::<Stickers>() else { return };
    let gone: Vec<Id> = list.0.iter().filter(|(id, state)| !matches!(state, State::Loading) && crate::anim::unused_for(&id.to_string(), REST_AFTER)).map(|(id, _)| *id).collect();
    for id in gone {
        let list = &mut cx.global_mut::<Stickers>().0;
        let Some(at) = list.iter().position(|(known, _)| *known == id) else { continue };
        if let Some((_, State::Ready(image))) = list.remove(at) {
            cx.drop_image(image, None);
        }
    }
}

/// The rendered sticker, or `None` while loading. `Err` if it couldn't be.
pub fn image(id: Id, cx: &App) -> Result<Option<Arc<RenderImage>>, ()> {
    crate::anim::note_used(&id.to_string());
    let state = cx.try_global::<Stickers>().and_then(|s| s.0.iter().find(|(known, _)| *known == id));
    match state.map(|(_, s)| s) {
        Some(State::Ready(image)) => Ok(Some(image.clone())),
        Some(State::Failed) => Err(()),
        _ => Ok(None),
    }
}
