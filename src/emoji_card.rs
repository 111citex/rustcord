//! The card an emoji or sticker opens when clicked: what it is, whether we can use it,
//! and where it's from. Clicking it again or anywhere else closes it. Where something
//! comes from is asked of Discord only for what we can't tell ourselves, and once.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use cord_core::Id;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, Global, InteractiveElement as _, IntoElement, ObjectFit, ParentElement as _, Pixels, Point, SharedString,
    StatefulInteractiveElement as _, Styled as _, StyledImage as _, anchored, deferred, div, img,
};
use serde_json::Value;

use crate::store::Store;
use crate::ui::u;

/// A press that closed the card, on what opened it, shouldn't open it right back.
const SAME_PRESS: Duration = Duration::from_millis(300);

#[derive(Clone, Debug, PartialEq)]
pub enum Subject {
    /// One of the standard emoji, usable by everyone.
    Standard { name: String, picture: String },
    Emoji { id: Id, name: String, picture: String },
    Sticker { id: Id, name: String, picture: String },
}

impl Subject {
    fn key(&self) -> String {
        match self {
            Subject::Standard { name, .. } => format!("s:{name}"),
            Subject::Emoji { id, .. } => format!("e:{id}"),
            Subject::Sticker { id, .. } => format!("t:{id}"),
        }
    }

    fn noun(&self) -> &'static str {
        if matches!(self, Subject::Sticker { .. }) { "sticker" } else { "emoji" }
    }

    fn picture(&self) -> &str {
        match self {
            Subject::Standard { picture, .. } | Subject::Emoji { picture, .. } | Subject::Sticker { picture, .. } => picture,
        }
    }

    fn title(&self) -> String {
        match self {
            Subject::Standard { name, .. } | Subject::Emoji { name, .. } => format!(":{name}:"),
            Subject::Sticker { name, .. } => name.clone(),
        }
    }
}

/// Where something comes from, as far as anyone can say.
#[derive(Clone, Debug, PartialEq)]
pub enum Origin {
    Finding,
    /// A server we're in: this one, or another.
    Joined { here: bool, name: String, icon: Option<String>, invite_only: bool },
    Public { id: Id, name: String, icon: Option<String>, online: Option<u64> },
    Private,
}

/// What Discord says about where something's from. No server in it means it's private.
fn origin_from_reply(reply: &Value) -> Origin {
    let guild = if reply["guild"].is_object() { &reply["guild"] } else { reply };
    let (Some(id), Some(name)) = (guild["id"].as_str().and_then(|i| i.parse().ok()).map(Id), guild["name"].as_str()) else {
        return Origin::Private;
    };
    let icon = guild["icon"].as_str().map(|hash| cord_core::cdn::guild_icon_url(id, hash, 64));
    Origin::Public { id, name: name.to_string(), icon, online: guild["approximate_presence_count"].as_u64() }
}

/// What the card says, from what's known. `(sentence, offer Nitro)`.
fn sentence(subject: &Subject, origin: &Origin, nitro: bool) -> (String, bool) {
    let noun = subject.noun();
    if matches!(subject, Subject::Standard { .. }) {
        return ("A default emoji. You can use this emoji everywhere on Discord.".to_string(), false);
    }
    match origin {
        Origin::Finding => (String::new(), false),
        Origin::Joined { here: true, .. } if nitro => (format!("This {noun} is from this server. You can use it everywhere."), false),
        Origin::Joined { here: true, .. } => (format!("This {noun} is from this server. Get Nitro to use it everywhere."), true),
        Origin::Joined { here: false, .. } if nitro => (format!("This {noun} is from a server you're in. You can use it everywhere."), false),
        Origin::Joined { here: false, .. } => (format!("This {noun} is from a server you're in. Get Nitro to use it everywhere."), true),
        Origin::Public { .. } if nitro => (format!("Join the server to use this {noun} everywhere."), false),
        Origin::Public { .. } => (format!("Want to use this {noun} everywhere? Get Nitro and join the server."), true),
        Origin::Private => (format!("This {noun} is from a private server. You'd have to ask for an invitation."), false),
    }
}

struct Open {
    subject: Subject,
    position: Point<Pixels>,
    origin: Origin,
}

impl Global for Open {}

thread_local! {
    static STORE: RefCell<Option<Entity<Store>>> = const { RefCell::new(None) };
    static LAST_CLOSED: RefCell<Option<(String, Instant)>> = const { RefCell::new(None) };
    /// What Discord's said, so that it's only asked once.
    static KNOWN: RefCell<HashMap<String, Origin>> = RefCell::default();
}

