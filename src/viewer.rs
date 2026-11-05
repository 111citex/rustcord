//! The picture viewer: clicking a picture in the chat opens it over the
//! whole window, fitted to it, instead of in the browser. A click outside
//! it or Escape closes it; the wheel zooms, dragging moves it.

use gpui_kit::component::ActiveTheme;
use gpui_kit::{
    Animation, AnimationExt, App, FontWeight, ScrollDelta, Global, InteractiveElement, IntoElement, KeyDownEvent, MouseButton, ObjectFit, ParentElement,
    Styled, Window, div,
};

use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::relative;

use crate::ui::u;

struct Viewing {
    /// Every picture in the conversation, and which of them this is.
    gallery: Vec<Shown>,
    at: Option<usize>,
    /// Fading out.
    closing: bool,
    /// The picture, and what "Open in Browser" opens (a post's page, say).
    src: String,
    open: String,
    dims: Option<(u32, u32)>,
    /// Which opening this is: each gets its own animation.
    opened: usize,
    /// How far it's zoomed (1 fits the window) and moved, in UI pixels.
    zoom: f32,
    pan: (f32, f32),
    /// Where the pointer last was while dragging it, in window pixels.
    dragging: Option<(f32, f32)>,
}

impl Global for Viewing {}

const OPEN_TIME: std::time::Duration = std::time::Duration::from_millis(180);
const CLOSE_TIME: std::time::Duration = std::time::Duration::from_millis(150);

pub fn show(src: &str, open: &str, dims: Option<(u32, u32)>, cx: &mut App) {
    static OPENINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let opened = OPENINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let gallery = GALLERY.with_borrow(Clone::clone).map_or_else(Vec::new, |list| list(cx));
    let at = gallery.iter().position(|shown| shown.src == src);
    cx.set_global(Viewing { gallery, at, closing: false, src: src.to_string(), open: open.to_string(), dims, opened, zoom: 1., pan: (0., 0.), dragging: None });
    cx.refresh_windows();
}

/// A picture in the conversation, and who sent it.
#[derive(Clone)]
pub struct Shown {
    pub src: String,
    pub open: String,
    pub dims: Option<(u32, u32)>,
    pub author: String,
    pub avatar: String,
    pub when: String,
}

thread_local! {
    static GALLERY: std::cell::RefCell<Option<std::rc::Rc<dyn Fn(&App) -> Vec<Shown>>>> = const { std::cell::RefCell::new(None) };
}

/// Tells how to list the pictures in the conversation. Set by the chat.
pub fn watch_gallery(list: std::rc::Rc<dyn Fn(&App) -> Vec<Shown>>) {
    GALLERY.with_borrow_mut(|g| *g = Some(list));
}

/// Moves to picture `to` of the conversation.
fn go_to(to: usize, cx: &mut App) {
    let Some(view) = cx.try_global::<Viewing>().filter(|v| !v.closing) else { return };
    let Some(shown) = view.gallery.get(to).cloned() else { return };
    let view = cx.global_mut::<Viewing>();
    view.at = Some(to);
    view.src = shown.src;
    view.open = shown.open;
    view.dims = shown.dims;
    view.zoom = 1.;
    view.pan = (0., 0.);
    cx.refresh_windows();
}

fn step(by: isize, cx: &mut App) {
    if let Some(at) = cx.try_global::<Viewing>().and_then(|v| v.at)
        && let Some(to) = at.checked_add_signed(by)
    {
        go_to(to, cx);
    }
}

/// The thumbnails to show around picture `at` of `count`, at most `most`.
fn strip_range(at: usize, count: usize, most: usize) -> std::ops::Range<usize> {
    let start = at.saturating_sub(most / 2).min(count.saturating_sub(most));
    start..(start + most).min(count)
}

