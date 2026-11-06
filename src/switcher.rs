//! "Find or start a conversation": Discord's quick switcher, on Ctrl+K or the pill
//! atop the DM list. Empty, it offers previous channels, drafts and mentions; typed
//! into, it finds conversations, people, channels and servers by name. `@`, `#`,
//! `!` and `*` first narrow it to people, text channels, voice channels or servers.

use cord_core::Id;
use cord_core::cache::Cache;
use cord_core::model::ChannelKind;
use gpui_kit::assets::IconName;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, anchored, deferred, div, point, px,
};

use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::store::Store;
use crate::transition::Showing;
use crate::ui::u;

/// Where a row goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Channel(Id),
    /// Someone with no DM open yet.
    User(Id),
    Guild(Id),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    Person,
    Group,
    Text,
    Voice,
    Server,
}

#[derive(Clone, Debug)]
struct Item {
    target: Target,
    label: String,
    detail: String,
    picture: Option<String>,
    look: Look,
    mentions: u32,
    /// The heading it sits under, when the box is empty.
    section: &'static str,
    /// A server's channel: the server's picture, ahead of the channel's sign.
    server: Option<crate::panels::Picture>,
}

/// What a leading `@`, `#`, `!` or `*` narrows the search to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Narrow {
    Everything,
    People,
    Text,
    Voice,
    Servers,
}

pub fn narrow(query: &str) -> (Narrow, &str) {
    let mut chars = query.chars();
    let mode = match chars.next() {
        Some('@') => Narrow::People,
        Some('#') => Narrow::Text,
        Some('!') => Narrow::Voice,
        Some('*') => Narrow::Servers,
        _ => return (Narrow::Everything, query.trim()),
    };
    (mode, chars.as_str().trim())
}

/// How well `name` matches `query`, best first: the same, starting with it, a word
/// starting with it, holding it, or holding its letters in order. `None`: not at all.
pub fn score(name: &str, query: &str) -> Option<u8> {
    let (name, query) = (name.to_lowercase(), query.to_lowercase());
    if query.is_empty() {
        return Some(5);
    }
    if name == query {
        return Some(0);
    }
    if name.starts_with(&query) {
        return Some(1);
    }
    if name.split(|c: char| !c.is_alphanumeric()).any(|word| word.starts_with(&query)) {
        return Some(2);
    }
    if name.contains(&query) {
        return Some(3);
    }
    let mut letters = name.chars();
    query.chars().all(|q| letters.any(|n| n == q)).then_some(4)
}

fn private_item(cache: &Cache, channel: &cord_core::model::Channel, section: &'static str) -> Item {
    let group = channel.kind == ChannelKind::GroupDm;
    let picture = match (&channel.icon, group) {
        (Some(hash), true) => Some(format!("{}/channel-icons/{}/{hash}.png?size=64", cord_core::cdn::CDN, channel.id)),
        _ => channel.recipient_ids.first().and_then(|u| cache.user(*u)).map(|u| cache.avatar_url(None, u, 64)),
    };
    let detail = match (group, channel.recipient_ids.first().and_then(|u| cache.user(*u))) {
        (true, _) => format!("{} Members", channel.recipient_ids.len() + 1),
        (false, Some(user)) => user.username.clone(),
        (false, None) => String::new(),
    };
    Item {
        target: Target::Channel(channel.id),
        label: crate::nav::channel_label(cache, channel),
        detail,
        picture,
        look: if group { Look::Group } else { Look::Person },
        mentions: cache.mentions(channel.id),
        section,
        server: None,
    }
}

fn channel_item(cache: &Cache, channel: Id, section: &'static str) -> Option<Item> {
    let place = cache.channel(channel)?;
    let Some(guild) = place.guild_id else { return Some(private_item(cache, place, section)) };
    let server = cache.guild(guild)?;
    Some(Item {
        target: Target::Channel(channel),
        label: place.name.clone().unwrap_or_default(),
        detail: server.name.clone(),
        picture: None,
        look: if matches!(place.kind, ChannelKind::Voice | ChannelKind::Stage) { Look::Voice } else { Look::Text },
        mentions: cache.mentions(channel),
        section,
        server: Some(crate::panels::Picture::of_server(server)),
    })
}

