//! The member list on the right of a server channel: group headers ("ONLINE
//! — 4", a role), then people with their presence, role colour and status.
//! The rows come from cord-core's lazy list; rows scrolled into view that the
//! server hasn't sent yet ask for their range.

use cord_core::Id;
use cord_core::cdn;
use cord_core::member_list::{ActivityCard, ActivityKind, Group, ListItem, ListMember, MemberListModel, PresenceLine, Status, StatusEmoji};
use cord_core::model::Role;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, AppContext, Context, Entity, EventEmitter, FontWeight, Hsla, InteractiveElement, IntoElement,
    ListAlignment, ListState, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled,
    StyledImage, Subscription, Window, div, img, list,
};

use crate::ui::u;
use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::store::{Services, StoreEvent};

const AVATAR: f32 = 32.;
const ROW_INSET: f32 = 8.;
/// Fixed row heights. Every row of a kind is exactly this tall in every state
/// (loading, with or without a status line), so the virtual list never
/// mis-measures and rows never overlap.
const MEMBER_ROW: f32 = 44.;
const HEADER_ROW: f32 = 40.;
const CARD_ROW: f32 = 68.;
/// The "Activity — N" cards above the list. Discord has none, so they are off.
const SHOW_ACTIVITY: bool = false;
/// Most activity cards shown at the top of the list.
const MAX_CARDS: usize = 5;

/// The height of a row of this kind.
fn row_height(kind: Kind) -> f32 {
    match kind {
        Kind::Header | Kind::ActivityHeader => HEADER_ROW,
        Kind::Card => CARD_ROW,
        Kind::Member => MEMBER_ROW,
        Kind::Empty => 0.,
    }
}

/// A name or avatar in the list was clicked.
pub enum MembersEvent {
    /// A right click on someone: their menu.
    OpenMenu { user: Id, name: String, position: gpui_kit::Point<gpui_kit::Pixels> },
    OpenProfile { user: Id, position: Point<Pixels> },
}

impl EventEmitter<MembersEvent> for MembersPanel {}

/// What a row of the list looks like, as far as its height goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Header,
    /// The collapsible "Activity — N" header.
    ActivityHeader,
    /// An activity card.
    Card,
    /// A member, or the placeholder standing in for one.
    Member,
    /// Something we don't draw.
    Empty,
}

/// Rows present in both lists whose height differs: the only ones the
/// virtual list has to measure again when the content changes.
fn height_changes(old: &[Kind], new: &[Kind]) -> Vec<usize> {
    old.iter().zip(new).enumerate().filter(|(_, (a, b))| a != b).map(|(ix, _)| ix).collect()
}

/// How many rows sit above the model's rows: the activity header and its cards.
fn lead_rows(total: usize, shown: usize, collapsed: bool) -> usize {
    match (total, collapsed) {
        (0, _) => 0,
        (_, true) => 1,
        _ => 1 + shown,
    }
}

/// Members with a game or music activity, most first, and how many there are.
fn activity_members(model: &MemberListModel) -> (Vec<ListMember>, usize) {
    if !SHOW_ACTIVITY {
        return (Vec::new(), 0);
    }
    let mut shown = Vec::new();
    let mut total = 0;
    for ix in 0..model.len() {
        if let Some(ListItem::Member(m)) = model.get(ix)
            && m.card.is_some()
            && m.status != Status::Offline
        {
            total += 1;
            if shown.len() < MAX_CARDS {
                shown.push(m.clone());
            }
        }
    }
    (shown, total)
}

fn kinds(model: Option<&MemberListModel>, total: usize, shown: usize, collapsed: bool) -> Vec<Kind> {
    let Some(model) = model else { return Vec::new() };
    let lead = lead_rows(total, shown, collapsed);
    (0..lead)
        .map(|ix| if ix == 0 { Kind::ActivityHeader } else { Kind::Card })
        .chain((0..model.len())
        .map(|ix| match model.get(ix) {
            Some(ListItem::Group(_)) => Kind::Header,
            Some(ListItem::Unknown) => Kind::Empty,
            Some(ListItem::Member(_)) | None => Kind::Member,
        }))
        .collect()
}

pub struct MembersPanel {
    /// The channel an old list is standing in for.
    stale_for: Option<Id>,
    services: Services,
    guild: Id,
    list: ListState,
    scroller: SmoothScroller,
    /// The row kinds `list` was last told about.
    seen: Vec<Kind>,
    /// Members whose activity cards are shown, and how many there are in all.
    cards: Vec<ListMember>,
    activity_total: usize,
    activity_collapsed: bool,
    /// The member row under the pointer: its nameplate animates.
    hovered: Option<Id>,
    _subscriptions: Vec<Subscription>,
}

