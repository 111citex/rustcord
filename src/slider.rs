//! A continuous slider with a knob: it follows the pointer exactly, even off the track.

use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{App, Bounds, DispatchPhase, Hsla, IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent, ParentElement as _, Pixels, SharedString, Styled as _, div, relative};
use gpui_kit::InteractiveElement as _;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

const THICKNESS: f32 = 4.;
const KNOB: f32 = 12.;

thread_local! {
    /// The slider being dragged. Only one pointer, so only ever one.
    static DRAGGING: RefCell<Option<SharedString>> = const { RefCell::new(None) };
}

fn u(px: f32) -> gpui_kit::Rems {
    gpui_kit::rems(px / 16.)
}

pub fn dragging(id: &str) -> bool {
    DRAGGING.with_borrow(|d| d.as_ref().is_some_and(|d| d.as_ref() == id))
}

/// Where `position` falls along the track, 0 to 1. Upright sliders have 1 at the top.
pub fn level_at(position: gpui_kit::Point<Pixels>, bounds: Bounds<Pixels>, upright: bool) -> f32 {
    let along = if upright {
        f32::from(bounds.bottom() - position.y) / f32::from(bounds.size.height).max(1.)
    } else {
        f32::from(position.x - bounds.left()) / f32::from(bounds.size.width).max(1.)
    };
    along.clamp(0., 1.)
}

/// Fills its parent. `level` is 0 to 1; `set` gets every change while dragging.
pub fn slider(id: impl Into<SharedString>, level: f32, upright: bool, filled: Hsla, track: Hsla, set: Rc<dyn Fn(f32, &mut App)>) -> impl IntoElement {
    slider_with_release(id, level, upright, filled, track, set, None)
}

/// [`slider`], also calling `release` when the drag ends.
pub fn slider_with_release(id: impl Into<SharedString>, level: f32, upright: bool, filled: Hsla, track: Hsla, set: Rc<dyn Fn(f32, &mut App)>, release: Option<Rc<dyn Fn(&mut App)>>) -> impl IntoElement {
    let id: SharedString = id.into();
    let level = level.clamp(0., 1.);
    let bounds = Rc::new(Cell::new(Bounds::<Pixels>::default()));
    let (noted, painted, down_bounds) = (bounds.clone(), bounds.clone(), bounds);
    let (down_id, paint_id, down_set) = (id.clone(), id.clone(), set.clone());
    let bar = div().absolute().rounded_full();
    let knob = div().absolute().size(u(KNOB)).rounded_full().bg(gpui_kit::white()).shadow_sm();
    div()
        .id(id)
        .relative()
        .size_full()
        .flex()
        .items_center()
        .justify_center()
        .cursor_pointer()
        .on_mouse_down(MouseButton::Left, move |event, _, cx| {
            cx.stop_propagation();
            DRAGGING.with_borrow_mut(|d| *d = Some(down_id.clone()));
            down_set(level_at(event.position, down_bounds.get(), upright), cx);
        })
        .child(
            div()
                .relative()
                .rounded_full()
                .bg(track)
                .when(upright, |d| d.h_full().w(u(THICKNESS)))
                .when(!upright, |d| d.w_full().h(u(THICKNESS)))
                .child(if upright { bar.bottom_0().left_0().right_0().h(relative(level)).bg(filled) } else { bar.top_0().left_0().bottom_0().w(relative(level)).bg(filled) })
                .child(if upright {
                    knob.left(u((THICKNESS - KNOB) / 2.)).bottom(relative(level)).mb(u(-KNOB / 2.))
                } else {
                    knob.top(u((THICKNESS - KNOB) / 2.)).left(relative(level)).ml(u(-KNOB / 2.))
                }),
        )
        .child(
            gpui_kit::canvas(
                move |at, _, _| noted.set(at),
                move |_, _, window, _| {
                    let (bounds, id, set) = (painted.clone(), paint_id.clone(), set.clone());
                    let release_missed = release.clone();
                    window.on_mouse_event(move |event: &MouseMoveEvent, phase, _, cx| {
                        if phase != DispatchPhase::Capture || !dragging(&id) {
                            return;
                        }
                        if event.pressed_button != Some(MouseButton::Left) {
                            DRAGGING.with_borrow_mut(|d| *d = None);
                            if let Some(release) = &release_missed {
                                release(cx);
                            }
                            return;
                        }
                        set(level_at(event.position, bounds.get(), upright), cx);
                    });
                    let (up_id, release) = (paint_id.clone(), release.clone());
                    window.on_mouse_event(move |_: &MouseUpEvent, phase, _, cx| {
                        if phase == DispatchPhase::Capture && dragging(&up_id) {
                            DRAGGING.with_borrow_mut(|d| *d = None);
                            if let Some(release) = &release {
                                release(cx);
                            }
                        }
                    });
                },
            )
            .absolute()
            .top_0()
            .left_0()
            .size_full(),
        )
}