/// What the box offers with nothing typed.
fn suggestions(cache: &Cache, recent: &[Id], drafts: &[(Id, String)], here: Option<Id>) -> Vec<Item> {
    let mut items: Vec<Item> = recent.iter().filter(|c| Some(**c) != here).filter_map(|c| channel_item(cache, *c, "Previous Channels")).take(8).collect();
    items.extend(drafts.iter().filter_map(|(c, text)| {
        let mut item = channel_item(cache, *c, "Drafts")?;
        item.detail = text.lines().next().unwrap_or("").chars().take(60).collect();
        Some(item)
    }));
    let mut pinged: Vec<Item> = cache
        .private_channels()
        .into_iter()
        .map(|c| c.id)
        .chain(cache.guilds().flat_map(|g| cache.guild_channels(g.id).into_iter().map(|c| c.id)))
        .filter(|c| cache.mentions(*c) > 0)
        .filter_map(|c| channel_item(cache, c, "Mentions"))
        .collect();
    pinged.sort_by(|a, b| b.mentions.cmp(&a.mentions));
    items.extend(pinged);
    items
}

/// Everything matching `query`, best match first.
fn matches(cache: &Cache, query: &str, recent: &[Id]) -> Vec<Item> {
    let (mode, query) = narrow(query);
    let wants = |look: Look| match mode {
        Narrow::Everything => true,
        Narrow::People => matches!(look, Look::Person | Look::Group),
        Narrow::Text => look == Look::Text,
        Narrow::Voice => look == Look::Voice,
        Narrow::Servers => look == Look::Server,
    };
    let mut found: Vec<(u8, usize, Item)> = Vec::new();
    let recency = |target: Target| match target {
        Target::Channel(c) => recent.iter().position(|r| *r == c).unwrap_or(usize::MAX),
        _ => usize::MAX,
    };
    let mut add = |item: Item, names: &[&str]| {
        if !wants(item.look) {
            return;
        }
        if let Some(best) = names.iter().filter_map(|n| score(n, query)).min() {
            found.push((best, recency(item.target), item));
        }
    };
    let mut with_dm = std::collections::HashSet::new();
    for channel in cache.private_channels() {
        let item = private_item(cache, channel, "");
        let user = (channel.kind == ChannelKind::Dm).then(|| channel.recipient_ids.first().and_then(|u| cache.user(*u))).flatten();
        if let Some(user) = user {
            with_dm.insert(user.id);
            add(item, &[&cache.display_name(None, user).to_string(), &user.username]);
        } else {
            let label = item.label.clone();
            add(item, &[&label]);
        }
    }
    for friend in cache.friend_ids().into_iter().filter(|f| !with_dm.contains(f)) {
        let Some(user) = cache.user(friend) else { continue };
        let item = Item {
            target: Target::User(friend),
            label: cache.display_name(None, user).to_string(),
            detail: user.username.clone(),
            picture: Some(cache.avatar_url(None, user, 64)),
            look: Look::Person,
            mentions: 0,
            section: "",
            server: None,
        };
        add(item, &[&cache.display_name(None, user).to_string(), &user.username]);
    }
    for guild in cache.guilds() {
        let icon = guild.icon.as_ref().map(|hash| format!("{}/icons/{}/{hash}.png?size=64", cord_core::cdn::CDN, guild.id));
        add(Item { target: Target::Guild(guild.id), label: guild.name.clone(), detail: "Server".into(), picture: icon, look: Look::Server, mentions: cache.guild_mentions(guild.id), section: "", server: None }, &[&guild.name]);
        if mode == Narrow::Servers || mode == Narrow::People {
            continue;
        }
        for channel in cache.guild_channels(guild.id) {
            let wanted = matches!(channel.kind, ChannelKind::Text | ChannelKind::Announcement | ChannelKind::Voice | ChannelKind::Stage);
            if !wanted || !cache.can_view(channel.id) {
                continue;
            }
            if let Some(item) = channel_item(cache, channel.id, "") {
                let name = item.label.clone();
                add(item, &[&name]);
            }
        }
    }
    found.sort_by(|a, b| (a.0, a.1).cmp(&(b.0, b.1)).then_with(|| a.2.label.len().cmp(&b.2.label.len())));
    found.into_iter().take(40).map(|(_, _, item)| item).collect()
}