/// Tells this where to find out about servers.
pub fn set_store(store: Entity<Store>) {
    STORE.with_borrow_mut(|s| *s = Some(store));
}

fn close(cx: &mut App) {
    let Some(open) = cx.try_global::<Open>() else { return };
    let key = open.subject.key();
    LAST_CLOSED.with_borrow_mut(|last| *last = Some((key, Instant::now())));
    cx.remove_global::<Open>();
    crate::ui::redraw_shell(cx);
}

/// What can be told without asking anyone.
fn known_origin(subject: &Subject, here: Option<Id>, store: &Entity<Store>, cx: &App) -> Option<Origin> {
    let cache = store.read(cx).cache();
    let joined = |guild: &cord_core::model::Guild| Origin::Joined {
        here: Some(guild.id) == here,
        name: guild.name.clone(),
        icon: guild.icon.as_ref().map(|hash| cord_core::cdn::guild_icon_url(guild.id, hash, 64)),
        invite_only: true,
    };
    match subject {
        Subject::Standard { .. } => Some(Origin::Private),
        Subject::Emoji { id, .. } => cache.guild_with_emoji(*id).map(joined),
        Subject::Sticker { .. } => None,
    }
}

/// Opens the card, or closes it if it's already showing this.
pub fn toggle(subject: Subject, here: Option<Id>, position: Point<Pixels>, cx: &mut App) {
    let key = subject.key();
    let just_closed = LAST_CLOSED.with_borrow(|last| last.as_ref().is_some_and(|(k, when)| *k == key && when.elapsed() < SAME_PRESS));
    if just_closed {
        return;
    }
    if cx.try_global::<Open>().is_some_and(|open| open.subject.key() == key) {
        close(cx);
        return;
    }
    let Some(store) = STORE.with_borrow(Clone::clone) else { return };
    let origin = known_origin(&subject, here, &store, cx).or_else(|| KNOWN.with_borrow(|k| k.get(&key).cloned()));
    let finding = origin.is_none();
    cx.set_global(Open { subject: subject.clone(), position, origin: origin.unwrap_or(Origin::Finding) });
    crate::ui::redraw_shell(cx);
    if !finding {
        return;
    }
    let core = store.read(cx).core().clone();
    cx.spawn(async move |cx| {
        let reply = match &subject {
            Subject::Emoji { id, .. } => core.emoji_source(*id).await,
            Subject::Sticker { id, .. } => core.sticker_source(*id).await,
            Subject::Standard { .. } => return,
        };
        let found = reply.map_or(Origin::Private, |reply| origin_from_reply(&reply));
        KNOWN.with_borrow_mut(|known| {
            known.insert(key.clone(), found.clone());
        });
        cx.update(|cx| {
            if cx.try_global::<Open>().is_some_and(|open| open.subject.key() == key) {
                cx.global_mut::<Open>().origin = found;
                crate::ui::redraw_shell(cx);
            }
        });
    })
    .detach();
}

/// `element`, opening the card for `subject` when clicked.
pub fn clickable(element: AnyElement, id: SharedString, subject: Subject, here: Option<Id>) -> AnyElement {
    let spot = crate::markdown_view::NameSpot::default();
    let target = spot.clone();
    crate::markdown_view::spotted(&spot, div()
        .id(id)
        .flex_shrink_0()
        .cursor_pointer()
        .on_click(move |event, _, cx| {
            cx.stop_propagation();
            toggle(subject.clone(), here, crate::markdown_view::card_at(&target, event.position()), cx);
        })
        .child(element))
        .into_any_element()
}