impl MembersPanel {
    pub fn new(services: Services, guild: Id, channel: Id, cx: &mut Context<Self>) -> Self {
        let list = ListState::new(0, ListAlignment::Top, gpui_kit::px(300.));
        let subscriptions = vec![
            // Role colours and names change through the store's ordinary notifications.
            cx.observe(&services.store, |this, _, cx| {
                this.sync(cx);
                cx.notify();
            }),
            // The list itself changes constantly and gets its own event, so the
            // chat page isn't redrawn for every presence tick.
            cx.subscribe(&services.store, |this, _, event: &StoreEvent, cx| {
                if matches!(event, StoreEvent::MemberList) {
                    this.sync(cx);
                    cx.notify();
                }
            }),
            cx.observe(&services.settings, |_, _, cx| cx.notify()),
        ];
        services.store.update(cx, |store, cx| store.subscribe_members(guild, channel, cx));
        let mut panel = MembersPanel {
            stale_for: None,
            services,
            guild,
            scroller: SmoothScroller::new(ScrollTarget::List(list.clone())),
            list,
            seen: Vec::new(),
            cards: Vec::new(),
            activity_total: 0,
            activity_collapsed: false,
            hovered: None,
            _subscriptions: subscriptions,
        };
        panel.sync(cx);
        panel
    }

    /// Keeps the virtual list in step with the member list model.
    fn sync(&mut self, cx: &mut Context<Self>) {
        let store = self.services.store.read(cx);
        // Who's hidden has changed, and with it the height of their rows.
        let stale = store.cache().member_list_stale_for();
        if self.stale_for != stale {
            self.stale_for = stale;
            self.list.remeasure_items(0..self.list.item_count());
        }
        let model = store.cache().member_list().filter(|m| m.guild_id == self.guild);
        let (cards, total) = model.map(activity_members).unwrap_or_default();
        let now = kinds(model, total, cards.len(), self.activity_collapsed);
        self.cards = cards;
        self.activity_total = total;
        let old = self.seen.len();
        if now.len() > old {
            self.list.splice(old..old, now.len() - old);
        } else if now.len() < old {
            self.list.splice(now.len()..old, 0);
        }
        for ix in height_changes(&self.seen, &now) {
            self.list.remeasure_items(ix..ix + 1);
        }
        self.seen = now;
    }
}

/// The name a member goes by in the list: server nickname, else global name.
pub fn list_name(m: &ListMember) -> &str {
    m.nick.as_deref().filter(|n| !n.is_empty()).unwrap_or_else(|| m.user.display_name())
}

fn avatar_url(guild: Id, m: &ListMember, size: u32) -> String {
    match m.avatar.as_deref().filter(|a| !a.is_empty()) {
        Some(hash) => cdn::guild_avatar_url(guild, m.user.id, hash, size),
        None => cdn::avatar_url(m.user.id, m.user.avatar.as_deref(), size),
    }
}

/// "Online — 4", "Offline — 12", or the role's name as written. `count` is the
/// group's total from the list update (the row itself carries none).
pub fn group_title(group: &Group, count: u32, roles: &[Role]) -> String {
    let name = match group.id.as_str() {
        "online" => "Online".to_string(),
        "offline" => "Offline".to_string(),
        _ => group
            .role()
            .and_then(|id| roles.iter().find(|r| r.id == id))
            .map_or_else(|| "Members".to_string(), |r| r.name.clone()),
    };
    format!("{name} — {count}")
}

/// Discord's presence colours.
pub fn status_color(status: Status) -> Hsla {
    rgb_color(match status {
        Status::Online => 0x23a55a,
        Status::Idle => 0xf0b232,
        Status::Dnd => 0xf23f43,
        Status::Offline => 0x80848e,
    })
}

/// 0xRRGGBB, as Discord stores role colours.
pub fn rgb_color(rgb: u32) -> Hsla {
    gpui_kit::rgb(rgb).into()
}

/// Grey stand-ins for the rows while the list comes in.
fn member_skeleton(cx: &gpui_kit::App) -> impl IntoElement {
    // Name widths, in px: uneven, as names are.
    const NAMES: [f32; 16] = [96., 132., 78., 118., 88., 146., 104., 72., 124., 92., 138., 84., 110., 76., 128., 98.];
    let bar = cx.theme().foreground.opacity(0.07);
    v_flex().w_full().pt(u(20.)).px(u(16.)).gap(u(14.)).overflow_hidden().children(NAMES.iter().map(|name| {
        h_flex()
            .flex_shrink_0()
            .gap(u(12.))
            .items_center()
            .child(div().size(u(32.)).flex_shrink_0().rounded_full().bg(bar))
            .child(div().w(u(*name)).h(u(12.)).rounded(u(6.)).bg(bar))
    }))
}