pub struct Switcher {
    store: Entity<Store>,
    input: Entity<InputState>,
    items: Vec<Item>,
    selected: usize,
    /// What the list was last built for.
    built_for: Option<String>,
    scroll: ScrollHandle,
    scroller: SmoothScroller,
    here: Option<Id>,
    _watching: Vec<Subscription>,
}

struct Open(Entity<Switcher>, Showing);
impl Global for Open {}

/// Open, and not on its way out.
pub fn is_open(cx: &App) -> bool {
    cx.try_global::<Open>().is_some_and(|open| !open.1.closing)
}

pub fn close(cx: &mut App) {
    crate::transition::leave::<Open>(cx, |open| &mut open.1);
}

/// Opens it, or closes it if it's open: Ctrl+K toggles, as in Discord.
pub fn toggle(store: Entity<Store>, here: Option<Id>, window: &mut Window, cx: &mut App) {
    if is_open(cx) {
        close(cx);
        return;
    }
    let switcher = cx.new(|cx| {
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Where would you like to go?"));
        input.update(cx, |input, cx| input.focus(window, cx));
        let typed = cx.subscribe(&input, |this: &mut Switcher, _, event: &InputEvent, cx| match event {
            InputEvent::PressEnter { .. } => this.go(this.selected, cx),
            _ => cx.notify(),
        });
        let keys = cx.intercept_keystrokes(|event, _, cx| {
            let Some(open) = cx.try_global::<Open>().filter(|o| !o.1.closing).map(|o| o.0.clone()) else { return };
            let key = event.keystroke.key.as_str();
            match key {
                "escape" => close(cx),
                "up" | "down" => open.update(cx, |this, cx| this.step(if key == "up" { -1 } else { 1 }, cx)),
                "tab" => open.update(cx, |this, cx| this.step(if event.keystroke.modifiers.shift { -1 } else { 1 }, cx)),
                _ => return,
            }
            cx.stop_propagation();
        });
        let scroll = ScrollHandle::new();
        let scroller = SmoothScroller::new(ScrollTarget::Handle(scroll.clone())).owned_by("switcher");
        Switcher { store, input, items: Vec::new(), selected: 0, built_for: None, scroll, scroller, here, _watching: vec![typed, keys] }
    });
    cx.set_global(Open(switcher, Showing::new()));
    cx.refresh_windows();
}

impl Switcher {
    fn step(&mut self, by: i32, cx: &mut Context<Self>) {
        if self.items.is_empty() {
            return;
        }
        let count = self.items.len() as i32;
        self.selected = (self.selected as i32 + by).rem_euclid(count) as usize;
        // The keys put it somewhere: a glide under way would carry it off again.
        self.scroller.stop();
        self.scroll.scroll_to_item(self.row_of(self.selected));
        cx.notify();
    }

    fn row_of(&self, ix: usize) -> usize {
        row_index(&self.items, ix)
    }

    fn go(&mut self, ix: usize, cx: &mut Context<Self>) {
        let Some(item) = self.items.get(ix).cloned() else { return };
        close(cx);
        match item.target {
            Target::Channel(channel) => {
                if let Some(place) = self.store.read(cx).cache().channel(channel) {
                    crate::links::go(crate::nav::channel_path(place), cx);
                }
            }
            Target::User(user) => self.store.update(cx, |store, cx| store.message_user(user, cx)),
            Target::Guild(guild) => {
                let cache = self.store.read(cx).cache();
                let channel = crate::places::get().guilds.get(&guild).copied().filter(|c| cache.channel(*c).is_some()).or_else(|| crate::nav::first_text_channel(cache, guild));
                let path = channel.map_or_else(|| format!("/channels/{guild}"), |c| format!("/channels/{guild}/{c}"));
                crate::links::go(path, cx);
            }
        }
    }
}

