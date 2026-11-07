//! Picture in picture: someone's camera in a small window inside the window, while the
//! call's page isn't the one showing. Dragged by any part, resized from its corner,
//! always 16:9, as Discord's is.

use cord_core::Id;
use gpui_kit::component::{ActiveTheme, Icon, h_flex};
use gpui_kit::{
    AnyElement, App, Entity, Global, InteractiveElement as _, IntoElement as _, ParentElement as _, Pixels, Point, SharedString,
    StatefulInteractiveElement as _, Styled as _, Window, deferred, div, point, px,
};

use gpui_kit::assets::IconName;
use crate::call_ring::clamp_to_window;
use gpui_kit::prelude::FluentBuilder as _;
use crate::store::Store;
use crate::ui::u;

/// Width, in Discord's pixels: where it starts, and how small and big it goes.
const START_WIDTH: f32 = 320.;
const LEAST_WIDTH: f32 = 240.;
const MOST_WIDTH: f32 = 960.;
/// The corner that resizes it.
const GRIP: f32 = 18.;

#[derive(Default)]
struct Pip {
    /// Top left, once moved; bottom right of the window until then.
    at: Option<Point<Pixels>>,
    /// Width in Discord's pixels; the height follows at 16:9.
    width: Option<f32>,
    /// Being dragged: where in it the pointer took hold.
    grab: Option<Point<Pixels>>,
    /// Being resized: where the pointer started, and the width then.
    resize: Option<(Point<Pixels>, f32)>,
    /// Whose camera, when there's a choice; the first on otherwise.
    showing: Option<u64>,
    /// Closed by the × until a camera turns on that wasn't on.
    hidden_for: Option<Vec<u64>>,
    /// It moved or changed size since the button went down: that wasn't a click.
    moved: bool,
}

impl Global for Pip {}

fn pip(cx: &mut App) -> &mut Pip {
    if !cx.has_global::<Pip>() {
        cx.set_global(Pip::default());
    }
    cx.global_mut::<Pip>()
}

fn size(width: f32) -> (f32, f32) {
    (width, width * 9. / 16.)
}

/// Where it sits by default: in the bottom right corner, clear of the edges.
fn default_at(width: f32, window: &Window) -> Point<Pixels> {
    let scale = f32::from(window.rem_size()) / 16.;
    let (w, h) = size(width);
    let viewport = window.viewport_size();
    point(viewport.width - px((w + 24.) * scale), viewport.height - px((h + 96.) * scale))
}

/// Follows the pointer while it's dragged or resized, wherever the pointer goes.
fn watcher() -> impl gpui_kit::IntoElement {
    gpui_kit::canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &gpui_kit::MouseMoveEvent, phase, window, cx| {
                if phase != gpui_kit::DispatchPhase::Capture {
                    return;
                }
                let Some(state) = cx.try_global::<Pip>() else { return };
                if state.grab.is_none() && state.resize.is_none() {
                    return;
                }
                if !event.dragging() {
                    let state = cx.global_mut::<Pip>();
                    state.grab = None;
                    state.resize = None;
                    crate::camera::hold_size(false);
                    return;
                }
                let scale = f32::from(window.rem_size()) / 16.;
                let viewport = window.viewport_size();
                let state = cx.global_mut::<Pip>();
                state.moved = true;
                let width = state.width.unwrap_or(START_WIDTH);
                if let Some((from, was)) = state.resize {
                    // Wider by however far right or down it went, the larger of the two at 16:9.
                    let dx = f32::from(event.position.x - from.x) / scale;
                    let dy = f32::from(event.position.y - from.y) / scale * 16. / 9.;
                    state.width = Some((was + dx.max(dy)).clamp(LEAST_WIDTH, MOST_WIDTH));
                } else if let Some(grab) = state.grab {
                    let wanted = event.position - grab;
                    let (w, h) = size(width);
                    let kept = clamp_to_window(
                        point(f32::from(wanted.x), f32::from(wanted.y)),
                        (w * scale, h * scale),
                        (f32::from(viewport.width), f32::from(viewport.height)),
                        48. * scale,
                    );
                    state.at = Some(point(px(kept.x), px(kept.y)));
                }
                crate::ui::redraw_overlays(cx);
            });
            window.on_mouse_event(|_: &gpui_kit::MouseUpEvent, _, _, cx| {
                if let Some(state) = cx.try_global::<Pip>()
                    && (state.grab.is_some() || state.resize.is_some())
                {
                    let state = cx.global_mut::<Pip>();
                    state.grab = None;
                    state.resize = None;
                    crate::camera::hold_size(false);
                    crate::ui::redraw_overlays(cx);
                }
            });
        },
    )
    .absolute()
    .size_0()
}