/// An avatar with its avatar decoration (`decoration`, 1.2× the avatar,
/// centred) and Discord's status mark on top of both: a dot when online, a
/// moon when idle, a barred dot for do-not-disturb, a ring when offline, and
/// a phone when they're only on mobile.
pub fn avatar_with_status(
    avatar: Avatar,
    size: f32,
    status: Option<Status>,
    mobile: bool,
    decoration: Option<String>,
    ring: Hsla,
) -> impl IntoElement {
    avatar_with_overlay(avatar, size, status, mobile, decoration, ring, None)
}

/// `avatar_with_status`, with `over` on the picture itself: under the decoration
/// and the status mark.
pub fn avatar_with_overlay(
    avatar: Avatar,
    size: f32,
    status: Option<Status>,
    mobile: bool,
    decoration: Option<String>,
    ring: Hsla,
    over: Option<gpui_kit::AnyElement>,
) -> impl IntoElement {
    let dot = (size * 0.3).max(10.);
    div()
        .relative()
        .flex_shrink_0()
        .size(u(size))
        .child(avatar.size(u(size)))
        .children(over)
        .when_some(decoration, |this, url| {
            // Clipped a pixel in: scaled pictures pick up a faint line from
            // whatever sits beside them in the texture atlas.
            this.child(
                div()
                    .absolute()
                    .top(u(1. - size * 0.1))
                    .left(u(1. - size * 0.1))
                    .size(u(size * 1.2 - 2.))
                    .overflow_hidden()
                    .child(img(url).absolute().top(u(-1.)).left(u(-1.)).size(u(size * 1.2))),
            )
        })
        .when_some(status, |this, status| {
            let color = status_color(status);
            let mark = match status {
                Status::Online if mobile => div()
                    .w(u(dot * 0.7))
                    .h(u(dot))
                    .rounded(u(dot * 0.15))
                    .bg(color)
                    .flex()
                    .justify_center()
                    .pt(u(dot * 0.15))
                    .child(div().w(u(dot * 0.45)).h(u(dot * 0.55)).rounded(u(1.)).bg(ring)),
                Status::Idle => div()
                    .relative()
                    .size(u(dot))
                    .rounded_full()
                    .bg(color)
                    .child(div().absolute().top(u(-dot * 0.1)).left(u(-dot * 0.1)).size(u(dot * 0.6)).rounded_full().bg(ring)),
                Status::Dnd => div()
                    .size(u(dot))
                    .rounded_full()
                    .bg(color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().w(u(dot * 0.6)).h(u(dot * 0.22)).rounded_full().bg(ring)),
                Status::Offline => div()
                    .size(u(dot))
                    .rounded_full()
                    .bg(color)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(div().size(u(dot * 0.45)).rounded_full().bg(ring)),
                Status::Online => div().size(u(dot)).rounded_full().bg(color),
            };
            this.child(
                div()
                    .absolute()
                    .right(u(-2.))
                    .bottom(u(-2.))
                    .size(u(dot + 4.))
                    .rounded_full()
                    .bg(ring)
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(mark),
            )
        })
}

/// A group's heading; a role with an icon shows it before the name.
fn header_row(title: String, icon: Option<cord_core::cache::RoleIcon>, cx: &gpui_kit::App) -> AnyElement {
    h_flex()
        .w_full()
        .h(u(row_height(Kind::Header)))
        .px(u(ROW_INSET + 8.))
        .pt(u(19.))
        .gap(u(4.))
        .items_start()
        .when_some(icon, |d, icon| d.child(crate::badges::role_icon("group-icon", &icon, 16.)))
        .text_size(u(13.))
        .line_height(u(16.))
        .font_weight(FontWeight::MEDIUM)
        .text_color(cx.theme().muted_foreground)
        .child(title)
        .into_any_element()
}

/// The icon drawn before an activity line; custom statuses have none.
fn activity_icon(kind: Option<ActivityKind>) -> Option<IconName> {
    match kind? {
        ActivityKind::Playing | ActivityKind::Competing => Some(IconName::Gamepad2),
        ActivityKind::Listening => Some(IconName::Music),
        ActivityKind::Streaming | ActivityKind::Watching => Some(IconName::Monitor),
        ActivityKind::Custom | ActivityKind::Other => None,
    }
}

