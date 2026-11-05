//! The "are you sure?" over the whole window, for what acts on other people: it can't
//! be taken back quietly.

use std::rc::Rc;
use std::time::Duration;

use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::{AnimationExt as _, AnyElement, App, FontWeight, Global, InteractiveElement as _, IntoElement as _, ParentElement as _, StatefulInteractiveElement as _, Styled as _, Window, anchored, deferred, div};

use crate::ui::u;

const FADE: Duration = Duration::from_millis(180);

struct Asking {
    title: String,
    body: String,
    action_label: &'static str,
    action: Rc<dyn Fn(&mut App)>,
    closing: bool,
    number: usize,
}

impl Global for Asking {}

/// Asks, and does `action` on a yes.
pub fn ask(title: String, body: String, action_label: &'static str, action: impl Fn(&mut App) + 'static, cx: &mut App) {
    static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let number = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    cx.set_global(Asking { title, body, action_label, action: Rc::new(action), closing: false, number });
    cx.refresh_windows();
}

fn close(cx: &mut App) {
    let Some(asking) = cx.try_global::<Asking>().filter(|a| !a.closing) else { return };
    let number = asking.number;
    cx.global_mut::<Asking>().closing = true;
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(FADE).await;
        cx.update(|cx| {
            if cx.try_global::<Asking>().is_some_and(|a| a.number == number) {
                cx.remove_global::<Asking>();
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

/// The question, if there is one.
pub fn is_open(cx: &App) -> bool {
    cx.has_global::<Asking>()
}

pub fn overlay(window: &Window, cx: &App) -> Option<AnyElement> {
    let asking = cx.try_global::<Asking>()?;
    let theme = cx.theme();
    let (viewport, closing, number) = (window.viewport_size(), asking.closing, asking.number);
    let action = asking.action.clone();
    let button = |id: &'static str, label: &'static str, bg: gpui_kit::Hsla, fg: gpui_kit::Hsla| {
        div()
            .id(id)
            .h(u(36.))
            .px(u(16.))
            .flex()
            .items_center()
            .rounded(u(8.))
            .bg(bg)
            .text_color(fg)
            .text_size(u(14.))
            .font_weight(FontWeight::MEDIUM)
            .cursor_pointer()
            .hover(|d| d.opacity(0.85))
            .child(label)
    };
    let card = v_flex()
        .id("confirm-card")
        .relative()
        .occlude()
        .w(u(420.))
        .max_w_full()
        .p(u(20.))
        .gap(u(16.))
        .rounded(u(12.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .on_click(|_, _, cx| cx.stop_propagation())
        .child(div().text_size(u(20.)).font_weight(FontWeight::SEMIBOLD).child(asking.title.clone()))
        .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child(asking.body.clone()))
        .child(
            h_flex()
                .justify_end()
                .gap(u(8.))
                .child(button("confirm-cancel", "Cancel", theme.foreground.opacity(0.08), theme.foreground).on_click(|_, _, cx| {
                    cx.stop_propagation();
                    close(cx);
                }))
                .child(button("confirm-yes", asking.action_label, theme.danger, gpui_kit::white()).on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    close(cx);
                    action(cx);
                })),
        )
        .with_animation(("confirm-card", number * 2 + closing as usize), gpui_kit::Animation::new(FADE).with_easing(gpui_kit::ease_out_quint()), move |d, t| d.top(u(if closing { t * 8. } else { (1. - t) * 12. })));
    let backdrop = div()
        .id("confirm-backdrop")
        .occlude()
        .w(viewport.width)
        .h(viewport.height)
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::black().opacity(0.6))
        .on_click(|_, _, cx| close(cx))
        .child(card)
        .with_animation(("confirm-fade", number * 2 + closing as usize), gpui_kit::Animation::new(FADE).with_easing(gpui_kit::ease_out_quint()), move |d, t| d.opacity(if closing { 1. - t } else { t }));
    Some(deferred(anchored().position(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.))).child(backdrop)).with_priority(6).into_any_element())
}
