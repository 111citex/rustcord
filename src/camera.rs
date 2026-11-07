//! Someone's camera, drawn from the newest picture the call has of it.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use gpui_kit::{App, Corners, Entity, IntoElement, ObjectFit, RenderImage, Styled, canvas};

use crate::store::Store;

/// Pictures that left the screen are kept this many frames before going: one still
/// being drawn can't lose its pixels under it.
const KEEP_OLD: usize = 3;

struct Shown {
    number: u64,
    image: Arc<RenderImage>,
    old: Vec<Arc<RenderImage>>,
}

thread_local! {
    static SHOWN: RefCell<HashMap<u64, Shown>> = RefCell::default();
    /// Something's being resized by hand: the decoder keeps its size until it's let go,
    /// rather than rescaling at every step of the drag.
    static HOLD_SIZE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Holds, or lets go of, the decode size while a video's being resized.
pub fn hold_size(held: bool) {
    HOLD_SIZE.set(held);
}

/// `user`'s camera, or their Go Live with `stream`, filling whatever it's sized to,
/// cropped to fit.
pub fn view(store: Entity<Store>, user: u64, radius: f32, stream: bool) -> impl IntoElement + Styled {
    view_fit(store, user, radius, stream, ObjectFit::Cover)
}

/// `view`, fitted as asked: `Contain` shows all of it, with bars.
pub fn view_fit(store: Entity<Store>, user: u64, radius: f32, stream: bool, fit: ObjectFit) -> impl IntoElement + Styled {
    // A stream and a camera of the same person are kept apart.
    let key = if stream { user | 1 << 63 } else { user };
    canvas(
        |_, _, _| (),
        move |bounds, (), window, cx: &mut App| {
            let picture = if stream { store.read(cx).stream_picture(cord_core::Id(user)) } else { store.read(cx).camera(cord_core::Id(user)) };
            let Some(picture) = picture else { return };
            // The decoder makes frames this size, at most: what's drawn, not what's sent.
            let scale = window.scale_factor();
            let device = |px: gpui_kit::Pixels| (f32::from(px) * scale).ceil().max(16.) as u32;
            if !HOLD_SIZE.get() {
                store.read(cx).show_video_at(cord_core::Id(user), stream, (device(bounds.size.width), device(bounds.size.height)));
            }
            let user = key;
            let (image, retired) = SHOWN.with_borrow_mut(|shown| {
                let fresh = shown.get(&user).is_none_or(|s| s.number != picture.number);
                if fresh {
                    let Some(buffer) = image::RgbaImage::from_raw(picture.width, picture.height, picture.bgra.to_vec()) else {
                        return (shown.get(&user).map(|s| s.image.clone()), Vec::new());
                    };
                    // GPUI's images are BGRA, which is what the decoder gave.
                    let image = Arc::new(RenderImage::new([image::Frame::new(buffer)]));
                    let entry = shown.entry(user).or_insert_with(|| Shown { number: picture.number, image: image.clone(), old: Vec::new() });
                    if entry.image.id != image.id {
                        let previous = std::mem::replace(&mut entry.image, image);
                        entry.old.push(previous);
                    }
                    entry.number = picture.number;
                    let excess = entry.old.len().saturating_sub(KEEP_OLD);
                    let retired: Vec<_> = entry.old.drain(..excess).collect();
                    (Some(entry.image.clone()), retired)
                } else {
                    (shown.get(&user).map(|s| s.image.clone()), Vec::new())
                }
            });
            for old in retired {
                let _ = window.drop_image(old);
            }
            if let Some(image) = image {
                let corners = Corners::all(window.rem_size() * (radius / 16.));
                let fitted = fit.get_bounds(bounds, image.size(0));
                let _ = window.paint_image(bounds, fitted, corners, image, 0, false);
            }
            // About 30 a second while it shows: cameras don't go faster than that, much.
            alhazen_gpui::redraw_after(Duration::from_millis(33), window, cx);
        },
    )
}

/// Lets go of the pictures of cameras no longer on.
pub fn forget_others(on: &[u64], streaming: Option<u64>, cx: &mut App) {
    let gone: Vec<Shown> = SHOWN.with_borrow_mut(|shown| {
        let users: Vec<u64> = shown.keys().copied().filter(|u| !on.contains(u) && streaming.is_none_or(|s| *u != s | 1 << 63)).collect();
        users.iter().filter_map(|u| shown.remove(u)).collect()
    });
    for shown in gone {
        for image in shown.old.into_iter().chain([shown.image]) {
            cx.drop_image(image, None);
        }
    }
}