pub fn overlay(window: &Window, cx: &App) -> Option<AnyElement> {
    let open = cx.try_global::<Open>()?;
    let (switcher, showing) = (open.0.clone(), open.1);
    let viewport = window.viewport_size();
    let switcher = crate::transition::pop(div().relative().max_w_full().child(switcher), "switcher-card", showing);
    let backdrop = div()
        .id("switcher-backdrop")
        .occlude()
        .w(viewport.width)
        .h(viewport.height)
        .flex()
        .justify_center()
        .pt(viewport.height * 0.18)
        .bg(gpui_kit::black().opacity(0.6))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| close(cx))
        .child(switcher);
    let backdrop = crate::transition::fade(backdrop, "switcher-backdrop", showing);
    Some(deferred(anchored().position(point(px(0.), px(0.))).child(backdrop)).with_priority(4).into_any_element())
}

/// The scroll child holding item `ix`: headings are children of their own.
fn row_index(items: &[Item], ix: usize) -> usize {
    let headings = items.iter().take(ix + 1).enumerate().filter(|(i, item)| !item.section.is_empty() && (*i == 0 || items[i - 1].section != item.section)).count();
    ix + headings
}

fn look_icon(look: Look) -> IconName {
    match look {
        Look::Text => IconName::Hash,
        Look::Voice => IconName::Volume2,
        Look::Group => IconName::Users,
        Look::Person => IconName::AtSign,
        Look::Server => IconName::Compass,
    }
}