/// Which step of `min..=max` a slider's `level` lands on.
pub fn snapped(level: f32, min: f32, max: f32, step: f32) -> f32 {
    (min + (level * (max - min) / step).round() * step).clamp(min, max)
}

thread_local! {
    /// The step a held slider is on, not yet applied.
    static HELD_AT: std::cell::Cell<Option<f32>> = const { std::cell::Cell::new(None) };
}

/// [`ticked`], but `apply` only runs when the slider's let go, for a setting that would
/// move the slider itself out from under the pointer. Shows the held value until then.
pub fn ticked_on_release(id: impl Into<SharedString>, value: f32, (min, max, step): (f32, f32, f32), label: impl Fn(f32) -> String, colors: (Hsla, Hsla, Hsla), apply: Rc<dyn Fn(f32, &mut App)>) -> impl IntoElement {
    let (filled, track, text) = colors;
    let shown = HELD_AT.get().unwrap_or(value);
    let ticks = ((max - min) / step).round() as usize + 1;
    let level = ((shown - min) / (max - min)).clamp(0., 1.);
    let hold: Rc<dyn Fn(f32, &mut App)> = Rc::new(move |level, cx| {
        let at = snapped(level, min, max, step);
        if HELD_AT.get() != Some(at) {
            HELD_AT.set(Some(at));
            cx.refresh_windows();
        }
    });
    let release: Rc<dyn Fn(&mut App)> = Rc::new(move |cx| {
        if let Some(at) = HELD_AT.take() {
            apply(at, cx);
            cx.refresh_windows();
        }
    });
    div()
        .flex()
        .items_center()
        .gap(u(12.))
        .child(
            div()
                .flex()
                .flex_col()
                .w(u(260.))
                .gap(u(2.))
                .child(div().w_full().h(u(16.)).child(slider_with_release(id, level, false, filled, track, hold, Some(release))))
                .child(div().w_full().flex().justify_between().children((0..ticks).map(|_| div().w(u(1.)).h(u(5.)).bg(track)))),
        )
        .child(div().min_w(u(48.)).text_color(text).whitespace_nowrap().child(label(shown)))
}

/// A slider that lands only on steps, with a tick under each, and its value beside it.
pub fn ticked(id: impl Into<SharedString>, value: f32, (min, max, step): (f32, f32, f32), label: String, colors: (Hsla, Hsla, Hsla), set: Rc<dyn Fn(f32, &mut App)>) -> impl IntoElement {
    let (filled, track, text) = colors;
    let ticks = ((max - min) / step).round() as usize + 1;
    let level = ((value - min) / (max - min)).clamp(0., 1.);
    let snap: Rc<dyn Fn(f32, &mut App)> = Rc::new(move |level, cx| set(snapped(level, min, max, step), cx));
    div()
        .flex()
        .items_center()
        .gap(u(12.))
        .child(
            div()
                .flex()
                .flex_col()
                .w(u(260.))
                .gap(u(2.))
                .child(div().w_full().h(u(16.)).child(slider(id, level, false, filled, track, snap)))
                .child(div().w_full().flex().justify_between().children((0..ticks).map(|_| div().w(u(1.)).h(u(5.)).bg(track)))),
        )
        .child(div().min_w(u(48.)).text_color(text).whitespace_nowrap().child(label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui_kit::{point, px, size};

    #[test]
    fn it_lands_on_steps() {
        assert_eq!(snapped(0., 100., 160., 5.), 100.);
        assert_eq!(snapped(1., 100., 160., 5.), 160.);
        assert_eq!(snapped(0.5, 100., 160., 5.), 130.);
        assert_eq!(snapped(0.51, 100., 160., 5.), 130.);
        assert_eq!(snapped(0.55, 100., 160., 5.), 135.);
    }

    #[test]
    fn the_level_follows_the_pointer_and_stops_at_the_ends() {
        let track = Bounds::new(point(px(10.), px(20.)), size(px(100.), px(50.)));
        assert_eq!(level_at(point(px(60.), px(0.)), track, false), 0.5);
        assert_eq!(level_at(point(px(-500.), px(0.)), track, false), 0.);
        assert_eq!(level_at(point(px(500.), px(0.)), track, false), 1.);
        assert_eq!(level_at(point(px(0.), px(70.)), track, true), 0.);
        assert_eq!(level_at(point(px(0.), px(20.)), track, true), 1.);
    }
}
