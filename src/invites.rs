//! Invite cards under messages that link a `discord.gg/…` invite. What each
//! code leads to is fetched once per run and kept app-wide; the chat list
//! warms the codes for the rows it is about to draw, like GIFs.

use std::collections::HashMap;
use std::sync::Arc;

use cord_core::Core;
use cord_core::invite::{Invite, invite_codes};
use cord_core::model::Message;
use gpui_kit::component::ActiveTheme;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, FontWeight, Global, Hsla, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    StatefulInteractiveElement, Styled, StyledImage, div, img,
};

use crate::ui::u;

/// The card's fixed height, loaded or not, so the row never changes size.
pub const CARD_H: f32 = 118.;
const CARD_W: f32 = 432.;

#[derive(Clone)]
enum State {
    Loading,
    Ready(Arc<Invite>),
    Invalid,
}

#[derive(Default)]
struct Invites(HashMap<String, State>);

impl Global for Invites {}

/// Starts fetching the invites `message` links that aren't known yet.
pub fn warm(message: &Message, core: &Arc<Core>, cx: &mut App) {
    for code in invite_codes(&message.content) {
        if !cx.has_global::<Invites>() {
            cx.set_global(Invites::default());
        }
        if cx.global::<Invites>().0.contains_key(&code) {
            continue;
        }
        cx.global_mut::<Invites>().0.insert(code.clone(), State::Loading);
        let core = core.clone();
        cx.spawn(async move |cx| {
            let state = match core.invite(code.clone()).await {
                Ok(invite) => State::Ready(Arc::new(invite)),
                Err(_) => State::Invalid,
            };
            let _ = cx.update(|cx| {
                cx.global_mut::<Invites>().0.insert(code, state);
                crate::ui::redraw_chat(cx);
            });
        })
        .detach();
    }
}

/// One card per invite `message` links.
pub fn cards(message: &Message, joined: &dyn Fn(cord_core::Id) -> bool, cx: &App) -> Vec<AnyElement> {
    invite_codes(&message.content)
        .into_iter()
        .take(3)
        .map(|code| {
            let state = cx.try_global::<Invites>().and_then(|i| i.0.get(&code).cloned()).unwrap_or(State::Loading);
            card(message.id, &code, state, joined, cx)
        })
        .collect()
}

fn card(message: cord_core::Id, code: &str, state: State, joined: &dyn Fn(cord_core::Id) -> bool, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let (fg, muted) = (theme.foreground, theme.muted_foreground);
    let green = Hsla::from(gpui_kit::rgb(0x23a55a));
    let heading = match &state {
        State::Invalid => "You received an invite, but…",
        _ => "You've been invited to join a server",
    };
    let icon_box = div().size(u(50.)).flex_shrink_0().rounded(u(16.)).overflow_hidden().bg(fg.opacity(0.08));
    let (icon, title, detail, button): (AnyElement, AnyElement, Option<AnyElement>, Option<AnyElement>) = match state {
        State::Loading => (
            icon_box.into_any_element(),
            div().w(u(140.)).h(u(16.)).rounded_full().bg(fg.opacity(0.12)).into_any_element(),
            Some(div().w(u(100.)).h(u(12.)).rounded_full().bg(fg.opacity(0.08)).into_any_element()),
            None,
        ),
        State::Invalid => (
            icon_box.child(div().size_full().flex().items_center().justify_center().text_size(u(22.)).child("?")).into_any_element(),
            div().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.danger).child("Invalid Invite").into_any_element(),
            Some(div().text_size(u(14.)).text_color(muted).child("Try sending a new invite!").into_any_element()),
            None,
        ),
        State::Ready(invite) => {
            let guild = invite.guild.clone().unwrap_or_default();
            let initials: String = guild.name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect();
            let icon = match guild.icon_url(100) {
                Some(url) => icon_box.child(img(url).size_full().rounded(u(16.)).object_fit(ObjectFit::Cover)),
                None => icon_box.child(div().size_full().flex().items_center().justify_center().text_size(u(16.)).child(initials)),
            };
            let stat = |dot: Hsla, text: String| {
                div()
                    .flex()
                    .items_center()
                    .gap(u(4.))
                    .child(div().size(u(8.)).rounded_full().bg(dot))
                    .child(text)
            };
            let counts = div()
                .flex()
                .gap(u(12.))
                .text_size(u(14.))
                .text_color(muted)
                .when_some(invite.approximate_presence_count, |d, n| d.child(stat(green, format!("{} Online", group_digits(n)))))
                .when_some(invite.approximate_member_count, |d, n| d.child(stat(muted, format!("{} Members", group_digits(n)))));
            let inside = joined(guild.id);
            let id = guild.id;
            let url = format!("https://discord.com/invite/{}", invite.code);
            let button = div()
                .id(gpui_kit::SharedString::from(format!("invite-join-{message}-{}", invite.code)))
                .h(u(40.))
                .px(u(16.))
                .flex()
                .flex_shrink_0()
                .items_center()
                .rounded(u(8.))
                .bg(green)
                .text_color(gpui_kit::white())
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .hover(|d| d.opacity(0.9))
                .on_click(move |_, _, cx| {
                    // Joining from outside the official client asks for a captcha;
                    // a server we're already in just opens.
                    if inside {
                        crate::links::go(format!("/channels/{id}"), cx);
                    } else {
                        cx.open_url(&url);
                    }
                })
                .child(if inside { "Joined" } else { "Join" });
            (
                icon.into_any_element(),
                div().min_w_0().truncate().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).text_color(fg).child(guild.name).into_any_element(),
                Some(counts.into_any_element()),
                Some(button.into_any_element()),
            )
        }
    };
    div()
        .id(gpui_kit::SharedString::from(format!("invite-{message}-{code}")))
        .w_full()
        .max_w(u(CARD_W))
        .h(u(CARD_H))
        .p(u(16.))
        .flex()
        .flex_col()
        .gap(u(12.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .child(div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(muted).child(heading))
        .child(
            div()
                .flex()
                .items_center()
                .gap(u(16.))
                .child(icon)
                .child(div().flex_1().min_w_0().flex().flex_col().gap(u(4.)).child(title).children(detail))
                .children(button),
        )
        .into_any_element()
}

/// 12345 → "12,345".
fn group_digits(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    #[test]
    fn digits_are_grouped() {
        assert_eq!(super::group_digits(5), "5");
        assert_eq!(super::group_digits(1234), "1,234");
        assert_eq!(super::group_digits(1234567), "1,234,567");
    }
}
