//! The small panel for a call ringing us: a window inside the window, not one of
//! its own. It pops in, can be dragged anywhere by any part but its two buttons,
//! and pops out when the call's answered, declined, or stops ringing.

use std::time::Duration;

use cord_core::Id;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::{
    AnimationExt as _, AnyElement, App, Entity, FontWeight, Global, InteractiveElement as _, IntoElement as _, ParentElement as _, Pixels, Point,
    SharedString, StatefulInteractiveElement as _, Styled as _, Window, deferred, div, point, px,
};

use gpui_kit::assets::IconName;
use crate::store::Store;
use crate::ui::u;

const POP: Duration = Duration::from_millis(220);
/// The panel's size, in Discord's pixels.
const WIDTH: f32 = 232.;
const HEIGHT: f32 = 268.;

/// Who's calling, as last drawn: kept while the panel pops out.
#[derive(Clone)]
struct Caller {
    channel: Id,
    name: String,
    picture: String,
}

#[derive(Default)]
struct Ring {
    shown: Option<Caller>,
    closing: bool,
    /// Counts every pop, so each plays its own animation.
    number: usize,
    /// Where its top left corner is, once it's been dragged; centred near the top until then.
    at: Option<Point<Pixels>>,
    /// While dragging: where in the panel it was grabbed.
    grab: Option<Point<Pixels>>,
}

impl Global for Ring {}

/// Keeps a dragged panel on screen: at least `keep` of it stays inside the window.
pub fn clamp_to_window(at: Point<f32>, size: (f32, f32), window: (f32, f32), keep: f32) -> Point<f32> {
    let x = at.x.clamp(keep - size.0, (window.0 - keep).max(keep - size.0));
    let y = at.y.clamp(0., (window.1 - keep).max(0.));
    point(x, y)
}

/// Who's calling in `channel`: the group's name and picture, or the person's.
fn caller(store: &Store, channel: Id) -> Option<Caller> {
    let cache = store.cache();
    let place = cache.channel(channel)?;
    let name = crate::nav::channel_label(cache, place);
    let picture = match (&place.icon, place.kind) {
        (Some(hash), cord_core::model::ChannelKind::GroupDm) => format!("{}/channel-icons/{channel}/{hash}.png?size=128", cord_core::cdn::CDN),
        _ => {
            // In a group without a picture, whoever's in the call already.
            let someone = cache.call(channel).and_then(|c| c.participants().next()).or_else(|| place.recipient_ids.first().copied());
            someone.and_then(|id| cache.user(id)).map(|u| cache.avatar_url(None, u, 128)).unwrap_or_default()
        }
    };
    Some(Caller { channel, name, picture })
}

/// Follows what's ringing: pops a new call in, and the last one out.
fn follow(store: &Entity<Store>, cx: &mut App) {
    if !cx.has_global::<Ring>() {
        cx.set_global(Ring::default());
    }
    let ringing = store.read(cx).incoming_calls().first().copied();
    let now = ringing.and_then(|channel| caller(store.read(cx), channel));
    let ring = cx.global_mut::<Ring>();
    match (now, ring.shown.as_ref().map(|c| c.channel)) {
        (Some(caller), shown) if shown != Some(caller.channel) || ring.closing => {
            ring.number += 1;
            ring.closing = false;
            ring.shown = Some(caller);
        }
        // Names and pictures arrive late sometimes.
        (Some(caller), _) => ring.shown = Some(caller),
        (None, Some(_)) if !ring.closing => {
            ring.number += 1;
            ring.closing = true;
            let number = ring.number;
            cx.spawn(async move |cx| {
                cx.background_executor().timer(POP).await;
                cx.update(|cx| {
                    let ring = cx.global_mut::<Ring>();
                    if ring.number == number {
                        ring.shown = None;
                        ring.closing = false;
                        crate::ui::redraw_overlays(cx);
                    }
                });
            })
            .detach();
        }
        _ => {}
    }
}

/// Moves the panel along with the pointer while it's being dragged, wherever the pointer goes.
fn drag_watcher() -> impl gpui_kit::IntoElement {
    gpui_kit::canvas(
        |_, _, _| {},
        |_, _, window, _| {
            window.on_mouse_event(|event: &gpui_kit::MouseMoveEvent, phase, window, cx| {
                if phase != gpui_kit::DispatchPhase::Capture {
                    return;
                }
                let Some(ring) = cx.try_global::<Ring>() else { return };
                let Some(grab) = ring.grab else { return };
                if !event.dragging() {
                    cx.global_mut::<Ring>().grab = None;
                    return;
                }
                let scale = f32::from(window.rem_size()) / 16.;
                let viewport = window.viewport_size();
                let wanted = event.position - grab;
                let kept = clamp_to_window(
                    point(f32::from(wanted.x), f32::from(wanted.y)),
                    (WIDTH * scale, HEIGHT * scale),
                    (f32::from(viewport.width), f32::from(viewport.height)),
                    48. * scale,
                );
                cx.global_mut::<Ring>().at = Some(point(px(kept.x), px(kept.y)));
                crate::ui::redraw_overlays(cx);
            });
            window.on_mouse_event(|_: &gpui_kit::MouseUpEvent, _, _, cx| {
                if cx.try_global::<Ring>().is_some_and(|r| r.grab.is_some()) {
                    cx.global_mut::<Ring>().grab = None;
                }
            });
        },
    )
    .absolute()
    .size_0()
}