impl Render for Switcher {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let text = self.input.read(cx).value().to_string();
        if self.built_for.as_deref() != Some(text.as_str()) {
            let store = self.store.read(cx);
            let recent = crate::places::get().recent;
            self.items = if text.trim().is_empty() { suggestions(store.cache(), &recent, &crate::drafts::waiting(), self.here) } else { matches(store.cache(), &text, &recent) };
            self.selected = 0;
            self.built_for = Some(text.clone());
            self.scroller.stop();
            self.scroll.scroll_to_item(0);
        }
        let theme = cx.theme();
        let mut rows: Vec<AnyElement> = Vec::new();
        let mut last_section = "";
        for (ix, item) in self.items.iter().enumerate() {
            if !item.section.is_empty() && item.section != last_section {
                last_section = item.section;
                rows.push(div().px(u(12.)).pt(u(12.)).pb(u(4.)).text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(theme.muted_foreground).child(item.section.to_uppercase()).into_any_element());
            }
            let selected = ix == self.selected;
            let muted = theme.muted_foreground;
            let picture: AnyElement = match (&item.server, &item.picture) {
                // A server's channel: its server, a small chevron, then the channel's sign.
                (Some(server), _) => h_flex()
                    .gap(u(4.))
                    .items_center()
                    .child(server.view(20., theme.popover, cx))
                    .child(Icon::new(IconName::ChevronRight).size(u(12.)).text_color(muted))
                    .child(Icon::new(look_icon(item.look)).size(u(18.)).text_color(muted))
                    .into_any_element(),
                (None, Some(url)) => Avatar::new().border_0().src(url.clone()).name(item.label.clone()).size(u(20.)).into_any_element(),
                (None, None) => Icon::new(look_icon(item.look)).size(u(18.)).text_color(muted).into_any_element(),
            };
            rows.push(
                h_flex()
                    .id(("switcher-row", ix))
                    .h(u(36.))
                    .px(u(12.))
                    .gap(u(10.))
                    .items_center()
                    .rounded(u(4.))
                    .cursor_pointer()
                    .when(selected, |d| d.bg(theme.foreground.opacity(0.1)))
                    .on_hover(cx.listener(move |this, hovered: &bool, _, cx| {
                        if *hovered && this.selected != ix {
                            this.selected = ix;
                            cx.notify();
                        }
                    }))
                    .on_click(cx.listener(move |this, _, _, cx| this.go(ix, cx)))
                    .child(div().h(u(20.)).min_w(u(20.)).flex_shrink_0().flex().items_center().justify_center().child(picture))
                    .child(div().flex_shrink_0().max_w(u(300.)).truncate().text_size(u(16.)).font_weight(FontWeight::MEDIUM).child(SharedString::from(item.label.clone())))
                    .child(div().flex_1().min_w_0().truncate().text_size(u(13.)).text_color(theme.muted_foreground).child(SharedString::from(item.detail.clone())))
                    .when(item.mentions > 0, |d| {
                        d.child(
                            div()
                                .flex_shrink_0()
                                .px(u(6.))
                                .h(u(16.))
                                .rounded_full()
                                .flex()
                                .items_center()
                                .bg(theme.danger)
                                .text_color(gpui_kit::white())
                                .text_size(u(12.))
                                .font_weight(FontWeight::BOLD)
                                .child(item.mentions.to_string()),
                        )
                    })
                    .into_any_element(),
            );
        }
        let empty = rows.is_empty();
        v_flex()
            .id("switcher")
            .occlude()
            .w(u(570.))
            .max_w_full()
            .max_h(u(480.))
            .p(u(16.))
            .gap(u(8.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(div().h(u(56.)).px(u(8.)).flex().items_center().rounded(u(6.)).bg(theme.background).text_size(u(20.)).child(Input::new(&self.input).appearance(false).large()))
            .child(
                div().relative().flex_1().min_h_0().flex().flex_col().child(self.scroller.layer()).child(
                    div()
                        .id("switcher-list")
                        .flex_1()
                        .min_h_0()
                        .overflow_y_scroll()
                        .track_scroll(&self.scroll)
                        .when(empty, |d| d.child(div().py(u(24.)).text_center().text_size(u(14.)).text_color(theme.muted_foreground).child(if text.trim().is_empty() { "Nowhere to suggest yet." } else { "No results found." })))
                        .children(rows),
                ),
            )
            .child(
                div()
                    .pt(u(4.))
                    .text_size(u(12.))
                    .text_color(theme.muted_foreground)
                    .child("PROTIP: Start searches with @ # ! * to narrow down results."),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scores_closer_matches_higher() {
        assert_eq!(score("general", "general"), Some(0));
        assert_eq!(score("general", "gen"), Some(1));
        assert_eq!(score("off-topic", "top"), Some(2));
        assert_eq!(score("offtopic", "top"), Some(3));
        assert_eq!(score("general", "gnrl"), Some(4));
        assert_eq!(score("general", "xyz"), None);
        assert_eq!(score("Kit Cat", "CAT"), Some(2), "ignoring case");
    }

    #[test]
    fn a_first_sign_narrows_the_search() {
        assert_eq!(narrow("@kit"), (Narrow::People, "kit"));
        assert_eq!(narrow("# general"), (Narrow::Text, "general"));
        assert_eq!(narrow("!lounge"), (Narrow::Voice, "lounge"));
        assert_eq!(narrow("*cats"), (Narrow::Servers, "cats"));
        assert_eq!(narrow(" cats "), (Narrow::Everything, "cats"));
    }

    #[test]
    fn row_numbers_count_the_headings() {
        let item = |section| Item { target: Target::Guild(Id(1)), label: String::new(), detail: String::new(), picture: None, look: Look::Server, mentions: 0, section, server: None };
        let items = vec![item("Previous Channels"), item("Previous Channels"), item("Drafts")];
        assert_eq!((row_index(&items, 0), row_index(&items, 1), row_index(&items, 2)), (1, 2, 4));
    }
}