/// The card, if one's open.
pub fn overlay(cx: &App) -> Option<AnyElement> {
    let open = cx.try_global::<Open>()?;
    let theme = cx.theme();
    let nitro = STORE.with_borrow(|s| s.as_ref().is_some_and(|store| store.read(cx).cache().has_nitro()));
    let (text, offer) = sentence(&open.subject, &open.origin, nitro);
    let standard = matches!(open.subject, Subject::Standard { .. });
    let picture = img(open.subject.picture().to_string()).size(u(48.)).flex_shrink_0().object_fit(ObjectFit::Contain);
    let heading = match &open.origin {
        Origin::Public { .. } => "Get this emoji from",
        _ if matches!(open.subject, Subject::Sticker { .. }) => "This sticker is from",
        _ => "This emoji is from",
    };
    let server = match &open.origin {
        Origin::Joined { name, icon, invite_only, .. } => Some((name.clone(), icon.clone(), if *invite_only { "Invite-only server".to_string() } else { String::new() }, None)),
        Origin::Public { id, name, icon, online } => Some((name.clone(), icon.clone(), online.map_or_else(String::new, |n| format!("{n} Online")), Some(*id))),
        _ => None,
    };
    let card = v_flex()
        .id("emoji-card")
        .occlude()
        .w(u(300.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .on_mouse_down_out(|_, _, cx| close(cx))
        .child(
            v_flex()
                .p(u(16.))
                .gap(u(12.))
                .child(
                    h_flex()
                        .gap(u(12.))
                        .items_start()
                        .child(picture)
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(open.subject.title()))
                                .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child(text)),
                        ),
                )
                .when(offer, |d| {
                    d.child(
                        div()
                            .id("emoji-card-nitro")
                            .h(u(36.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(u(8.))
                            .bg(gpui_kit::white())
                            .text_color(gpui_kit::black())
                            .text_size(u(14.))
                            .font_weight(FontWeight::MEDIUM)
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.85))
                            .on_click(|_, _, cx| cx.open_url("https://discord.com/nitro"))
                            .child("Get Nitro"),
                    )
                }),
        )
        .when_some(server.filter(|_| !standard), |d, (name, icon, detail, join)| {
            d.child(
                v_flex()
                    .p(u(16.))
                    .gap(u(8.))
                    .border_t_1()
                    .border_color(theme.border)
                    .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child(heading))
                    .child(
                        h_flex()
                            .gap(u(12.))
                            .items_center()
                            .when_some(icon, |d, url| d.child(img(url).size(u(40.)).flex_shrink_0().rounded(u(10.))))
                            .child(
                                v_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(div().truncate().text_size(u(14.)).font_weight(FontWeight::SEMIBOLD).child(name))
                                    .child(
                                        h_flex()
                                            .gap(u(8.))
                                            .text_size(u(12.))
                                            .text_color(theme.muted_foreground)
                                            .child(detail)
                                            .when_some(join, |d, id| {
                                                d.child(
                                                    div()
                                                        .id("emoji-card-join")
                                                        .text_color(theme.link)
                                                        .cursor_pointer()
                                                        .hover(|d| d.underline())
                                                        .on_click(move |_, _, cx| cx.open_url(&format!("https://discord.com/servers/{id}")))
                                                        .child("Join Server"),
                                                )
                                            }),
                                    ),
                            ),
                    ),
            )
        });
    Some(deferred(anchored().position(open.position).snap_to_window_with_margin(gpui_kit::px(8.)).child(card)).with_priority(3).into_any_element())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn emoji() -> Subject {
        Subject::Emoji { id: Id(1), name: "a".into(), picture: String::new() }
    }

    fn joined(here: bool) -> Origin {
        Origin::Joined { here, name: "x".into(), icon: None, invite_only: true }
    }

    #[test]
    fn a_standard_emoji_is_for_everyone() {
        let standard = Subject::Standard { name: "sob".into(), picture: String::new() };
        let (text, offer) = sentence(&standard, &Origin::Private, false);
        assert!(text.contains("everywhere on Discord"));
        assert!(!offer);
    }

    #[test]
    fn nitro_is_only_offered_to_those_without_it() {
        assert!(sentence(&emoji(), &joined(true), false).1);
        assert!(!sentence(&emoji(), &joined(true), true).1);
        assert!(sentence(&emoji(), &joined(true), true).0.contains("You can use it everywhere"));
    }

    #[test]
    fn a_private_server_offers_nothing_but_says_so() {
        let (text, offer) = sentence(&emoji(), &Origin::Private, false);
        assert!(text.contains("private server") && text.contains("invitation"));
        assert!(!offer);
    }

    #[test]
    fn stickers_say_sticker() {
        let sticker = Subject::Sticker { id: Id(2), name: "s".into(), picture: String::new() };
        assert!(sentence(&sticker, &joined(false), false).0.contains("This sticker"));
    }

    #[test]
    fn replies_are_read_with_or_without_a_wrapper() {
        let wrapped = serde_json::json!({"type": "GUILD", "guild": {"id": "5", "name": "cats", "approximate_presence_count": 9}});
        assert_eq!(origin_from_reply(&wrapped), Origin::Public { id: Id(5), name: "cats".into(), icon: None, online: Some(9) });
        let bare = serde_json::json!({"id": "6", "name": "dogs"});
        assert!(matches!(origin_from_reply(&bare), Origin::Public { id: Id(6), .. }));
        assert_eq!(origin_from_reply(&serde_json::json!({"type": "GUILD"})), Origin::Private);
    }
}