/// The window, when there's a camera to show and the call's own page isn't showing.
pub fn overlay(store: &Entity<Store>, looking_at: Option<Id>, window: &Window, cx: &mut App) -> Option<AnyElement> {
    let status = store.read(cx).voice_status()?;
    if looking_at == Some(status.channel) {
        return None;
    }
    let mut cameras = store.read(cx).cameras();
    // A Go Live we're watching comes before any camera.
    let stream = store.read(cx).watching().map(|w| w.0).filter(|w| store.read(cx).stream_picture(Id(*w)).is_some());
    if cameras.is_empty() && stream.is_none() {
        return None;
    }
    cameras.sort_unstable();
    let state = pip(cx);
    // Closed: stays so until someone new turns theirs on.
    if let Some(hidden_for) = &state.hidden_for {
        if cameras.iter().all(|c| hidden_for.contains(c)) {
            return None;
        }
        state.hidden_for = None;
    }
    let user = stream.or(state.showing.filter(|u| cameras.contains(u))).or(cameras.first().copied())?;
    let width = state.width.unwrap_or(START_WIDTH);
    let at = state.at.unwrap_or_else(|| default_at(width, window));
    let (w, h) = size(width);
    let name = store.read(cx).cache().user(Id(user)).map(|u| store.read(cx).cache().display_name(status.guild, u).to_string()).unwrap_or_default();
    let theme = cx.theme();
    let channel = status.channel;
    let guild = status.guild;
    let corner_button = |id: &'static str, icon: IconName, tip: &'static str| {
        div()
            .id(id)
            .size(u(28.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(u(6.))
            .bg(gpui_kit::black().opacity(0.6))
            .text_color(gpui_kit::white())
            .cursor_pointer()
            .hover(|d| d.bg(gpui_kit::black().opacity(0.85)))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(Icon::new(icon).size(u(16.)))
    };
    let next = cameras.iter().position(|c| *c == user).map(|i| cameras[(i + 1) % cameras.len()]);
    let previous = cameras.iter().position(|c| *c == user).map(|i| cameras[(i + cameras.len() - 1) % cameras.len()]);
    let many = stream.is_none() && cameras.len() > 1;
    let hide_for = cameras.clone();
    let card = div()
        .id("video-pip")
        .group("video-pip")
        .occlude()
        .relative()
        .w(u(w))
        .h(u(h))
        .rounded(u(10.))
        .overflow_hidden()
        .border_1()
        .border_color(theme.border)
        .bg(gpui_kit::black())
        .shadow_lg()
        .cursor_grab()
        .on_mouse_down(gpui_kit::MouseButton::Left, move |event: &gpui_kit::MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let state = pip(cx);
            let at = state.at.unwrap_or_else(|| default_at(state.width.unwrap_or(START_WIDTH), window));
            state.at = Some(at);
            state.grab = Some(event.position - at);
            state.moved = false;
        })
        // A click that didn't move it goes to the call.
        .on_click(move |event, _, cx| {
            if event.click_count() >= 1 && cx.try_global::<Pip>().is_some_and(|s| !s.moved) {
                let to = match guild {
                    Some(guild) => format!("/channels/{guild}/{channel}"),
                    None => format!("/channels/@me/{channel}"),
                };
                crate::links::go(to, cx);
            }
        })
        .child(crate::camera::view(store.clone(), user, 10., stream.is_some()).absolute().top_0().left_0().size_full())
        .child(
            div()
                .absolute()
                .left(u(8.))
                .bottom(u(8.))
                .max_w(gpui_kit::relative(0.8))
                .px(u(8.))
                .py(u(3.))
                .rounded(u(6.))
                .bg(gpui_kit::black().opacity(0.7))
                .text_color(gpui_kit::white())
                .text_size(u(13.))
                .truncate()
                .child(SharedString::from(name)),
        )
        // On hover: switch whose camera, or close it.
        .child(
            h_flex()
                .absolute()
                .top(u(8.))
                .right(u(8.))
                .gap(u(6.))
                .opacity(0.)
                .group_hover("video-pip", |d| d.opacity(1.))
                .when(many, |d| {
                    d.child(corner_button("pip-previous", IconName::ChevronLeft, "Previous camera").on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        pip(cx).showing = previous;
                        crate::ui::redraw_overlays(cx);
                    }))
                    .child(corner_button("pip-next", IconName::ChevronRight, "Next camera").on_click(move |_, _, cx| {
                        cx.stop_propagation();
                        pip(cx).showing = next;
                        crate::ui::redraw_overlays(cx);
                    }))
                })
                .child(corner_button("pip-close", IconName::Close, "Hide").on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    pip(cx).hidden_for = Some(hide_for.clone());
                    crate::ui::redraw_overlays(cx);
                })),
        )
        // The corner that resizes it, keeping 16:9.
        .child(
            div()
                .id("pip-resize")
                .absolute()
                .right_0()
                .bottom_0()
                .size(u(GRIP))
                .cursor(gpui_kit::CursorStyle::ResizeUpLeftDownRight)
                .on_mouse_down(gpui_kit::MouseButton::Left, move |event: &gpui_kit::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let state = pip(cx);
                    state.resize = Some((event.position, state.width.unwrap_or(START_WIDTH)));
                    state.moved = true;
                    crate::camera::hold_size(true);
                })
                .child(
                    div()
                        .absolute()
                        .right(u(4.))
                        .bottom(u(4.))
                        .size(u(8.))
                        .border_r_2()
                        .border_b_2()
                        .border_color(gpui_kit::white().opacity(0.7))
                        .opacity(0.)
                        .group_hover("video-pip", |d| d.opacity(1.)),
                ),
        );
    // Exactly where it was put. `anchored` would flip it to its other corner whenever part of
    // it's outside the window, then back the next frame: it shook. It's kept on screen above.
    Some(deferred(div().absolute().left(at.x).top(at.y).child(watcher()).child(card)).with_priority(4).into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn it_stays_sixteen_by_nine() {
        let (w, h) = size(320.);
        assert!((w / h - 16. / 9.).abs() < 0.001);
    }
}

