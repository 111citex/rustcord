//! The card beside a live person in the channel list: their stream's thumbnail, and a
//! button to watch it. It stays while the pointer goes from the row onto it, and
//! goes a moment after the pointer's left both, so it never blinks out on the way.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use cord_core::Id;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement as _, ObjectFit, ParentElement as _, Pixels, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, Window, anchored, deferred, div, img, point,
};

use crate::store::Store;
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::AnimationExt as _;
use crate::ui::u;

/// How long the card waits after the pointer's gone before it goes.
const LINGER: Duration = Duration::from_millis(250);
/// A thumbnail's asked for again after this.
const FRESH_FOR: Duration = Duration::from_secs(60);
const WIDTH: f32 = 300.;

struct Hover {
    guild: Option<Id>,
    channel: Id,
    user: Id,
    name: String,
    /// Where the card goes: its top left.
    at: gpui_kit::Point<Pixels>,
    on_row: bool,
    on_card: bool,
    left_at: Option<Instant>,
    /// Counts each time it comes up, so each plays its own animation.
    number: u64,
}

thread_local! {
    static HOVER: RefCell<Option<Hover>> = const { RefCell::new(None) };
    /// Thumbnails by stream key: the address, when it was asked for, whether it's on its way.
    static PREVIEWS: RefCell<HashMap<String, (Option<String>, Instant)>> = RefCell::default();
}

/// The stream key, as Discord writes it.
pub fn stream_key(guild: Option<Id>, channel: Id, user: Id) -> String {
    match guild {
        Some(guild) => format!("guild:{guild}:{channel}:{user}"),
        None => format!("call:{channel}:{user}"),
    }
}

/// The thumbnail's address, asking for it when it's missing or old.
pub fn preview(store: &Entity<Store>, key: &str, cx: &App) -> Option<String> {
    let known = PREVIEWS.with_borrow(|p| p.get(key).cloned());
    let stale = known.as_ref().is_none_or(|(_, at)| at.elapsed() > FRESH_FOR);
    if stale {
        PREVIEWS.with_borrow_mut(|p| {
            let url = known.as_ref().and_then(|(u, _)| u.clone());
            p.insert(key.to_string(), (url, Instant::now()));
        });
        let core = store.read(cx).core().clone();
        let key = key.to_string();
        cx.spawn(async move |cx| {
            let Ok(url) = core.stream_preview(key.clone()).await else { return };
            PREVIEWS.with_borrow_mut(|p| p.insert(key, (url, Instant::now())));
            cx.update(crate::ui::redraw_overlays);
        })
        .detach();
    }
    known.and_then(|(url, _)| url)
}

/// The pointer came onto, or left, a live person's row.
pub fn hover_row(on: bool, guild: Option<Id>, channel: Id, user: Id, name: String, at: gpui_kit::Point<Pixels>, cx: &mut App) {
    HOVER.with_borrow_mut(|hover| {
        if on {
            let same = hover.as_ref().filter(|h| h.user == user);
            let card = same.is_some_and(|h| h.on_card);
            // Still showing (or only just going): the same card stays, no new entrance.
            let showing = same.is_some_and(|h| h.on_row || h.on_card || h.left_at.is_some_and(|t| t.elapsed() < LINGER));
            let number = hover.as_ref().map_or(0, |h| h.number) + u64::from(!showing);
            *hover = Some(Hover { guild, channel, user, name, at, on_row: true, on_card: card, left_at: None, number });
        } else if let Some(h) = hover.as_mut().filter(|h| h.user == user) {
            h.on_row = false;
            h.left_at = Some(Instant::now());
        }
    });
    if !on {
        linger(cx);
    }
    crate::ui::redraw_overlays(cx);
}

fn hover_card(on: bool, cx: &mut App) {
    HOVER.with_borrow_mut(|hover| {
        if let Some(h) = hover.as_mut() {
            h.on_card = on;
            if on {
                h.left_at = None;
            } else {
                h.left_at = Some(Instant::now());
            }
        }
    });
    if !on {
        linger(cx);
    }
    crate::ui::redraw_overlays(cx);
}

/// Looks again once the wait's over, so the card goes then.
fn linger(cx: &mut App) {
    cx.spawn(async move |cx| {
        cx.background_executor().timer(LINGER + Duration::from_millis(20)).await;
        cx.update(crate::ui::redraw_overlays);
    })
    .detach();
}

/// The card, while the pointer's on the row or the card, or only just left.
pub fn overlay(store: &Entity<Store>, _window: &Window, cx: &App) -> Option<AnyElement> {
    let (guild, channel, user, name, at, number, leaving) = HOVER.with_borrow(|hover| {
        let h = hover.as_ref()?;
        let leaving = !h.on_row && !h.on_card;
        let showing = !leaving || h.left_at.is_some_and(|t| t.elapsed() < LINGER);
        showing.then(|| (h.guild, h.channel, h.user, h.name.clone(), h.at, h.number, leaving))
    })?;
    let key = stream_key(guild, channel, user);
    let url = preview(store, &key, cx);
    let theme = cx.theme();
    let watching = store.read(cx).watching() == Some(user);
    let watch = store.clone();
    let card = v_flex()
        .id("stream-card")
        .occlude()
        .w(u(WIDTH))
        .p(u(12.))
        .gap(u(10.))
        .rounded(u(10.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .on_hover(|on, _, cx| hover_card(*on, cx))
        .child(
            h_flex()
                .gap(u(8.))
                .items_center()
                .child(div().px(u(5.)).rounded(u(3.)).bg(theme.danger).text_color(theme.primary_foreground).text_size(u(10.)).font_weight(FontWeight::BOLD).child("LIVE"))
                .child(div().min_w_0().truncate().font_weight(FontWeight::SEMIBOLD).child(SharedString::from(name))),
        )
        .child(
            div()
                .relative()
                .w_full()
                .h(u(WIDTH * 9. / 16. - 13.))
                .rounded(u(6.))
                .overflow_hidden()
                .bg(gpui_kit::black())
                .flex()
                .items_center()
                .justify_center()
                .map(|d| match url {
                    Some(url) => d.child(img(url).absolute().top_0().left_0().size_full().object_fit(ObjectFit::Cover)),
                    None => d.text_size(u(13.)).text_color(theme.muted_foreground).child("Getting a preview\u{2026}"),
                }),
        )
        .child(
            div()
                .id("stream-card-watch")
                .h(u(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(6.))
                .bg(gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)))
                .text_color(gpui_kit::white())
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .hover(|d| d.opacity(0.9))
                .when(watching, |d| d.opacity(0.5).cursor_default())
                .on_click(move |_, _, cx| {
                    if !watching {
                        watch.update(cx, |store, cx| store.watch_stream_in(guild, channel, user, cx));
                    }
                })
                .child(if watching { "Watching" } else { "Watch Stream" }),
        )
        // In: fading up from a little below. Out, while it lingers: fading away.
        .with_animation(
            ("stream-card-motion", number * 2 + u64::from(leaving)),
            gpui_kit::Animation::new(if leaving { LINGER } else { Duration::from_millis(160) }).with_easing(gpui_kit::ease_out_quint()),
            move |card, t| {
                let shown = if leaving { 1. - t } else { t };
                card.opacity(shown).mt(u((1. - shown) * 8.))
            },
        );
    Some(deferred(anchored().position(point(at.x, at.y)).snap_to_window_with_margin(gpui_kit::px(8.)).child(card)).with_priority(3).into_any_element())
}