/// A round button over the picture.
fn round_button(id: &'static str, icon: gpui_kit::assets::IconName, on_press: impl Fn(&mut App) + 'static) -> gpui_kit::Stateful<gpui_kit::Div> {
    div()
        .id(id)
        .size(u(40.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .bg(gpui_kit::black().opacity(0.6))
        .text_color(gpui_kit::white())
        .cursor_pointer()
        .hover(|d| d.bg(gpui_kit::black().opacity(0.85)))
        .on_mouse_down(MouseButton::Left, move |_, _, cx| {
            cx.stop_propagation();
            on_press(cx);
        })
        .child(gpui_kit::component::Icon::new(icon).size(u(20.)))
}

pub fn close(cx: &mut App) {
    let Some(viewing) = cx.try_global::<Viewing>() else { return };
    if viewing.closing {
        return;
    }
    let opened = viewing.opened;
    cx.global_mut::<Viewing>().closing = true;
    cx.refresh_windows();
    // Gone once it's faded out, unless another's been opened since.
    cx.spawn(async move |cx| {
        cx.background_executor().timer(CLOSE_TIME).await;
        cx.update(|cx| {
            if cx.try_global::<Viewing>().is_some_and(|v| v.opened == opened) {
                cx.remove_global::<Viewing>();
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

thread_local! {
    /// Stills of GIFs that are really videos, and those videos.
    static VIDEOS: std::cell::RefCell<std::collections::HashMap<String, String>> = std::cell::RefCell::default();
}

/// Says the picture `still` is really the video at `video`.
pub fn link_video(still: &str, video: &str) {
    VIDEOS.with_borrow_mut(|v| {
        if v.len() > 500 {
            v.clear();
        }
        v.insert(still.to_string(), video.to_string());
    });
}

fn linked_video(still: &str) -> Option<String> {
    VIDEOS.with_borrow(|v| v.get(still).cloned())
}

pub fn is_open(cx: &App) -> bool {
    cx.has_global::<Viewing>()
}

/// Escape closes the viewer; for the window root's key handler.
pub fn on_key(event: &KeyDownEvent, _: &mut Window, cx: &mut App) {
    if is_open(cx) && matches!(event.keystroke.key.as_str(), "left" | "right") {
        step(if event.keystroke.key == "left" { -1 } else { 1 }, cx);
        cx.stop_propagation();
        return;
    }
    if event.keystroke.key == "escape" && is_open(cx) {
        // Its right click menu goes with it.
        crate::context_menu::close(cx);
        close(cx);
        cx.stop_propagation();
    }
}

/// The open picture over a dimmed window, or nothing. The wheel zooms it,
/// dragging moves it, a right click has the usual actions.
pub fn overlay(window: &Window, cx: &App) -> Option<gpui_kit::AnyElement> {
    let v = cx.try_global::<Viewing>()?;
    let viewport = window.viewport_size();
    let rem = f32::from(window.rem_size()) / 16.;
    // The room the picture gets, in the UI's own (zoomed) pixels.
    let (room_w, room_h) = (f32::from(viewport.width) / rem * 0.9, f32::from(viewport.height) / rem * if v.gallery.len() > 1 { 0.7 } else { 0.8 });
    let (w, h) = match v.dims {
        Some((w, h)) if w > 0 && h > 0 => {
            let scale = (room_w / w as f32).min(room_h / h as f32).min(1.);
            (w as f32 * scale, h as f32 * scale)
        }
        _ => (room_w, room_h),
    };
    // Fetched at its own size (within reason), so zooming in stays sharp.
    let (full_w, full_h) = v.dims.filter(|(w, h)| *w > 0 && *h > 0).map_or((w as u32, h as u32), |(w, h)| (w.min(2048), h.min(2048)));
    let raw_src = v.src.clone();
    let src = cord_core::cdn::sharp_url(&v.src, full_w, full_h);
    let open = v.open.clone();
    let (raw, link) = (v.src.clone(), v.open.clone());
    let (zoom, pan) = (v.zoom, v.pan);
    let theme = cx.theme();
    let (closing, opened) = (v.closing, v.opened);
    use gpui_kit::assets::IconName;
    let count = v.gallery.len();
    let current = v.at.and_then(|at| v.gallery.get(at).cloned().map(|shown| (at, shown)));
    let header = current.as_ref().map(|(_, shown)| {
        h_flex()
            .absolute()
            .top(u(16.))
            .left(u(24.))
            .gap(u(12.))
            .items_center()
            .text_color(gpui_kit::white())
            .child(gpui_kit::img(shown.avatar.clone()).size(u(40.)).rounded_full())
            .child(
                v_flex()
                    .child(div().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(shown.author.clone()))
                    .child(div().text_size(u(12.)).text_color(gpui_kit::white().opacity(0.7)).child(shown.when.clone())),
            )
    });
    let thumbnails: Vec<_> = current.as_ref().filter(|_| count > 1).map_or_else(Vec::new, |(at, _)| {
        strip_range(*at, count, 11)
            .map(|i| {
                let shown = &v.gallery[i];
                let selected = i == *at;
                // Big enough to cover its box whatever its shape, as the proxy only fits inside.
                const THUMB: f32 = 56.;
                let (ask_w, ask_h) = shown.dims.map_or((THUMB, THUMB), |(w, h)| {
                    let scale = (THUMB / w.max(1) as f32).max(THUMB / h.max(1) as f32);
                    (w as f32 * scale, h as f32 * scale)
                });
                let url = cord_core::cdn::sharp_url(&shown.src, (ask_w * 2.).ceil() as u32, (ask_h * 2.).ceil() as u32);
                div()
                    .id(("viewer-thumb", i))
                    .relative()
                    .size(u(THUMB))
                    .flex_shrink_0()
                    .when(!selected, |d| d.opacity(0.6).hover(|d| d.opacity(1.)))
                    .cursor_pointer()
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        cx.stop_propagation();
                        go_to(i, cx);
                    })
                    .child(crate::anim::image(url, ObjectFit::Cover, 8.).absolute().top_0().left_0().size_full())
                    // Drawn over the picture, on its very edge.
                    .when(selected, |d| {
                        d.child(div().absolute().top_0().left_0().size_full().rounded(u(8.)).border_2().border_color(gpui_kit::white()))
                    })
            })
            .collect()
    });
    let (can_back, can_on) = current.as_ref().map_or((false, false), |(at, _)| (*at > 0, *at + 1 < count));
    let root =
        div()
            .id("viewer")
            .occlude()
            .absolute()
            .inset_0()
            .overflow_hidden()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(u(12.))
            .bg(gpui_kit::black().opacity(0.85))
            .on_mouse_down(MouseButton::Left, |_, _, cx| close(cx))
            .on_scroll_wheel(|event, window, cx| {
                cx.stop_propagation();
                let lines = match event.delta {
                    ScrollDelta::Lines(lines) => lines.y,
                    ScrollDelta::Pixels(pixels) => f32::from(pixels.y) / f32::from(window.line_height()),
                };
                // Keeps the spot under the pointer under the pointer: the picture
                // grows around its centre, so it moves out by the difference.
                let scale = f32::from(window.rem_size()) / 16.;
                let size = window.viewport_size();
                let centre = (f32::from(size.width) / scale / 2., f32::from(size.height) / scale / 2. - LABELS_BELOW / 2.);
                let pointer = (f32::from(event.position.x) / scale, f32::from(event.position.y) / scale);
                let view = cx.global_mut::<Viewing>();
                let zoom = (view.zoom * (1. + lines * 0.15)).clamp(1., 8.);
                let grew = zoom / view.zoom;
                view.pan = zoomed_pan(view.pan, centre, pointer, grew);
                view.zoom = zoom;
                if view.zoom <= 1. {
                    view.pan = (0., 0.);
                }
                cx.refresh_windows();
            })
            .on_mouse_move(move |event, _, cx| {
                let Some(view) = cx.try_global::<Viewing>() else { return };
                let Some(last) = view.dragging else { return };
                if event.pressed_button != Some(MouseButton::Left) {
                    cx.global_mut::<Viewing>().dragging = None;
                    return;
                }
                let now = (f32::from(event.position.x), f32::from(event.position.y));
                let view = cx.global_mut::<Viewing>();
                view.pan = (view.pan.0 + (now.0 - last.0) / rem, view.pan.1 + (now.1 - last.1) / rem);
                view.dragging = Some(now);
                cx.refresh_windows();
            })
            .child(
                div()
                    .id("viewer-picture")
                    // Pressing the picture itself grabs it rather than closing.
                    .on_mouse_down(MouseButton::Left, |event, _, cx| {
                        cx.stop_propagation();
                        cx.global_mut::<Viewing>().dragging = Some((f32::from(event.position.x), f32::from(event.position.y)));
                    })
                    .on_mouse_up(MouseButton::Left, |_, _, cx| {
                        if cx.has_global::<Viewing>() {
                            cx.global_mut::<Viewing>().dragging = None;
                        }
                    })
                    .on_mouse_down(MouseButton::Right, move |event, _, cx| {
                        cx.stop_propagation();
                        show_menu(event.position, raw.clone(), link.clone(), cx);
                    })
                    .relative()
                    .left(u(pan.0))
                    .top(u(pan.1))
                    // A GIF that's really a video shows as what it is, not its still.
                    .child(match linked_video(&raw_src).and_then(|url| crate::gifs::player(&url, cx)) {
                        Some(player) => alhazen_gpui::video_view(player).corner_radius(u(4.)).object_fit(ObjectFit::Contain).w(u(w * zoom)).h(u(h * zoom)).flex_shrink_0().into_any_element(),
                        None => crate::anim::image_fading(src, None, ObjectFit::Contain, 4., gpui_kit::black()).w(u(w * zoom)).h(u(h * zoom)).flex_shrink_0().into_any_element(),
                    })
                    // Fades in and settles: the wait for the picture reads as an opening.
                    .with_animation(
                        ("viewer-open", v.opened),
                        Animation::new(std::time::Duration::from_millis(180)).with_easing(gpui_kit::ease_out_quint()),
                        |d, t| d.opacity(t),
                    ),
            )
            .child(
                div()
                    .id("viewer-open")
                    .text_size(u(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(gpui_kit::white().opacity(0.7))
                    .cursor_pointer()
                    .hover(|d| d.text_color(gpui_kit::white()).underline())
                    .on_mouse_down(MouseButton::Left, move |_, _, cx| {
                        cx.stop_propagation();
                        cx.open_url(&open);
                        close(cx);
                    })
                    .child("Open in Browser"),
            )
            .child(div().text_size(u(12.)).text_color(theme.muted_foreground).child("Scroll to zoom, drag to move, Esc to close"));
    let root = root
        .children(header)
        .child(div().absolute().top(u(16.)).right(u(24.)).child(round_button("viewer-close-button", IconName::Close, close)))
        .when(can_back, |d| d.child(div().absolute().left(u(24.)).top(relative(0.5)).child(round_button("viewer-back", IconName::ChevronLeft, |cx| step(-1, cx)))))
        .when(can_on, |d| d.child(div().absolute().right(u(24.)).top(relative(0.5)).child(round_button("viewer-on", IconName::ChevronRight, |cx| step(1, cx)))))
        .when(!thumbnails.is_empty(), |d| d.child(h_flex().absolute().bottom(u(12.)).left_0().right_0().justify_center().gap(u(6.)).items_center().children(thumbnails)));
    Some(if closing {
        root.with_animation(("viewer-close", opened), Animation::new(CLOSE_TIME).with_easing(gpui_kit::ease_out_quint()), |d, t| d.opacity(1. - t)).into_any_element()
    } else {
        root.with_animation(("viewer-backdrop", opened), Animation::new(OPEN_TIME).with_easing(gpui_kit::ease_out_quint()), |d, t| d.opacity(t)).into_any_element()
    })
}

/// The height of the two lines under the picture, which push its centre up.
const LABELS_BELOW: f32 = 62.;

/// The pan after growing by `grew` that keeps what's at `pointer` there.
fn zoomed_pan(pan: (f32, f32), centre: (f32, f32), pointer: (f32, f32), grew: f32) -> (f32, f32) {
    let keep = |pan: f32, centre: f32, pointer: f32| pointer - centre - (pointer - centre - pan) * grew;
    (keep(pan.0, centre.0, pointer.0), keep(pan.1, centre.1, pointer.1))
}

/// A picture's file name, from its URL.
pub fn file_name(url: &str) -> String {
    let path = url.split(['?', '#']).next().unwrap_or("");
    let name = path.rsplit('/').next().unwrap_or("").trim();
    if name.is_empty() { "image".to_string() } else { name.to_string() }
}

/// Fetches `url`'s bytes with curl, off the UI thread.
async fn fetch(url: String, cx: &gpui_kit::AsyncApp) -> Option<Vec<u8>> {
    cx.background_executor()
        .spawn(async move {
            let output = std::process::Command::new("curl").args(["-sSfL", "--max-time", "60", &url]).output().ok()?;
            output.status.success().then_some(output.stdout)
        })
        .await
}

/// Puts a picture on the clipboard as a PNG (what every app can paste), with
/// wl-copy: GPUI's Wayland clipboard only carries text.
fn copy_to_clipboard(bytes: &[u8]) -> bool {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let Ok(picture) = image::load_from_memory(bytes) else { return false };
    let mut png = std::io::Cursor::new(Vec::new());
    if picture.write_to(&mut png, image::ImageFormat::Png).is_err() {
        return false;
    }
    let Ok(mut child) = Command::new("wl-copy").args(["--type", "image/png"]).stdin(Stdio::piped()).spawn() else { return false };
    let wrote = child.stdin.take().is_some_and(|mut stdin| stdin.write_all(png.get_ref()).is_ok());
    wrote && child.wait().is_ok_and(|s| s.success())
}

pub fn show_menu(position: gpui_kit::Point<gpui_kit::Pixels>, src: String, link: String, cx: &mut App) {
    use crate::context_menu::{self as menu, Entry};
    let (save, copy) = (src.clone(), src.clone());
    let entries = vec![
        Entry::item("Copy Image", move |_, cx| {
            let url = copy.clone();
            cx.spawn(async move |cx| {
                let Some(bytes) = fetch(url, cx).await else { return };
                let job = cx.background_executor().spawn(async move { copy_to_clipboard(&bytes).then_some(()).ok_or(bytes) });
                // wl-copy not there or failed: GPUI's clipboard is the fallback.
                if let Err(bytes) = job.await {
                    let item = gpui_kit::ClipboardItem { entries: vec![gpui_kit::ClipboardEntry::Image(gpui_kit::Image::from_bytes(gpui_kit::ImageFormat::Png, bytes))] };
                    let _ = cx.update(|cx| cx.write_to_clipboard(item));
                }
            })
            .detach();
        }),
        Entry::item("Save Image", move |_, cx| {
            let url = save.clone();
            cx.spawn(async move |cx| {
                let Some(bytes) = fetch(url.clone(), cx).await else { return };
                let dir = dirs::download_dir().unwrap_or_else(std::env::temp_dir);
                let path = dir.join(file_name(&url));
                if let Err(error) = std::fs::write(&path, bytes) {
                    tracing::warn!(%error, "couldn't save a picture");
                } else {
                    tracing::info!(path = %path.display(), "saved a picture");
                }
            })
            .detach();
        }),
        Entry::Separator,
        Entry::copy("Copy Link", src.clone()),
        Entry::item("Open in Browser", move |_, cx| cx.open_url(&link)),
    ];
    menu::show(position, entries, cx);
}

#[cfg(test)]
mod tests {
    #[test]
    fn zooming_keeps_the_spot_under_the_pointer() {
        // A spot 100 right of centre, doubled, would land 200 right: pan back 100.
        assert_eq!(super::zoomed_pan((0., 0.), (500., 300.), (600., 300.), 2.), (-100., 0.));
        // At the centre nothing moves.
        assert_eq!(super::zoomed_pan((0., 0.), (500., 300.), (500., 300.), 2.), (0., 0.));
    }

    #[test]
    fn file_names_come_from_the_path() {
        assert_eq!(super::file_name("https://x.y/a/b.png?ex=1"), "b.png");
        assert_eq!(super::file_name("https://x.y/"), "image");
    }
}