/// A Go Live filling the window, as fullscreen videos do: all of it, on black.
pub fn fullscreen_overlay(store: &Entity<Store>, cx: &App) -> Option<AnyElement> {
    let key = crate::videos::fullscreen_key(cx)?;
    let user: u64 = key.strip_prefix("stream:")?.parse().ok()?;
    store.read(cx).stream_picture(Id(user))?;
    let frame = div()
        .id("stream-fullscreen")
        .group("stream-fullscreen")
        .absolute()
        .top_0()
        .left_0()
        .size_full()
        .bg(gpui_kit::black())
        .occlude()
        .child(crate::camera::view_fit(store.clone(), user, 0., true, gpui_kit::ObjectFit::Contain).absolute().top_0().left_0().size_full())
        .child(
            div()
                .id("stream-fullscreen-leave")
                .absolute()
                .top(u(16.))
                .right(u(16.))
                .size(u(40.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(8.))
                .bg(gpui_kit::black().opacity(0.6))
                .text_color(gpui_kit::white())
                .cursor_pointer()
                .opacity(0.)
                .group_hover("stream-fullscreen", |d| d.opacity(1.))
                .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Exit Full Screen (Esc)").build(window, cx))
                .on_click(|_, window, cx| crate::videos::leave_fullscreen(window, cx))
                .child(Icon::new(IconName::Minimize).size(u(20.))),
        );
    Some(deferred(frame).with_priority(4).into_any_element())
}