/// What the line under a name is made of.
#[derive(Debug, PartialEq)]
pub struct StatusParts {
    /// The green speaker: they're connected to voice.
    pub voice: bool,
    /// The green game icon and how many more games, beside a custom status.
    pub game: Option<(IconName, u32)>,
    /// A "•" before the status.
    pub bullet: bool,
    pub emoji: Option<StatusEmoji>,
    /// The icon drawn with the text (a game's line, when no custom status).
    pub text_icon: Option<IconName>,
    pub text: String,
}

/// How Discord lays the line out: a custom status beside small voice and
/// game icons; with none, the game's own line; or just "In voice".
pub fn status_parts(line: &PresenceLine, voice: bool) -> Option<StatusParts> {
    let custom = line.custom.is_some() || line.emoji.is_some();
    let more = line.games.saturating_sub(1);
    if custom {
        let game = activity_icon(line.game).map(|icon| (icon, more));
        return Some(StatusParts {
            voice,
            bullet: voice || game.is_some(),
            game,
            emoji: line.emoji.clone(),
            text_icon: None,
            text: line.custom.clone().unwrap_or_default(),
        });
    }
    if let Some(text) = &line.game_text {
        let text = if more > 0 { format!("{text} +{more}") } else { text.clone() };
        return Some(StatusParts { voice, game: None, bullet: voice, emoji: None, text_icon: activity_icon(line.game), text });
    }
    voice.then(|| StatusParts { voice, game: None, bullet: false, emoji: None, text_icon: None, text: "In voice".into() })
}

fn status_emoji(emoji: &StatusEmoji) -> AnyElement {
    match emoji.id {
        Some(id) => crate::anim::emoji(crate::message_text::custom_emoji_url(id, emoji.animated), 14.),
        None => crate::badges::emoji_image(&emoji.name, 14.).into_any_element(),
    }
}

/// The line under a name, in the member list and DMs alike.
pub fn status_element(line: &PresenceLine, voice: bool, cx: &gpui_kit::App) -> Option<AnyElement> {
    let parts = status_parts(line, voice)?;
    let theme = cx.theme();
    let green = status_color(Status::Online);
    Some(
        h_flex()
            .h(u(16.))
            .min_w_0()
            .gap(u(4.))
            .items_center()
            .text_color(theme.muted_foreground)
            .when(parts.voice, |this| this.child(Icon::new(IconName::Volume2).size(u(12.)).flex_shrink_0().text_color(green)))
            .when_some(parts.game, |this, (icon, more)| {
                this.child(Icon::new(icon).size(u(13.)).flex_shrink_0().text_color(green)).when(more > 0, |this| {
                    this.child(div().flex_shrink_0().text_size(u(11.)).text_color(green).child(format!("+{more}")))
                })
            })
            .when(parts.bullet, |this| this.child(div().flex_shrink_0().text_size(u(11.)).child("\u{2022}")))
            .when_some(parts.text_icon, |this, icon| this.child(Icon::new(icon).size(u(12.)).flex_shrink_0()))
            .when_some(parts.emoji.as_ref(), |this, emoji| this.child(status_emoji(emoji)))
            .child(div().min_w_0().truncate().text_size(u(12.)).line_height(u(16.)).child(parts.text))
            .into_any_element(),
    )
}

/// A small pill after a name: the blurple "APP" or a server tag.
fn pill(text: String, fill: Option<Hsla>, cx: &gpui_kit::App) -> AnyElement {
    let theme = cx.theme();
    div()
        .flex_shrink_0()
        .h(u(13.))
        .px(u(4.))
        .rounded(u(3.))
        .flex()
        .items_center()
        .text_size(u(9.))
        .line_height(u(13.))
        .font_weight(FontWeight::SEMIBOLD)
        .when_some(fill, |this, fill| this.bg(fill).text_color(gpui_kit::white()))
        .when(fill.is_none(), |this| this.bg(theme.foreground.opacity(0.12)).text_color(theme.foreground))
        .child(text)
        .into_any_element()
}

const BOOST_PINK: u32 = 0xff73fa;

