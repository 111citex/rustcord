//! Long GIFs, decoded a frame at a time. Their file stays in memory (it's small) and
//! a thread decodes the next frame as the last is taken, so the thread sits idle when
//! nobody's looking. Short ones are all decoded up front: decoding them over and over
//! would cost more than keeping them.

use std::collections::HashMap;
use std::io::Cursor;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, sync_channel};
use std::time::{Duration, Instant};

use gpui_kit::{App, ImageId, RenderImage, StreamSource, Window};
use image::AnimationDecoder;

const MIN_WAKE: Duration = Duration::from_millis(33);
/// Frames kept after being replaced: a redraw can still be showing one.
const KEEP_OLD: usize = 2;

struct Stream {
    frames: Receiver<image::Frame>,
    current: Option<Arc<RenderImage>>,
    old: Vec<Arc<RenderImage>>,
    next_at: Instant,
    drawn: Instant,
}

thread_local! {
    static STREAMS: std::cell::RefCell<HashMap<ImageId, Stream>> = std::cell::RefCell::default();
}

/// Decodes frames for as long as somebody's receiving them, from the top again each
/// time the file ends.
fn decode(source: StreamSource, out: std::sync::mpsc::SyncSender<image::Frame>) {
    loop {
        let Ok(decoder) = image::codecs::gif::GifDecoder::new(Cursor::new(source.bytes.clone())) else { return };
        let mut any = false;
        for frame in decoder.into_frames() {
            let Ok(mut frame) = frame else { continue };
            any = true;
            // GPUI's images are BGRA.
            for pixel in frame.buffer_mut().chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            if out.send(gpui_kit::fit_frame(frame, source.fit)).is_err() {
                return;
            }
        }
        if !any {
            return;
        }
    }
}

/// What to paint for `image` right now: itself, unless it's a long GIF.
pub fn frame(image: &Arc<RenderImage>, window: &mut Window, cx: &mut App) -> Arc<RenderImage> {
    let Some(source) = image.stream() else { return image.clone() };
    let now = Instant::now();
    let (shown, wait, retired) = STREAMS.with_borrow_mut(|streams| {
        let stream = streams.entry(image.id).or_insert_with(|| {
            let (out, frames) = sync_channel(1);
            let source = source.clone();
            let _ = std::thread::Builder::new().name("gif-decode".into()).spawn(move || decode(source, out));
            Stream { frames, current: None, old: Vec::new(), next_at: now, drawn: now }
        });
        stream.drawn = now;
        let mut retired = Vec::new();
        if now >= stream.next_at && window.is_window_active()
            && let Ok(frame) = stream.frames.try_recv()
        {
            stream.next_at = now + Duration::from(frame.delay()).max(MIN_WAKE);
            if let Some(previous) = stream.current.replace(Arc::new(RenderImage::new([frame]))) {
                stream.old.push(previous);
            }
            let excess = stream.old.len().saturating_sub(KEEP_OLD);
            retired.extend(stream.old.drain(..excess));
        }
        let wait = stream.next_at.saturating_duration_since(now).max(MIN_WAKE);
        (stream.current.clone(), wait, retired)
    });
    for old in retired {
        let _ = window.drop_image(old);
    }
    if window.is_window_active() {
        alhazen_gpui::redraw_after(wait, window, cx);
    }
    shown.unwrap_or_else(|| image.clone())
}

/// Lets go of streams not drawn since `when`, which ends their threads.
pub fn drop_undrawn_since(when: Instant, cx: &mut App) {
    let gone: Vec<Stream> = STREAMS.with_borrow_mut(|streams| {
        let ids: Vec<ImageId> = streams.iter().filter(|(_, s)| s.drawn < when).map(|(id, _)| *id).collect();
        ids.iter().filter_map(|id| streams.remove(id)).collect()
    });
    for stream in gone {
        for image in stream.old.into_iter().chain(stream.current) {
            cx.drop_image(image, None);
        }
    }
}
