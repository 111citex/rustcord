//! A message on its own, away from its chat: in the pinned messages popout and in
//! search results. Its name, time and content as the chat draws them, with Jump.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::Message;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _, Styled as _, div};

use crate::markdown_view::{Env, ParseCache, Shared};
use crate::store::Store;
use crate::ui::u;

/// What the cards' content needs: spoilers, polls and reactions work as in the chat.
/// `channel_of` tells which channel each message shown is in.
pub fn shared(store: &Entity<Store>, channel_of: Rc<HashMap<Id, Id>>, parsed: Rc<RefCell<ParseCache>>, revealed: Rc<RefCell<std::collections::HashSet<(Id, u16)>>>, text_size: f32) -> Shared {
    let (react_store, vote_store, react_channels, vote_channels) = (store.clone(), store.clone(), channel_of.clone(), channel_of.clone());
    let reveal_into = revealed.clone();
    Shared {
        poll_ui: Rc::default(),
        vote: Rc::new(move |message, answers, cx| {
            if let Some(channel) = vote_channels.get(&message).copied() {
                vote_store.update(cx, |store, cx| store.vote_poll(channel, message, answers, cx));
            }
        }),
        revealed,
        reveal: Rc::new(move |message, group, cx| {
            reveal_into.borrow_mut().insert((message, group));
            crate::ui::redraw_chat(cx);
        }),
        // Only links inside a card jump; they open their channel.
        jump: Rc::new(|_, _| {}),
        react: Rc::new(move |message, emoji, on, cx| {
            if let Some(channel) = react_channels.get(&message).copied() {
                react_store.update(cx, |store, cx| store.set_reaction(channel, message, emoji, on, cx));
            }
        }),
        open_user: Rc::new(|user, position, _, cx| crate::links::open_profile(user, position, cx)),
        who_reacted: Rc::new(|_, _| {}),
        remeasure: Rc::new(|_, cx| crate::ui::redraw_chat(cx)),
        inert: false,
        emoji_cap: None,
        in_card: false,
        text_size,
        parsed,
    }
}

/// The address that opens `message` in its channel, scrolled to it.
pub fn link(message: &Message, guild: Option<Id>) -> String {
    let place = guild.map_or_else(|| "@me".to_string(), |g| g.to_string());
    format!("https://discord.com/channels/{place}/{}/{}", message.channel_id, message.id)
}

/// One message card. `extra` sits beside Jump at the top right while hovered (Unpin, say).
pub fn card(key: SharedString, message: &Message, guild: Option<Id>, cache: &Cache, shared: &Shared, extra: Option<AnyElement>, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let author = &message.author;
    let styled = cache.styled_user(author);
    let colors = guild.map(|g| cache.member_colors(g, author.id)).unwrap_or_default();
    let look = crate::badges::name_look(styled, &colors);
    let name = cache.display_name(guild, author).to_string();
    let group = SharedString::from(format!("{key}-group"));
    let url = link(message, guild);
    let jump = div()
        .id(SharedString::from(format!("{key}-jump")))
        .px(u(8.))
        .h(u(24.))
        .flex()
        .items_center()
        .rounded(u(4.))
        .bg(theme.background)
        .text_size(u(12.))
        .font_weight(FontWeight::MEDIUM)
        .cursor_pointer()
        .hover(|d| d.bg(theme.foreground.opacity(0.12)))
        .on_click(move |_, _, cx| {
            cx.stop_propagation();
            crate::links::open(&url, cx);
        })
        .child("Jump");
    v_flex()
        .id(key.clone())
        .group(group.clone())
        .relative()
        .w_full()
        .p(u(12.))
        .gap(u(2.))
        .rounded(u(8.))
        .bg(theme.foreground.opacity(0.04))
        .border_1()
        .border_color(theme.border)
        .hover(|d| d.bg(theme.foreground.opacity(0.07)))
        .child(
            h_flex()
                .gap(u(8.))
                .items_center()
                .child(Avatar::new().border_0().src(cache.avatar_url(guild, author, 64)).name(name.clone()).size(u(24.)))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(u(15.))
                        .font_weight(FontWeight::MEDIUM)
                        .when_some(look.color, |d, c| d.text_color(c))
                        .when_some(look.font, |d, f| d.font_family(f))
                        .child(SharedString::from(name)),
                )
                .child(div().flex_shrink_0().text_size(u(12.)).text_color(theme.muted_foreground).child(crate::nav::format_message_time(message.id.timestamp_ms(), &chrono::Local::now()))),
        )
        .child(div().pl(u(32.)).min_w_0().child(crate::rich_view::body(&Env { shared, cache, message, guild }, cx)))
        .child(
            h_flex()
                .absolute()
                .top(u(8.))
                .right(u(8.))
                .gap(u(4.))
                .opacity(0.)
                .group_hover(group, |d| d.opacity(1.))
                .children(extra)
                .child(jump),
        )
        .into_any_element()
}