fn member_row(
    ix: usize,
    m: &ListMember,
    guild: Id,
    colors: &[u32],
    hovered: bool,
    in_voice: bool,
    panel: &gpui_kit::WeakEntity<MembersPanel>,
    cx: &gpui_kit::App,
) -> AnyElement {
    let theme = cx.theme();
    let (panel, user) = (panel.clone(), m.user.id);
    let offline = m.status == Status::Offline;
    let avatar = Avatar::new().border_0().src(avatar_url(guild, m, 64)).name(list_name(m).to_string());
    // A definite width, so long names and statuses truncate instead of
    // widening the row and wrapping onto the rows around it.
    let nameplate = m.user.nameplate_url();
    let palette = m.user.nameplate_palette().map(str::to_string);
    let nameplate_video = m.user.nameplate_video_url();
    let animated = nameplate_video.as_deref().filter(|_| hovered).and_then(|url| crate::nameplates::ready(url, cx));
    let look = crate::badges::name_look(&m.user, colors);
    let decoration = m.user.decoration_url();
    // What the art fades out into: the colour of the list behind it.
    let row_bg = theme.secondary;
    let row = div()
        .id(("member", ix))
        .relative()
        .w_full()
        .overflow_hidden()
        .px_2()
        // 44px rows in Discord: 42 drawn, 1px of space above and below.
        .h(u(MEMBER_ROW - 2.))
        .rounded(u(8.))
        .on_hover({
            let (panel, video) = (panel.clone(), nameplate_video.clone());
            move |over, _, cx| {
                if *over && let Some(url) = &video {
                    crate::nameplates::warm(url, cx);
                }
                let id = over.then_some(user);
                let over = *over;
                let _ = panel.update(cx, |panel, cx| {
                    let core = panel.services.core.clone();
                    crate::profile_preload::hover(user, Some(guild), over, &core, cx);
                    if panel.hovered != id {
                        panel.hovered = id;
                        cx.notify();
                    }
                });
            }
        })
        // A nameplate: the art behind the row, fading in from the left, animated
        // while hovered, with the row tinted its colour. GPUI doesn't clip
        // to rounded corners, so each layer rounds itself.
        .when_some(nameplate, |this, url| {
            // The animation replaces the still (both are see-through, so they
            // can't be stacked), and only once it's decoded: no gap between them.
            let layer = |src: gpui_kit::ImageSource| {
                img(src).absolute().top_0().left_0().size_full().rounded(u(8.)).object_fit(gpui_kit::ObjectFit::Cover)
            };
            let picture = match animated {
                Some(image) => crate::anim::image(image, gpui_kit::ObjectFit::Cover, 8.)
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .into_any_element(),
                None => layer(url.into()).into_any_element(),
            };
            this.child(
                div()
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full()
                    .rounded(u(8.))
                    .opacity(0.6)
                    .child(picture),
            )
                .children(crate::nameplates::backdrop(palette.as_deref(), row_bg, theme.mode.is_dark(), 8.))
        })
        .flex()
        .items_center()
        .gap_3()
        .cursor_pointer()
        .hover(|style| style.bg(theme.secondary_hover))
        .when(offline, |this| this.opacity(0.45))
        .on_mouse_down(gpui_kit::MouseButton::Right, {
            let (panel, name) = (panel.clone(), list_name(m).to_string());
            move |event, _, cx| {
                let (position, name) = (event.position, name.clone());
                let _ = panel.update(cx, |_, cx| cx.emit(MembersEvent::OpenMenu { user, name, position }));
            }
        })
        .on_click(move |event, _, cx| {
            let position = event.position();
            let _ = panel.update(cx, |_, cx| cx.emit(MembersEvent::OpenProfile { user, position }));
        })
        .child(avatar_with_status(avatar, AVATAR, Some(m.status), m.mobile, decoration, theme.background))
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .overflow_hidden()
                .child(
                    h_flex()
                        .h(u(17.5))
                        .gap(u(5.))
                        .items_center()
                        .child(
                            crate::badges::name_clip(div().min_w_0(), &look.stops)
                                .text_size(u(16.))
                                .line_height(u(20.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(look.color.unwrap_or(theme.foreground))
                                .when_some(look.font, |this, font| this.font_family(font))
                                .child(crate::badges::name_effect(crate::badges::name_text(("member-name", ix), list_name(m), &look.stops, hovered), list_name(m), &look, hovered)),
                        )
                        .when(m.bot, |this| this.child(pill("✓ APP".into(), Some(cx.theme().primary), cx)))
                        .map(|this| match m.user.server_tag() {
                            Some((tag, badge)) => {
                                this.child(crate::badges::tag_pill(format!("mtag-{ix}"), tag, badge.as_deref(), cx))
                            }
                            None => this.when_some(m.tag.clone(), |this, tag| this.child(pill(tag, None, cx))),
                        })
                        // Discord's member list shows the boost gem, not role icons.
                        .when(m.booster, |this| {
                            this.child(
                                Icon::default().path("icons/boost.svg").size(u(14.)).flex_shrink_0().text_color(rgb_color(BOOST_PINK)),
                            )
                        }),
                )
                .children(status_element(&m.line, in_voice, cx)),
        );
    div().w_full().px(u(ROW_INSET)).py(u(1.)).child(row).into_any_element()
}

/// Time since an activity began: "4:07", or "3:16:27" past an hour.
pub fn elapsed_text(start_ms: u64, now_ms: u64) -> String {
    let secs = now_ms.saturating_sub(start_ms) / 1000;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m}:{s:02}") }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

/// "Activity — 3" with a chevron; clicking folds the cards away.
fn activity_header_row(
    total: usize,
    collapsed: bool,
    panel: &gpui_kit::WeakEntity<MembersPanel>,
    cx: &gpui_kit::App,
) -> AnyElement {
    let theme = cx.theme();
    let panel = panel.clone();
    div()
        .id("activity-header")
        .w_full()
        .h(u(HEADER_ROW))
        .px(u(ROW_INSET + 8.))
        .pt(u(19.))
        .cursor_pointer()
        .on_click(move |_, _, cx| {
            let _ = panel.update(cx, |this, cx| {
                this.activity_collapsed = !this.activity_collapsed;
                this.sync(cx);
                cx.notify();
            });
        })
        .child(
            h_flex()
                .h(u(16.))
                .gap(u(3.))
                .items_center()
                .text_size(u(13.))
                .line_height(u(16.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .child(format!("Activity — {total}"))
                .child(
                    Icon::new(if collapsed { IconName::ChevronRight } else { IconName::ChevronDown })
                        .size(u(11.)),
                ),
        )
        .into_any_element()
}

/// One activity card: who, what they're doing, for how long, and the art.
fn activity_card_row(
    ix: usize,
    m: &ListMember,
    card: &ActivityCard,
    guild: Id,
    color: Option<u32>,
    panel: &gpui_kit::WeakEntity<MembersPanel>,
    cx: &gpui_kit::App,
) -> AnyElement {
    let theme = cx.theme();
    let (panel, user) = (panel.clone(), m.user.id);
    let icon = activity_icon(Some(card.kind));
    let elapsed = card.start_ms.map(|s| elapsed_text(s, now_ms()));
    let avatar = Avatar::new().border_0().src(avatar_url(guild, m, 32)).name(list_name(m).to_string()).size(u(16.));
    let body = v_flex()
        .flex_1()
        .min_w_0()
        .gap(u(1.5))
        .child(
            h_flex()
                .h(u(16.))
                .gap(u(5.))
                .items_center()
                .child(div().flex_shrink_0().size(u(16.)).child(avatar))
                .child(
                    div()
                        .min_w_0()
                        .truncate()
                        .text_size(u(12.))
                        .line_height(u(16.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(color.map_or(theme.foreground, rgb_color))
                        .child(list_name(m).to_string()),
                ),
        )
        .child(
            div()
                .truncate()
                .text_size(u(12.))
                .line_height(u(16.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.foreground)
                .child(card.details.clone().unwrap_or_else(|| card.name.clone())),
        )
        .child(
            h_flex()
                .h(u(14.5))
                .gap(u(4.))
                .items_center()
                .text_size(u(11.))
                .line_height(u(14.5))
                .text_color(status_color(Status::Online))
                .when_some(icon, |this, icon| this.child(Icon::new(icon).size(u(11.)).flex_shrink_0()))
                .when_some(elapsed, |this, text| this.child(div().flex_shrink_0().child(text)))
                .when(card.details.is_some(), |this| {
                    this.child(
                        div().min_w_0().truncate().text_color(theme.muted_foreground).child(card.name.clone()),
                    )
                }),
        );
    let row = h_flex()
        .id(("activity-card", ix))
        .w_full()
        .h(u(CARD_ROW - 8.))
        .overflow_hidden()
        .p(u(6.5))
        .gap(u(6.5))
        .items_center()
        .rounded(u(6.5))
        .bg(theme.muted)
        .border_1()
        .border_color(theme.border)
        .cursor_pointer()
        .hover(|style| style.bg(theme.secondary_hover))
        .on_click(move |event, _, cx| {
            let position = event.position();
            let _ = panel.update(cx, |_, cx| cx.emit(MembersEvent::OpenProfile { user, position }));
        })
        .child(body)
        .when_some(card.image.clone(), |this, url| {
            this.child(
                div().flex_shrink_0().size(u(45.)).rounded(u(3.)).overflow_hidden().child(
                    gpui_kit::img(cord_core::cdn::fitted_url(&url, 112, 112))
                        .size_full()
                        .object_fit(gpui_kit::ObjectFit::Cover)
                        .with_fallback(|| div().into_any_element()),
                ),
            )
        });
    div().w_full().px(u(ROW_INSET)).pb(u(6.5)).child(row).into_any_element()
}

/// A grey bar where a row will be once the server sends it.
fn placeholder_row(cx: &gpui_kit::App) -> AnyElement {
    h_flex()
        .h(u(row_height(Kind::Member)))
        .mx(u(ROW_INSET))
        .px_2()
        .gap_3()
        .items_center()
        .child(div().size(u(AVATAR)).rounded_full().bg(cx.theme().secondary_hover))
        .child(div().h(u(9.5)).w(u(104.)).rounded(u(4.)).bg(cx.theme().secondary_hover))
        .into_any_element()
}

impl Render for MembersPanel {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::count("members");
        let theme = cx.theme();
        let smooth = self.services.settings.read(cx).get().smooth_scrolling;
        let store = self.services.store.clone();
        let panel = cx.entity().downgrade();
        let hovered = self.hovered;
        let guild = self.guild;
        let empty = self.seen.is_empty();
        let cards = self.cards.clone();
        let (total, collapsed) = (self.activity_total, self.activity_collapsed);
        let lead = lead_rows(total, cards.len(), collapsed);

        let rows = list(self.list.clone(), move |ix, _window, cx| {
            let cache = store.read(cx).cache();
            let Some(model) = cache.member_list() else { return div().into_any_element() };
            if ix < lead {
                if ix == 0 {
                    return activity_header_row(total, collapsed, &panel, cx);
                }
                return match cards.get(ix - 1) {
                    Some(m) => match &m.card {
                        Some(card) => {
                            activity_card_row(ix, m, card, guild, cache.member_color(guild, m.user.id), &panel, cx)
                        }
                        None => div().into_any_element(),
                    },
                    None => div().into_any_element(),
                };
            }
            let (ix, row_ix) = (ix - lead, ix);
            match model.get(ix) {
                Some(ListItem::Group(group)) => {
                    let roles = cache.guild(guild).map(|g| g.roles.as_slice()).unwrap_or_default();
                    let count = model.group_count(&group.id).unwrap_or(group.count);
                    let icon = group.id.parse().ok().and_then(|role| cache.role_icon_for(guild, &[Id(role)]));
                    header_row(group_title(group, count, roles), icon, cx)
                }
                // Still showing another channel's list until this one's comes: not
                // those who can't see this one.
                Some(ListItem::Member(m)) if cache.member_list_stale_for().is_some_and(|channel| !cache.member_can_see(channel, m.user.id)) => div().into_any_element(),
                Some(ListItem::Member(m)) => {
                    let in_voice = cache.voice_state(guild, m.user.id).is_some_and(|v| v.channel_id.is_some());
                    member_row(row_ix, m, guild, &cache.member_colors(guild, m.user.id), hovered == Some(m.user.id), in_voice, &panel, cx)
                }
                Some(ListItem::Unknown) => div().into_any_element(),
                None => {
                    // Scrolled to a range the server hasn't sent: ask for it.
                    let store = store.clone();
                    cx.defer(move |cx| store.update(cx, |store, cx| store.want_member_row(ix, cx)));
                    placeholder_row(cx)
                }
            }
        });

        v_flex()
            .w_full()
            .h_full()
            .flex_shrink_0()
            .bg(theme.secondary)
            .border_l_1()
            .border_color(theme.border)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(empty, |this| this.child(member_skeleton(cx)))
                    .when(false, |this| {
                        this.child(
                            div().p_4().text_sm().text_color(theme.muted_foreground).child("Loading members…"),
                        )
                    })
                    .when(smooth && !empty, |this| this.child(self.scroller.layer()))
                    .when(!empty, |this| this.child(rows.size_full())),
            )
    }
}

pub fn new(services: Services, guild: Id, channel: Id, cx: &mut gpui_kit::App) -> Entity<MembersPanel> {
    cx.new(|cx| MembersPanel::new(services, guild, channel, cx))
}

#[cfg(test)]
mod tests {
    use super::*;
    use cord_core::model::User;

    fn member(nick: Option<&str>, global: Option<&str>, avatar: Option<&str>) -> ListMember {
        ListMember {
            user: User { id: Id(7), username: "gus".into(), global_name: global.map(Into::into), avatar: None, bot: false, extras: Default::default() },
            nick: nick.map(Into::into),
            avatar: avatar.map(Into::into),
            roles: vec![],
            status: Status::Online,
            activity: None,
            activity_kind: None,
            bot: false,
            tag: None,
            card: None,
            mobile: false,
            booster: false,
            line: PresenceLine::default(),
        }
    }

    #[test]
    fn only_rows_whose_height_changed_are_measured_again() {
        use Kind::*;
        // A placeholder filling in with its member keeps its height.
        assert!(height_changes(&[Header, Member, Member], &[Header, Member, Member]).is_empty());
        // Rows only in one of the lists are the splice's business.
        assert!(height_changes(&[Header], &[Header, Member]).is_empty());
        assert_eq!(height_changes(&[Header, Member, Empty], &[Header, Header, Member]), [1, 2]);
    }

    #[test]
    fn row_heights_are_fixed_per_kind() {
        // Discord's sizes at 100% zoom; `ui::u` scales them with Settings › Zoom.
        assert_eq!(row_height(Kind::Member), 44.);
        assert_eq!(row_height(Kind::Header), 40.);
        assert_eq!(row_height(Kind::ActivityHeader), 40.);
        assert_eq!(row_height(Kind::Empty), 0.);
        // A row with a status line fits: 20px name line + 16px status line.
        assert!(20. + 16. <= row_height(Kind::Member));
        assert!(AVATAR <= row_height(Kind::Member));
    }

    #[test]
    fn activity_icons() {
        assert!(activity_icon(Some(ActivityKind::Playing)).is_some());
        assert!(activity_icon(Some(ActivityKind::Listening)).is_some());
        assert!(activity_icon(Some(ActivityKind::Custom)).is_none());
        assert!(activity_icon(None).is_none());
    }

    #[test]
    fn names_prefer_nickname_then_global_name() {
        assert_eq!(list_name(&member(Some("Gusto"), Some("Gus G"), None)), "Gusto");
        assert_eq!(list_name(&member(Some(""), Some("Gus G"), None)), "Gus G");
        assert_eq!(list_name(&member(None, None, None)), "gus");
    }

    #[test]
    fn avatars_prefer_the_server_one() {
        assert_eq!(
            avatar_url(Id(300), &member(None, None, Some("sv")), 64),
            "https://cdn.discordapp.com/guilds/300/users/7/avatars/sv.png?size=64"
        );
        assert!(avatar_url(Id(300), &member(None, None, None), 64).contains("/embed/avatars/"));
    }

    #[test]
    fn group_titles() {
        let roles = [Role { id: Id(301), name: "Moderators".into(), color: 0, position: 1, permissions: 0, hoist: true, icon: None, unicode_emoji: None, colors: None, mentionable: false }];
        let g = |id: &str, count| Group { id: id.into(), count };
        assert_eq!(group_title(&g("online", 0), 4, &roles), "Online — 4");
        assert_eq!(group_title(&g("offline", 0), 12, &roles), "Offline — 12");
        assert_eq!(group_title(&g("301", 0), 2, &roles), "Moderators — 2");
        assert_eq!(group_title(&g("999", 0), 1, &roles), "Members — 1", "a role we don't know");
    }

    #[test]
    fn elapsed() {
        assert_eq!(elapsed_text(0, 247_000), "4:07");
        assert_eq!(elapsed_text(0, 11_787_000), "3:16:27");
        assert_eq!(elapsed_text(5_000, 1_000), "0:00", "a start in the future");
    }

    #[test]
    fn lead_rows_count() {
        assert_eq!(lead_rows(0, 0, false), 0);
        assert_eq!(lead_rows(7, 5, false), 6);
        assert_eq!(lead_rows(7, 5, true), 1);
    }

    #[test]
    fn status_lines() {
        let mut line = PresenceLine::default();
        assert_eq!(status_parts(&line, false), None);
        assert_eq!(status_parts(&line, true).unwrap().text, "In voice");
        line.custom = Some("hi".into());
        let p = status_parts(&line, true).unwrap();
        assert!(p.voice && p.bullet && p.game.is_none());
        assert!(!status_parts(&line, false).unwrap().bullet);
        line.game = Some(ActivityKind::Playing);
        line.games = 2;
        assert_eq!(status_parts(&line, false).unwrap().game, Some((IconName::Gamepad2, 1)));
        line.custom = None;
        line.game_text = Some("Playing Rust".into());
        assert_eq!(status_parts(&line, false).unwrap().text, "Playing Rust +1");
    }
}