pub fn overlay(store: &Entity<Store>, window: &Window, cx: &mut App) -> Option<AnyElement> {
    follow(store, cx);
    let ring = cx.try_global::<Ring>()?;
    let caller = ring.shown.clone()?;
    let (closing, number) = (ring.closing, ring.number);
    let scale = f32::from(window.rem_size()) / 16.;
    let viewport = window.viewport_size();
    let at = ring.at.unwrap_or_else(|| point((viewport.width - px(WIDTH * scale)) / 2., viewport.height * 0.18));
    let theme = cx.theme();
    let (red, green) = (gpui_kit::Hsla::from(gpui_kit::rgb(0xda373c)), gpui_kit::Hsla::from(gpui_kit::rgb(0x248046)));
    let button = |id: &'static str, color: gpui_kit::Hsla, tip: &'static str, icon: Icon| {
        div()
            .id(id)
            .size(u(52.))
            .flex()
            .items_center()
            .justify_center()
            .rounded_full()
            .bg(color)
            .text_color(gpui_kit::white())
            .cursor_pointer()
            .hover(|d| d.opacity(0.85))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
            // Grabbing a button doesn't drag the panel.
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(icon.size(u(24.)))
    };
    let channel = caller.channel;
    let (accept_store, decline_store) = (store.clone(), store.clone());
    let card = v_flex()
        .id("call-ring")
        .occlude()
        .w(u(WIDTH))
        .h(u(HEIGHT))
        .pt(u(28.))
        .pb(u(24.))
        .items_center()
        .rounded(u(12.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .cursor_grab()
        .on_mouse_down(gpui_kit::MouseButton::Left, |event: &gpui_kit::MouseDownEvent, window, cx| {
            cx.stop_propagation();
            let scale = f32::from(window.rem_size()) / 16.;
            let viewport = window.viewport_size();
            let ring = cx.global_mut::<Ring>();
            let at = ring.at.unwrap_or_else(|| point((viewport.width - px(WIDTH * scale)) / 2., viewport.height * 0.18));
            ring.at = Some(at);
            ring.grab = Some(event.position - at);
        })
        .child(
            div()
                .relative()
                .size(u(80.))
                // A soft ring that breathes while it rings.
                .child(
                    div()
                        .absolute()
                        .top(u(-6.))
                        .left(u(-6.))
                        .size(u(92.))
                        .rounded_full()
                        .border_2()
                        .border_color(green)
                        .with_animation(("call-ring-pulse", number), gpui_kit::Animation::new(Duration::from_millis(1200)).repeat(), |d, t| {
                            d.opacity(0.9 * (1. - t))
                        }),
                )
                .child(Avatar::new().border_0().src(caller.picture.clone()).name(caller.name.clone()).size(u(80.))),
        )
        .child(
            div()
                .mt(u(16.))
                .px(u(16.))
                .max_w_full()
                .truncate()
                .text_size(u(18.))
                .font_weight(FontWeight::SEMIBOLD)
                .child(SharedString::from(caller.name.clone())),
        )
        .child(div().mt(u(4.)).text_size(u(14.)).text_color(theme.muted_foreground).child("Incoming Call…"))
        .child(div().flex_1())
        .child(
            h_flex()
                .gap(u(40.))
                .child(button("call-decline", red, "Decline", Icon::new(IconName::PhoneOff)).on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    decline_store.update(cx, |store, cx| store.decline_call(channel, cx));
                }))
                .child(button("call-accept", green, "Join Call", Icon::default().path("icons/phone.svg")).on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    accept_store.update(cx, |store, cx| store.accept_call(channel, cx));
                    crate::links::go(format!("/channels/@me/{channel}"), cx);
                })),
        )
        .with_animation(("call-ring-pop", number), gpui_kit::Animation::new(POP).with_easing(gpui_kit::ease_out_quint()), move |d, t| {
            // In: up from a little below and fading in; out: the reverse.
            let shown = if closing { 1. - t } else { t };
            d.opacity(shown).mt(u((1. - shown) * 16.))
        });
    Some(
        // Exactly where it was put: `anchored` flips it to another corner when part of it's out.
        deferred(div().absolute().left(at.x).top(at.y).child(drag_watcher()).child(card))
            .with_priority(5)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dragged_panel_stays_reachable() {
        let size = (200., 250.);
        let window = (1000., 800.);
        assert_eq!(clamp_to_window(point(100., 100.), size, window, 40.), point(100., 100.));
        assert_eq!(clamp_to_window(point(-500., -20.), size, window, 40.), point(-160., 0.), "some of it stays in from the left, none above");
        assert_eq!(clamp_to_window(point(5000., 5000.), size, window, 40.), point(960., 760.));
    }
}
