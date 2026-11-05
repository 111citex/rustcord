//! The profile card: a popover anchored where someone's name or avatar was
//! clicked. It shows what the cache already knows at once and fills in the
//! rest (banner, bio, pronouns, member since) when the profile arrives.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use cord_core::Id;
use cord_core::model::{Role, User};
use cord_core::profile::UserProfile;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::{ActiveTheme, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnimationExt, AnyElement, Context, Hsla, EventEmitter, FocusHandle, FontWeight, InteractiveElement, IntoElement,
    KeyDownEvent, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled, StyledImage,
    Task, Window, anchored,
    deferred, div, img,
};

use crate::ui::u;
use crate::members::{avatar_with_status, rgb_color};
use crate::store::Services;

const WIDTH: f32 = 300.;
const CLOSE_AFTER_NOTCHES: f32 = 5.;
const BANNER: f32 = 105.;
const AVATAR: f32 = 80.;

thread_local! {
    /// Each game's icon, fetched once. `None` is none, or not here yet.
    static APP_ICONS: std::cell::RefCell<std::collections::HashMap<String, Option<String>>> = std::cell::RefCell::default();
}

/// `app`'s icon if it's known, and otherwise goes to find out.
fn app_icon(app: String, core: std::sync::Arc<cord_core::Core>, cx: &mut Context<ProfileCard>) -> Option<String> {
    if let Some(known) = APP_ICONS.with_borrow(|icons| icons.get(&app).cloned()) {
        return known;
    }
    APP_ICONS.with_borrow_mut(|icons| icons.insert(app.clone(), None));
    cx.spawn(async move |card, cx| {
        let Ok(icon) = core.application_icon(app.clone()).await else { return };
        APP_ICONS.with_borrow_mut(|icons| icons.insert(app, icon));
        let _ = card.update(cx, |_, cx| cx.notify());
    })
    .detach();
    None
}

/// WCAG's contrast between two colours, from 1 (the same) to 21.
fn contrast(a: Hsla, b: Hsla) -> f32 {
    let light = |c: Hsla| {
        let c = gpui_kit::Rgba::from(c);
        let part = |v: f32| if v <= 0.03928 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) };
        0.2126 * part(c.r) + 0.7152 * part(c.g) + 0.0722 * part(c.b)
    };
    let (a, b) = (light(a), light(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A button in the profile's own colour, unless that would vanish into the card behind it,
/// then in its text colour. Returns the fill, and whichever of white or black reads on it.
fn button_colors(main: Hsla, surface: Hsla, text: Hsla) -> (Hsla, Hsla) {
    const STANDS_OUT: f32 = 1.6;
    let fill = if contrast(main, surface) < STANDS_OUT { text } else { main };
    let on_fill = if contrast(gpui_kit::white(), fill) >= contrast(gpui_kit::black(), fill) { gpui_kit::white() } else { gpui_kit::black() };
    (fill, on_fill)
}

/// The full profile's right side.
#[derive(Clone, Copy, PartialEq)]
enum Tab {
    Activity,
    Friends,
    Servers,
}

/// How many full profiles are open: the chat behind mustn't take the wheel.
static FULL_OPEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

pub fn full_open() -> bool {
    FULL_OPEN.load(std::sync::atomic::Ordering::Relaxed) > 0
}

/// The open card, and how much the wheel has turned lately.
struct CardWatch {
    card: gpui_kit::WeakEntity<ProfileCard>,
    notches: f32,
    last: std::time::Instant,
}

impl gpui_kit::Global for CardWatch {}

/// The wheel turned, anywhere in the window. The smooth scrollers call this
/// too: they take the wheel before the card's own listener could see it. A
/// nudge leaves the card open; `CLOSE_AFTER_NOTCHES` without a long pause
/// closes it. A full profile stays until it's dismissed.
pub fn note_wheel(notches: f32, cx: &mut gpui_kit::App) {
    if !cx.has_global::<CardWatch>() {
        return;
    }
    let now = std::time::Instant::now();
    let watch = cx.global_mut::<CardWatch>();
    let so_far = if now - watch.last > std::time::Duration::from_millis(1500) { 0. } else { watch.notches };
    watch.notches = so_far + notches;
    watch.last = now;
    if watch.notches >= CLOSE_AFTER_NOTCHES {
        watch.notches = 0.;
        let card = watch.card.clone();
        let _ = card.update(cx, |card, cx| {
            if !card.full {
                card.begin_close(cx);
            }
        });
    }
}

pub enum ProfileEvent {
    Close,
    /// A mutual friend clicked in a full profile: theirs, in its place.
    OpenFull(Id),
    /// The Message button: open a DM with this person.
    Message(Id),
}

impl EventEmitter<ProfileEvent> for ProfileCard {}

enum Load {
    Loading,
    Loaded(Arc<UserProfile>),
    /// Discord won't show it: they've set their profile private.
    Private,
    Failed,
}

pub struct ProfileCard {
    services: Services,
    user: Id,
    guild: Option<Id>,
    position: Point<Pixels>,
    /// Hangs left of `position` (beside the member list), not right of it.
    beside: bool,
    load: Load,
    /// The bio, parsed as markdown once.
    bio_cache: Rc<RefCell<crate::markdown_view::ParseCache>>,
    /// Which opening this is: each gets its own fade-in.
    opening: usize,
    /// The avatar was clicked: the whole profile, over the window.
    full: bool,
    /// The tab picked, if any: otherwise their activity, or else mutual friends.
    tab: Option<Tab>,
    /// The full profile's right side.
    right_scroll: gpui_kit::ScrollHandle,
    left_scroll: gpui_kit::ScrollHandle,
    left_scroller: crate::smooth_scroll::SmoothScroller,
    right_scroller: crate::smooth_scroll::SmoothScroller,
    /// The frame around the card, once its artwork is known.
    frame: Option<cord_core::profile::ProfileFrame>,
    effect: Option<cord_core::profile::ProfileEffect>,
    closing: Option<std::time::Instant>,
    toggled: bool,
    /// The roles' natural height, and their height when "+N" was clicked.
    roles_full: Rc<std::cell::Cell<Option<gpui_kit::Pixels>>>,
    roles_closed: Option<gpui_kit::Pixels>,
    _closing: Option<gpui_kit::Task<()>>,
    /// When the effect began, for its timing.
    effect_began: Rc<std::cell::Cell<Option<std::time::Instant>>>,
    /// "+N" was clicked: every role shows.
    roles_open: bool,
    focus: FocusHandle,
    _fetch: Task<()>,
}

impl ProfileCard {
    pub fn new(
        services: Services,
        user: Id,
        guild: Option<Id>,
        position: Point<Pixels>,
        beside: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        // Escape is heard while the card has focus.
        focus.focus(window, cx);
        let core = services.core.clone();
        let fetch = cx.spawn(async move |this, cx| {
            let result = core.fetch_profile(user, guild).await;
            let frame_sku = result.as_ref().ok().and_then(|p| p.frame_sku.clone());
            let effect_sku = result.as_ref().ok().and_then(|p| p.effect_sku.clone());
            let _ = this.update(cx, |card, cx| {
                card.load = match result {
                    Ok(profile) => Load::Loaded(profile),
                    Err(cord_core::http::HttpError::Forbidden(_)) => Load::Private,
                    Err(error) => {
                        tracing::warn!(%error, "couldn't load a profile");
                        Load::Failed
                    }
                };
                cx.notify();
            });
            if let Some(sku) = frame_sku {
                match core.profile_frame(sku).await {
                    Ok(frame) => {
                        let _ = this.update(cx, |card, cx| {
                            card.frame = frame;
                            cx.notify();
                        });
                    }
                    Err(error) => tracing::warn!(%error, "couldn't load a profile frame"),
                }
            }
            if let Some(sku) = effect_sku {
                match core.profile_effect(sku).await {
                    Ok(effect) => {
                        let _ = this.update(cx, |card, cx| {
                            if let Some(effect) = &effect {
                                crate::profile_effects::warm(effect, &core, cx);
                            }
                            card.effect = effect;
                            cx.notify();
                        });
                    }
                    Err(error) => tracing::warn!(%error, "couldn't load a profile effect"),
                }
            }
        });
        // Fetched while hovering: everything shows on the first frame.
        let cached = services.core.cached_profile(user, guild);
        let frame = cached.as_ref().and_then(|p| services.core.cached_frame(p.frame_sku.as_deref()?));
        static OPENINGS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let opening = OPENINGS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let left_scroll = gpui_kit::ScrollHandle::new();
        let left_scroller = crate::smooth_scroll::SmoothScroller::new(crate::smooth_scroll::ScrollTarget::Handle(left_scroll.clone())).in_modal();
        let right_scroll = gpui_kit::ScrollHandle::new();
        let right_scroller = crate::smooth_scroll::SmoothScroller::new(crate::smooth_scroll::ScrollTarget::Handle(right_scroll.clone())).in_modal();
        let card = cx.entity().downgrade();
        cx.set_global(CardWatch { card, notches: 0., last: std::time::Instant::now() });
        ProfileCard { toggled: false, roles_full: Rc::default(), roles_closed: None, closing: None, _closing: None, effect: None, effect_began: Rc::default(), left_scroll, left_scroller, right_scroll, right_scroller, opening, full: false, tab: None, services, user, guild, position, beside, load: cached.map_or(Load::Loading, Load::Loaded), bio_cache: Rc::default(), frame, roles_open: false, focus, _fetch: fetch }
    }
}

const CLOSE_FADE: std::time::Duration = std::time::Duration::from_millis(150);

/// How a Nitro profile theme draws: Discord lays a translucent overlay over the
/// two colours, dark or light to suit them, with text to match.
#[derive(Clone, Copy, Debug, PartialEq)]
struct ProfilePalette {
    overlay: gpui_kit::Hsla,
    text: gpui_kit::Hsla,
    muted: gpui_kit::Hsla,
}

impl ProfilePalette {
    fn of(top: u32) -> Self {
        let (r, g, b) = ((top >> 16) & 255, (top >> 8) & 255, top & 255);
        // Discord's darkness of a colour: light colours get the light theme.
        let darkness = 1. - (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32) / 255.;
        if darkness >= 0.5 {
            ProfilePalette { overlay: gpui_kit::black().opacity(0.6), text: gpui_kit::white(), muted: gpui_kit::white().opacity(0.75) }
        } else {
            let ink = rgb_color(0x060607);
            ProfilePalette { overlay: gpui_kit::white().opacity(0.6), text: ink, muted: ink.opacity(0.7) }
        }
    }
}

/// A role as a chip shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct Chip {
    pub id: Id,
    pub name: String,
    pub color: u32,
    /// One colour, or a gradient's stops.
    pub colors: Vec<u32>,
}

/// The member's roles, highest first. `@everyone` (the role with the
/// server's own id) isn't shown, as in Discord.
pub fn role_chips(guild: Id, roles: &[Role], member_roles: &[Id]) -> Vec<Chip> {
    let mut held: Vec<&Role> = roles.iter().filter(|r| r.id != guild && member_roles.contains(&r.id)).collect();
    held.sort_by_key(|r| std::cmp::Reverse((r.position, r.id)));
    held.into_iter().map(|r| Chip { id: r.id, name: r.name.clone(), color: r.color, colors: r.name_colors() }).collect()
}

/// The text of the full-width button at the bottom of the card.
pub fn message_label(username: &str) -> String {
    format!("Message @{username}")
}

impl Render for ProfileCard {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.full {
            return self.full_view(window, cx);
        }
        let theme = cx.theme();
        let store = self.services.store.read(cx);
        let cache = store.cache();
        let profile = match &self.load {
            Load::Loaded(p) => Some(p.clone()),
            _ => None,
        };

        let user: User = profile
            .as_ref()
            .map(|p| p.user.clone())
            .or_else(|| cache.user(self.user).cloned())
            .unwrap_or(User { id: self.user, username: "unknown-user".into(), global_name: None, avatar: None, bot: false, extras: Default::default() });
        // The cache's copy carries name styles, decorations and tags.
        let styled = cache.user(user.id).cloned().unwrap_or_else(|| user.clone());
        let name = profile
            .as_ref()
            .and_then(|p| p.nick.clone())
            .unwrap_or_else(|| if self.guild.is_some() { cache.display_name(self.guild, &user).to_string() } else { user.display_name().to_string() });
        let listed = self
            .guild
            .and_then(|g| cache.member_list().filter(|m| m.guild_id == g))
            .and_then(|m| m.find(user.id));
        let known = cache.presence(user.id);
        // No presence is how offline looks: Discord only sends them for who's online.
        let status = listed.map(|m| m.status).or(known.map(|p| p.status)).or(Some(cord_core::member_list::Status::Offline));
        let custom_line = listed.map(|m| m.line.clone()).or(known.map(|p| p.line.clone()));
        let member_roles: Vec<Id> = match (self.guild.and_then(|g| cache.member(g, user.id)), &profile) {
            (Some(m), _) => m.roles.clone(),
            (None, Some(p)) => p.roles.clone(),
            (None, None) => Vec::new(),
        };
        let chips = self
            .guild
            .and_then(|g| Some(role_chips(g, &cache.guild(g)?.roles, &member_roles)))
            .unwrap_or_default();
        // Who they are in this server first, then their own style; the profile's own copy has it for sure.
        let name_colors = self.guild.map(|g| cache.member_colors(g, user.id)).unwrap_or_default();
        let name_source = profile.as_ref().map(|p| &p.user).filter(|u| u.extras.display_name_styles.is_some()).unwrap_or(&styled);
        let name_look = crate::badges::name_look(name_source, &name_colors);
        let name_font = name_look.font;
        let is_me = cache.me().is_some_and(|me| me.id == user.id);
        let avatar_url = profile
            .as_ref()
            .and_then(|p| p.guild_avatar_url(160))
            .unwrap_or_else(|| cache.avatar_url(self.guild, &user, 160));
        let avatar = Avatar::new().border_0().src(avatar_url).name(name.clone());

        // A Nitro theme paints the whole card top to bottom; a shade that
        // lifts towards the bottom keeps text readable, as Discord's does.
        let themed = profile.as_ref().and_then(|p| p.theme_colors);
        let palette = themed.map(|(top, _)| ProfilePalette::of(top));
        let card_text = palette.map_or(theme.popover_foreground, |p| p.text);
        // The overlay is part of the picture, which gets dithered as a whole.
        let smooth_surface = themed.zip(palette).map(|((top, bottom), p)| (rgb_color(top).blend(p.overlay), rgb_color(bottom).blend(p.overlay)));
        let tint = palette.map_or(theme.foreground.opacity(0.07), |p| p.overlay);
        // The space around the avatar is the card's own colour, as if cut out of the banner.
        let ring = match (themed, palette) {
            (Some((top, _)), Some(p)) => rgb_color(top).blend(p.overlay),
            _ => theme.popover,
        };
        // A frame replaces the outline: an edge would show as a line under its art.
        let edge = match (&self.frame, themed) {
            (Some(_), _) => gpui_kit::transparent_black(),
            (None, Some((_, bottom))) => rgb_color(bottom),
            (None, None) => theme.border,
        };

        let accent = profile.as_ref().and_then(|p| p.accent_color).filter(|_| themed.is_none()).or(themed.map(|t| t.0));
        let banner = div().w_full().h(u(BANNER)).flex_shrink_0().overflow_hidden().rounded_t(u(7.)).bg(match accent {
            Some(rgb) => rgb_color(rgb),
            None => theme.primary.opacity(0.35),
        });
        // GPUI doesn't clip to the card's corners: the banner rounds itself.
        let banner = match profile.as_ref().and_then(|p| p.banner_url(600)) {
            Some(url) => banner.child(crate::anim::hover_banner(format!("card-banner-{}", self.opening), url, 7.)),
            None => banner,
        };

        let about = profile.as_ref().and_then(|p| p.bio.clone());
        let pronouns = profile.as_ref().and_then(|p| p.pronouns.clone());
        let badges = profile.as_ref().map(|p| p.badges.clone()).unwrap_or_default();
        let mutuals = profile.as_ref().map(|p| mutuals_line(p.mutual_friends, p.mutual_guilds.as_ref().map(Vec::len))).unwrap_or_default();
        let friend_line = profile.as_ref().map(|p| mutuals_line(p.mutual_friends, None)).unwrap_or_default();
        let server_line = profile.as_ref().map(|p| mutuals_line(None, p.mutual_guilds.as_ref().map(Vec::len))).unwrap_or_default();
        let faces = profile.as_ref().map(|p| p.mutual_friend_avatars.clone()).unwrap_or_default();
        let failed = matches!(self.load, Load::Failed);
        let loading = matches!(self.load, Load::Loading);
        let message_label = message_label(&user.username);
        let muted = palette.map_or(theme.muted_foreground, |p| p.muted);

        // Discord's row under the name: the first few, then "+N".
        let (shown, hidden) = badges_shown(badges.len());
        let badge_row = (!badges.is_empty()).then(|| {
            h_flex()
                .px(u(4.))
                .h(u(28.))
                .gap(u(2.))
                .items_center()
                .rounded(u(8.))
                .bg(tint)
                .children(badges.iter().take(shown).enumerate().map(|(i, b)| {
                    let tip = gpui_kit::SharedString::from(b.description.clone());
                    div()
                        .id(("badge", i))
                        .size(u(22.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                        .child(img(b.icon_url()).size(u(18.)))
                }))
                .when(hidden > 0, |d| d.child(div().px(u(3.)).text_size(u(12.)).font_weight(FontWeight::SEMIBOLD).child(format!("+{hidden}"))))
        });

        let bio = about.map(|bio| {
            let parsed = self.bio_cache.borrow_mut().parse(&bio);
            let shared = no_op_shared(self.bio_cache.clone());
            let message = placeholder_message();
            let env = crate::markdown_view::Env { shared: &shared, cache, message: &message, guild: self.guild };
            div().w_full().min_w_0().text_size(u(14.)).line_height(u(18.)).child(crate::markdown_view::blocks(&env, &parsed, "bio", cx))
        });

        // Roles fill two rows; the rest fold behind "+N", which opens them.
        let held = if self.roles_open { chips.len() } else { roles_that_fit(&chips) };
        let more = chips.len() - held;
        let role_pills: Vec<AnyElement> = chips
            .iter()
            .take(held)
            .enumerate()
            .map(|(i, c)| {
                let icon = self.guild.and_then(|g| cache.role_icon_for(g, &[c.id]));
                let dot: AnyElement = match c.colors.as_slice() {
                    [] => div().size(u(12.)).flex_shrink_0().rounded_full().bg(theme.muted_foreground).into_any_element(),
                    [one] => div().size(u(12.)).flex_shrink_0().rounded_full().bg(rgb_color(*one)).into_any_element(),
                    [one, two, ..] => h_flex()
                        .size(u(12.))
                        .flex_shrink_0()
                        .child(div().w(u(6.)).h_full().rounded_l(u(6.)).bg(rgb_color(*one)))
                        .child(div().w(u(6.)).h_full().rounded_r(u(6.)).bg(rgb_color(*two)))
                        .into_any_element(),
                };
                h_flex()
                    .gap(u(4.))
                    .items_center()
                    .h(u(24.))
                    .px(u(8.))
                    .rounded(u(12.))
                    .border_1()
                    .border_color(if let Some(p) = palette { p.text.opacity(0.2) } else { theme.border })
                    .text_size(u(12.))
                    .font_weight(FontWeight::MEDIUM)
                    .child(dot)
                    .child(c.name.clone())
                    .when_some(icon, |d, icon| d.child(crate::badges::role_icon(gpui_kit::SharedString::from(format!("chip-icon-{i}")), &icon, 14.)))
                    .into_any_element()
            })
            .collect();

        let content = v_flex()
            .px(u(16.))
            .pb(u(16.))
            .gap(u(12.))
            .rounded_b(u(8.))
            // The avatar hangs over the banner's bottom edge, ringed in the card colour.
            .child(
                div().h(u(AVATAR / 2. + 6.)).child(
                    div()
                        .id("profile-avatar")
                        .group("profile-avatar")
                        .relative()
                        .cursor_pointer()
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.open_full();
                            cx.notify();
                        }))
                        .mt(u(-AVATAR / 2. - 6.))
                        .size(u(AVATAR + 12.))
                        .p(u(6.))
                        .rounded(u((AVATAR + 12.) / 2.))
                        .bg(ring)
                        .child(crate::members::avatar_with_overlay(
                            avatar,
                            AVATAR,
                            status,
                            false,
                            styled.decoration_url_at(480),
                            ring,
                            // Dims under the pointer, to say it opens something.
                            Some(
                                div()
                                    .absolute()
                                    .top_0()
                                    .left_0()
                                    .size(u(AVATAR))
                                    .rounded_full()
                                    .invisible()
                                    .group_hover("profile-avatar", |d| d.visible())
                                    .bg(gpui_kit::black().opacity(0.3))
                                    .into_any_element(),
                            ),
                        )),
                ),
            )
            .child(
                v_flex()
                    .gap(u(4.))
                    .child(
                        h_flex()
                            .gap(u(6.))
                            .items_center()
                            .child(
                                crate::badges::name_clip(div().min_w_0(), &name_look.stops)
                                    .text_size(u(20.))
                                    .line_height(u(24.))
                                    .font_weight(FontWeight::BOLD)
                                    .text_color(name_look.color.unwrap_or(card_text))
                                    .when_some(name_font, |d, f| d.font_family(f))
                                    .child(crate::badges::name_effect(crate::badges::name_text("profile-name", &name, &name_look.stops, true), &name, &name_look, true)),
                            )
                            .when(user.bot, |d| d.child(crate::badges::app_badge())),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap(u(6.))
                            .items_center()
                            .text_size(u(14.))
                            .line_height(u(18.))
                            .child(match pronouns {
                                Some(p) => format!("{} \u{2022} {p}", user.handle()),
                                None => user.handle(),
                            })
                            .when_some(styled.server_tag(), |d, (tag, badge)| {
                                d.child(crate::badges::tag_pill_in("profile-tag", tag, badge.as_deref(), card_text))
                            }),
                    )
                    .children(badge_row.map(|row| h_flex().mt(u(4.)).child(row))),
            )
            .when(!mutuals.is_empty() && !is_me, |this| {
                this.child(
                    h_flex()
                        .gap(u(6.))
                        .items_center()
                        .text_size(u(13.))
                        .text_color(muted)
                        .when(!faces.is_empty(), |d| {
                            // Overlapping faces: sized by hand, as margins don't shrink a row.
                            let width = 16. + 12. * (faces.len() - 1) as f32;
                            d.child(h_flex().w(u(width)).flex_shrink_0().children(faces.iter().enumerate().map(|(i, url)| {
                                img(url.clone()).size(u(16.)).rounded_full().flex_shrink_0().when(i > 0, |d| d.ml(u(-4.)))
                            })))
                        })
                        // Each opens its own tab of the full profile.
                        .children([("friends", friend_line, Tab::Friends), ("servers", server_line, Tab::Servers)].into_iter().filter(|(_, line, _)| !line.is_empty()).enumerate().flat_map(|(i, (id, line, tab))| {
                            let dot = (i > 0).then(|| div().child("\u{2022}").into_any_element());
                            let link = div()
                                .id(id)
                                .cursor_pointer()
                                .hover(|d| d.underline())
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.tab = Some(tab);
                                    this.open_full();
                                    cx.notify();
                                }))
                                .child(line)
                                .into_any_element();
                            dot.into_iter().chain([link])
                        })),
                )
            })
            .when_some(custom_line.filter(|l| l.custom.is_some() || l.emoji.is_some()), |this, line| {
                this.child(
                    h_flex()
                        .gap(u(6.))
                        .items_center()
                        .px(u(10.))
                        .py(u(6.))
                        .rounded(u(8.))
                        .bg(tint)
                        .text_size(u(14.))
                        .line_height(u(18.))
                        .when_some(line.emoji.as_ref(), |d, e| match e.id {
                            Some(id) => d.child(crate::anim::emoji(crate::message_text::custom_emoji_url(id, e.animated), 18.)),
                            None => d.child(crate::badges::emoji_image(&e.name, 18.)),
                        })
                        .child(div().flex_1().min_w_0().child(line.custom.clone().unwrap_or_default())),
                )
            })
            .when(loading, |this| {
                this.child(
                    v_flex()
                        .gap(u(6.))
                        .child(div().w(u(180.)).h(u(12.)).rounded_full().bg(card_text.opacity(0.1)))
                        .child(div().w(u(120.)).h(u(12.)).rounded_full().bg(card_text.opacity(0.07))),
                )
            })
            .when(matches!(self.load, Load::Private), |this| {
                this.child(div().text_size(u(14.)).text_color(muted).child("This profile is private."))
            })
            .when(failed, |this| {
                this.child(div().text_size(u(14.)).text_color(muted).child("Couldn't load the full profile."))
            })
            .children(bio)
            .when(!role_pills.is_empty(), |this| {
                let (opened, moving) = crate::transition::progress(&gpui_kit::SharedString::from(format!("roles-{}", self.opening)), self.roles_open);
                let natural = self.roles_full.clone();
                let full = natural.get();
                let closed = self.roles_closed;
                // The row grows to its new height instead of the card jumping to it. The row
                // has to be the first child: that's the one measured.
                this.child(div().relative().overflow_hidden().when_some(closed.zip(full).filter(|_| moving), |d, (closed, full)| {
                    // Whole pixels: fractions make the text below shimmer as it settles.
                    d.h(gpui_kit::px(f32::from(closed + (full - closed) * opened).round()))
                }).on_children_prepainted(move |bounds, _, _| natural.set(bounds.first().map(|b| b.size.height))).child(
                    h_flex()
                        // Out of the flow while it opens, so the height being animated
                        // can't squeeze what's being measured to work it out.
                        .when(moving, |d| d.absolute().top_0().left_0().w_full())
                        .flex_wrap()
                        .gap(u(4.))
                        .children(role_pills)
                        .when(more > 0, |d| {
                            d.child(
                                div()
                                    .id("roles-more")
                                    .h(u(24.))
                                    .px(u(8.))
                                    .flex()
                                    .items_center()
                                    .rounded(u(12.))
                                    .border_1()
                                    .border_color(if let Some(p) = palette { p.text.opacity(0.2) } else { theme.border })
                                    .text_size(u(12.))
                                    .cursor_pointer()
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.roles_closed = this.roles_full.get();
                                        this.roles_open = true;
                                        cx.notify();
                                    }))
                                    .child(format!("+{more}")),
                            )
                        }),
                ).when(moving, |d| d.child(crate::transition::driver())))
            })
            .when(!is_me, |this| {
                let user = user.id;
                this.child(
                    div()
                        .id("profile-message")
                        .w_full()
                        .h(u(40.))
                        .px(u(12.))
                        .rounded(u(8.))
                        .flex()
                        .items_center()
                        .bg(tint)
                        .text_size(u(14.))
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|style| style.opacity(0.85))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(ProfileEvent::Message(user))))
                        .child(message_label),
                )
            });

        // A frame's pictures hang outside the card, scaled from their own
        // pixels to its width. Only "staple" layers are drawn so far.
        let scale = self.frame.as_ref().map_or(0., |f| WIDTH / f.inner_width);
        let (over_top, over_bottom, over_side) =
            self.frame.as_ref().map_or((0., 0., 0.), |f| (f.overflow_top * scale, f.overflow_bottom * scale, f.overflow_horizontal * scale));
        let frame_layers = |front: bool| -> Vec<AnyElement> {
            let Some(frame) = &self.frame else { return Vec::new() };
            frame
                .layers
                .iter()
                .filter(|l| l.kind == "staple" && (l.order == "front") == front)
                .map(|layer| {
                    let picture = img(frame.layer_url(layer)).absolute().left(u(-over_side)).w(u(WIDTH + over_side * 2.));
                    let picture = if layer.anchor == "bottom" { picture.bottom(u(-over_bottom)) } else { picture.top(u(-over_top)) };
                    picture.into_any_element()
                })
                .collect()
        };

        let card = v_flex()
            .id("profile-card")
            .relative()
            .track_focus(&self.focus)
            .occlude()
            .w(u(WIDTH))
            .rounded(u(8.))
            .border_1()
            .border_color(edge)
            .when(palette.is_none(), |d| d.bg(theme.popover))
            .children(smooth_surface.clone().map(|(top, bottom)| crate::gradient::fill(top, bottom, 8.)))
            .text_color(palette.map_or(theme.popover_foreground, |p| p.text))
            .shadow_lg()
            .on_mouse_down_out(cx.listener(|this, _, _, cx| this.begin_close(cx)))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.begin_close(cx);
                }
            }))
            .children(frame_layers(false))
            .child(banner)
            .child(content)
            .children(frame_layers(true))
            .on_hover(crate::transition::track_pointer(self.effect_hover_id()))
            .children(self.effect_layer(8.))
            .child(self.scroll_watch());
        // The frame hangs off the card without moving it; the only difference
        // is a wider margin when the card is pushed back inside the window.
        let margin = gpui_kit::px(8.) + window.rem_size() * (over_top.max(over_bottom).max(over_side) / 16.);

        let corner = if self.beside { gpui_kit::Anchor::TopRight } else { gpui_kit::Anchor::TopLeft };
        // Fades in and settles a little, so a fetch still under way reads as an opening.
        let opening = div().child(card).with_animation(
            ("profile-open", self.opening),
            gpui_kit::Animation::new(std::time::Duration::from_millis(170)).with_easing(gpui_kit::ease_out_quint()),
            |d, t| d.opacity(t).relative().top(u(8. * (1. - t))),
        );
        deferred(anchored().anchor(corner).position(self.position).snap_to_window_with_margin(margin).child(
            div()
                .when_some(self.closing_opacity(), |d, fade| d.opacity(fade).child(crate::transition::driver()))
                .child(opening),
        ))
            .with_priority(1)
            .into_any_element()
    }
}

impl Drop for ProfileCard {
    fn drop(&mut self) {
        if self.full {
            FULL_OPEN.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

/// A linked account's logo, by Discord's name for the service.
fn connection_icon(kind: &str) -> Option<String> {
    // Discord's own, from its asset list.
    let file = match kind {
        "battlenet" => "163c8cb9220efc74",
        "bluesky" => "2709f058378a099d",
        "crunchyroll" => "94ef3e8b7fa85a2e",
        "domain" => "b4376756bcbbf1ef",
        "ebay" => "b28a7a265581b6e3",
        "epicgames" => "199eceff4fca1a0c",
        "facebook" => "17be29f77bee4405",
        "github" => "a35ff3e86ffa1eb2",
        "instagram" => "c05dded52023ed43",
        "leagueoflegends" => "302a27a2e5cc3fb6",
        "mastodon" => "0e6385723fe5cd49",
        "paypal" => "dcb64a4ff8f61b2c",
        "playstation" => "eed203a7ec517e23",
        "reddit" => "adfd927dcc2049a5",
        "riotgames" => "772307b7d902b47b",
        "roblox" => "a4d8e9b0404a2d00",
        "skype" => "883a13b030d276ec",
        "spotify" => "d5719388ffc613da",
        "steam" => "1f7ec18f3695d4cf",
        "tiktok" => "a303feb844769624",
        "twitch" => "4fda00c96319c8ae",
        "twitter" => "a61999ae9bfb9658",
        "xbox" => "c4f09fda61827e19",
        "youtube" => "0fa530ba9c04ac32",
        _ => return None,
    };
    Some(format!("https://discord.com/assets/{file}.svg"))
}

impl ProfileCard {
    fn effect_hover_id(&self) -> gpui_kit::SharedString {
        format!("effect-hover-{}", self.opening).into()
    }

    /// The profile effect, thinning to 30% while the pointer's on the profile.
    fn effect_layer(&self, radius: f32) -> Option<gpui_kit::AnyElement> {
        let effect = self.effect.clone()?;
        let id = self.effect_hover_id();
        let (hovered, moving) = crate::transition::progress(&format!("{id}-fade").into(), crate::transition::pointer_on(&id));
        Some(
            div()
                .absolute()
                .top_0()
                .left_0()
                .size_full()
                .opacity(1. - 0.7 * hovered)
                .child(crate::profile_effects::overlay(effect, self.effect_began.clone(), radius))
                .when(moving, |d| d.child(crate::transition::driver()))
                .into_any_element(),
        )
    }

    /// Clicking their name again: fades out like any other close.
    pub fn toggle_off(&mut self, cx: &mut Context<Self>) {
        self.toggled = true;
        self.begin_close(cx);
    }

    pub fn toggled_off(&self) -> bool {
        self.toggled
    }

    /// Fades out, then asks to be closed.
    fn begin_close(&mut self, cx: &mut Context<Self>) {
        if self.closing.is_some() {
            return;
        }
        self.closing = Some(std::time::Instant::now());
        cx.notify();
        self._closing = Some(cx.spawn(async move |this, cx| {
            cx.background_executor().timer(CLOSE_FADE).await;
            let _ = this.update(cx, |_, cx| cx.emit(ProfileEvent::Close));
        }));
    }

    /// How see-through it should be, while closing.
    fn closing_opacity(&self) -> Option<f32> {
        let progress = self.closing?.elapsed().as_secs_f32() / CLOSE_FADE.as_secs_f32();
        Some(1. - crate::transition::ease(progress))
    }

    pub fn user(&self) -> Id {
        self.user
    }

    /// A trackpad, which no smooth scroller takes, still counts towards closing
    /// the card. Listens over the whole window.
    fn scroll_watch(&self) -> impl IntoElement {
        gpui_kit::canvas(
            |_, _, _| {},
            |_, _, window, _| {
                window.on_mouse_event(|event: &gpui_kit::ScrollWheelEvent, phase, _, cx| {
                    if phase != gpui_kit::DispatchPhase::Capture {
                        return;
                    }
                    // A mouse wheel is the smooth scrollers' to report, when it moves something.
                    if let gpui_kit::ScrollDelta::Pixels(pixels) = event.delta {
                        note_wheel(f32::from(pixels.y).abs() / 50., cx);
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }

    pub fn open_full(&mut self) {
        if !self.full {
            self.full = true;
            FULL_OPEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// The whole profile, opened from the card's avatar: everything the card
    /// has room for and more, with the people and servers you share beside it.
    /// Under the name: Message and the friend button, the way round that fits whether
    /// they're a friend, then the "…" menu. The main button takes their profile's color.
    fn full_actions(&self, card: gpui_kit::WeakEntity<Self>, user: Id, name: &str, main: Hsla, surface: Hsla, text: Hsla, cx: &gpui_kit::App) -> AnyElement {
        use crate::context_menu::{self as menu, Entry};
        use gpui_kit::component::Icon;
        let friend = self.services.store.read(cx).cache().is_friend(user);
        let (fill, on_fill) = button_colors(main, surface, text);
        let wide = |id: &'static str, icon: Icon, label: &'static str| {
            h_flex()
                .id(id)
                .h(u(36.))
                .px(u(14.))
                .gap(u(6.))
                .items_center()
                .rounded(u(8.))
                .bg(fill)
                .text_color(on_fill)
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .hover(|d| d.opacity(0.9))
                .child(icon.size(u(18.)))
                .child(label)
        };
        let square = |id: &'static str| {
            div()
                .id(id)
                .size(u(36.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(8.))
                .bg(text.opacity(0.12))
                .text_color(text)
                .cursor_pointer()
                .hover(|d| d.bg(text.opacity(0.2)))
        };
        let store = self.services.store.clone();
        let message_icon = || Icon::new(gpui_kit::assets::IconName::MessageCircle);
        let row = h_flex().gap(u(8.)).items_center();
        let row = if friend {
            let who = name.to_string();
            row.child(wide("full-message", message_icon(), "Message").on_click(move |_, _, cx| {
                let _ = card.update(cx, |_, cx| cx.emit(ProfileEvent::Message(user)));
            }))
                .child(square("full-friend").child(Icon::default().path("icons/user-check.svg").size(u(18.))).on_click(move |event, _, cx| {
                    let (store, who) = (store.clone(), who.clone());
                    menu::show(event.position(), vec![Entry::item("Remove Friend", move |_, cx| {
                        let store = store.clone();
                        crate::confirm::ask(format!("Remove {who}"), format!("Remove {who} from your friends?"), "Remove Friend", move |cx| {
                            store.update(cx, |store, cx| store.remove_friend(user, cx));
                        }, cx);
                    }).danger()], cx);
                }))
        } else {
            row.child(wide("full-add-friend", Icon::new(gpui_kit::assets::IconName::UserPlus), "Add Friend").on_click(move |_, _, cx| {
                store.update(cx, |store, cx| store.add_friend(user, cx));
            }))
                .child(square("full-message").child(message_icon().size(u(18.))).on_click(move |_, _, cx| {
                    let _ = card.update(cx, |_, cx| cx.emit(ProfileEvent::Message(user)));
                }))
        };
        row.child(
            square("full-more")
                .child(h_flex().gap(u(3.)).children((0..3).map(|_| div().size(u(4.)).rounded_full().bg(text))))
                .on_click(move |event, _, cx| {
                    menu::show(
                        event.position(),
                        vec![
                            Entry::later("Share Profile"),
                            Entry::later("Shop This Look"),
                            Entry::later("Invite to Server"),
                            Entry::Separator,
                            Entry::later("Ignore"),
                            Entry::later("Block").danger(),
                            Entry::later("Report User Profile").danger(),
                            Entry::Separator,
                            Entry::copy("Copy User ID", user.to_string()).icon(gpui_kit::assets::IconName::Copy),
                        ],
                        cx,
                    );
                }),
        )
        .into_any_element()
    }

    fn full_view(&mut self, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let card_view = cx.entity().downgrade();
        const COLUMN: f32 = 420.;
        let theme = cx.theme();
        let store = self.services.store.read(cx);
        let cache = store.cache();
        let profile = match &self.load {
            Load::Loaded(p) => Some(p.clone()),
            _ => None,
        };
        let user: User = profile
            .as_ref()
            .map(|p| p.user.clone())
            .or_else(|| cache.user(self.user).cloned())
            .unwrap_or(User { id: self.user, username: "unknown-user".into(), global_name: None, avatar: None, bot: false, extras: Default::default() });
        let styled = cache.user(user.id).cloned().unwrap_or_else(|| user.clone());
        // A friend nickname is for lists: a profile says who they say they are.
        let own_name = |u: &User| if self.guild.is_some() { cache.display_name(self.guild, u).to_string() } else { u.display_name().to_string() };
        let name = profile.as_ref().and_then(|p| p.nick.clone()).unwrap_or_else(|| own_name(&user));
        let full_colors = self.guild.map(|g| cache.member_colors(g, self.user)).unwrap_or_default();
        let full_source = profile.as_ref().map(|p| &p.user).filter(|u| u.extras.display_name_styles.is_some()).unwrap_or(&styled);
        let full_look = crate::badges::name_look(full_source, &full_colors);
        let font = full_look.font;
        let known = cache.presence(user.id);
        let listed = self.guild.and_then(|g| cache.member_list().filter(|m| m.guild_id == g)).and_then(|m| m.find(user.id));
        // No presence is how offline looks: Discord only sends them for who's online.
        let status = listed.map(|m| m.status).or(known.map(|p| p.status)).or(Some(cord_core::member_list::Status::Offline));
        let avatar_url = profile.as_ref().and_then(|p| p.guild_avatar_url(240)).unwrap_or_else(|| cache.avatar_url(self.guild, &user, 240));
        let themed = profile.as_ref().and_then(|p| p.theme_colors);
        let palette = themed.map(|(top, _)| ProfilePalette::of(top));
        let card_text = palette.map_or(theme.popover_foreground, |p| p.text);
        // The overlay is part of the picture, which gets dithered as a whole.
        let smooth_surface = themed.zip(palette).map(|((top, bottom), p)| (rgb_color(top).blend(p.overlay), rgb_color(bottom).blend(p.overlay)));
        let ring = match (themed, palette) {
            (Some((top, _)), Some(p)) => rgb_color(top).blend(p.overlay),
            _ => theme.popover,
        };
        let muted = palette.map_or(theme.muted_foreground, |p| p.muted);
        let accent = profile.as_ref().and_then(|p| p.accent_color).filter(|_| themed.is_none()).or(themed.map(|t| t.0));
        let banner_url = profile.as_ref().and_then(|p| p.banner_url(1024));
        let banner = div().w(u(COLUMN)).h(u(150.)).flex_shrink_0().overflow_hidden().rounded_t(u(12.)).bg(accent.map_or(theme.primary.opacity(0.35), rgb_color)).children(
            banner_url.map(|url| crate::anim::hover_banner(format!("full-banner-{}", self.opening), url, 12.)),
        );
        // A heading over a line of text, as the left column stacks them.
        let fact = |title: &str, value: String| {
            v_flex()
                .gap(u(4.))
                .child(div().text_size(u(12.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child(title.to_string()))
                .child(div().text_size(u(14.)).child(value))
        };
        let date = |iso: &str| chrono::DateTime::parse_from_rfc3339(iso).ok().map(|t| t.format("%b %-d, %Y").to_string());
        let created = chrono::DateTime::from_timestamp_millis(user.id.timestamp_ms() as i64).map(|t| t.format("%b %-d, %Y").to_string());
        let joined = profile.as_ref().and_then(|p| p.joined_at.as_deref()).and_then(&date);
        let friends = cache.friends_since(user.id).and_then(&date);
        let bio = profile.as_ref().and_then(|p| p.bio.clone()).map(|bio| {
            let parsed = self.bio_cache.borrow_mut().parse(&bio);
            let shared = no_op_shared(self.bio_cache.clone());
            let message = placeholder_message();
            let env = crate::markdown_view::Env { shared: &shared, cache, message: &message, guild: self.guild };
            div().w_full().min_w_0().text_size(u(14.)).line_height(u(18.)).child(crate::markdown_view::blocks(&env, &parsed, "bio", cx))
        });
        let is_me = cache.me().is_some_and(|me| me.id == user.id);
        let (user_id, blurple) = (user.id, Hsla::from(gpui_kit::rgb(0x5865f2)));
        let display_for_menu = user.display_name().to_string();
        let member_roles = self.guild.and_then(|g| cache.member(g, self.user)).map(|m| m.roles.clone()).unwrap_or_default();
        let role_list = self.guild.and_then(|g| Some(role_chips(g, &cache.guild(g)?.roles, &member_roles))).unwrap_or_default();
        // The window around it wears the theme too: a little further under its overlay.
        let modal_surface = smooth_surface.zip(palette).map(|((top, bottom), p)| (top.blend(p.overlay.opacity(0.35)), bottom.blend(p.overlay.opacity(0.35))));
        let left = v_flex()
            .id("full-left")
            .flex_shrink_0()
            .w(u(COLUMN))
            .h_full()
            .relative()
            .rounded(u(12.))
            .when(palette.is_none(), |d| d.bg(theme.popover))
            .text_color(palette.map_or(theme.popover_foreground, |p| p.text))
            .children(smooth_surface.map(|(top, bottom)| crate::gradient::fill(top, bottom, 12.)))
            .child(self.left_scroller.layer())
            .child(
                v_flex()
                    .id("full-left-scroll")
                    .size_full()
                    .relative()
                    .overflow_y_scroll()
                    .track_scroll(&self.left_scroll)
                    .child(banner)
            .child(
                v_flex()
                    .flex_1()
                    .px(u(20.))
                    .pb(u(20.))
                    .gap(u(16.))
                    .child(div().h(u(72.)).child(
                        div().mt(u(-72.)).size(u(136.)).p(u(8.)).rounded(u(68.)).bg(ring).child(avatar_with_status(
                            Avatar::new().border_0().src(avatar_url).name(name.clone()),
                            120.,
                            status,
                            false,
                            styled.decoration_url_at(480),
                            ring,
                        )),
                    ))
                    .child(
                        v_flex()
                            .gap(u(4.))
                            .child(
                                h_flex()
                                    .gap(u(8.))
                                    .items_center()
                                    .child(div().text_size(u(24.)).line_height(u(30.)).font_weight(FontWeight::BOLD).text_color(full_look.color.unwrap_or(card_text)).when_some(font, |d, f| d.font_family(f)).child(crate::badges::name_effect(crate::badges::name_text("full-profile-name", &name, &full_look.stops, true), &name, &full_look, true)))
                                    .when(user.bot, |d| d.child(crate::badges::app_badge())),
                            )
                            .child(
                                h_flex()
                                    .flex_wrap()
                                    .gap(u(6.))
                                    .items_center()
                                    .text_size(u(14.))
                                    .child(match profile.as_ref().and_then(|p| p.pronouns.clone()) {
                                        Some(p) => format!("{} \u{2022} {p}", user.handle()),
                                        None => user.handle(),
                                    })
                                    .when_some(styled.server_tag(), |d, (tag, badge)| d.child(crate::badges::tag_pill_in("full-tag", tag, badge.as_deref(), card_text))),
                            )
                            .when_some(profile.as_ref().filter(|p| !p.badges.is_empty()), |d, p| {
                                d.child(h_flex().mt(u(4.)).flex_wrap().gap(u(2.)).children(p.badges.iter().enumerate().map(|(i, b)| {
                                    let tip = gpui_kit::SharedString::from(b.description.clone());
                                    div()
                                        .id(("full-badge", i))
                                        .size(u(24.))
                                        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip.clone()).build(window, cx))
                                        .child(img(b.icon_url()).size(u(22.)))
                                })))
                            }),
                    )
                    .when(!is_me, |d| d.child(self.full_actions(card_view.clone(), user_id, &display_for_menu, themed.map(|t| rgb_color(t.0)).unwrap_or(blurple), ring, card_text, cx)))
                    .when(matches!(self.load, Load::Private), |d| d.child(div().text_size(u(14.)).text_color(muted).child("This profile is private.")))
                    .children(bio)
                    .when_some(created, |d, v| d.child(fact("Member Since", v)))
                    .when_some(joined, |d, v| d.child(fact("Joined This Server", v)))
                    .when_some(friends, |d, v| d.child(fact("Friends Since", v)))
                    .when(!role_list.is_empty(), |d| {
                        let border = palette.map_or(theme.border, |p| p.text.opacity(0.2));
                        d.child(
                            v_flex()
                                .gap(u(6.))
                                .child(div().text_size(u(12.)).font_weight(FontWeight::SEMIBOLD).child("Roles"))
                                .child(h_flex().flex_wrap().gap(u(4.)).children(role_list.iter().map(|c| {
                                    h_flex()
                                        .gap(u(4.))
                                        .items_center()
                                        .h(u(24.))
                                        .px(u(8.))
                                        .rounded(u(12.))
                                        .border_1()
                                        .border_color(border)
                                        .text_size(u(12.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .child(div().size(u(12.)).flex_shrink_0().rounded_full().bg(if c.color == 0 { theme.muted_foreground } else { rgb_color(c.color) }))
                                        .child(c.name.clone())
                                }))),
                        )
                    })
                    .when_some(profile.as_ref().filter(|p| !p.connections.is_empty()), |d, p| {
                        d.child(
                            v_flex()
                                .gap(u(6.))
                                .child(div().text_size(u(12.)).font_weight(FontWeight::SEMIBOLD).text_color(muted).child("Connections"))
                                .children(p.connections.iter().map(|(kind, name)| {
                                    h_flex()
                                        .gap(u(8.))
                                        .items_center()
                                        .text_size(u(14.))
                                        .child(match connection_icon(kind) {
                                            Some(url) => img(url).size(u(20.)).flex_shrink_0().with_fallback(|| gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Link).size(u(20.)).into_any_element()).into_any_element(),
                                            None => gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Link).size(u(20.)).flex_shrink_0().into_any_element(),
                                        })
                                        .child(name.clone())
                                })),
                        )
                    }),
                )
                .child(div().flex_shrink_0().h(u(4.))),
            );

        // Over the column, as on the card.
        let left = left.on_hover(crate::transition::track_pointer(self.effect_hover_id())).children(self.effect_layer(12.));

        let activity = self.services.store.read(cx).cache().presence(user_id).and_then(|p| p.activity.clone());
        // Also what's left when their game ended while looking at it.
        let tab = match self.tab {
            Some(Tab::Activity) | None if activity.is_some() => Tab::Activity,
            Some(Tab::Activity) | None => Tab::Friends,
            Some(tab) => tab,
        };
        let activity_icon = activity.as_ref().and_then(|a| a.image.clone().or_else(|| a.application_id.clone().and_then(|app| app_icon(app, self.services.core.clone(), cx))));
        let friends_list = profile.as_ref().filter(|_| !is_me).map(|p| p.mutual_friend_list.clone()).unwrap_or_default();
        let servers_list = profile.as_ref().filter(|_| !is_me).and_then(|p| p.mutual_guilds.clone()).unwrap_or_default();
        let (friend_count, server_count) = (friends_list.len(), servers_list.len());
        let tab_button = |id: &'static str, label: String, which: Tab, cx: &mut Context<Self>| {
            let on = tab == which;
            div()
                .id(id)
                .pb(u(10.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .relative()
                // A bar of its own, in the profile's text colour.
                .when(on, |d| d.child(div().absolute().bottom_0().left_0().right_0().h(u(2.)).bg(card_text)))
                .when(!on, |d| d.text_color(muted))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.tab = Some(which);
                    cx.notify();
                }))
                .child(label)
        };
        // Nobody has anything mutual with themselves.
        let tabs = if is_me {
            h_flex()
        } else {
            h_flex()
                .gap(u(24.))
                .when(activity.is_some(), |d| d.child(tab_button("tab-activity", "Activity".to_string(), Tab::Activity, cx)))
                .child(tab_button("tab-friends", format!("{friend_count} Mutual Friends"), Tab::Friends, cx))
                .child(tab_button("tab-servers", format!("{server_count} Mutual Servers"), Tab::Servers, cx))
        };
        let theme = cx.theme();
        let store = self.services.store.read(cx);
        let cache = store.cache();
        let hover = card_text.opacity(0.06);
        let rows: Vec<AnyElement> = match tab {
            Tab::Activity => activity
                .iter()
                .map(|a| {
                    let verb = match a.kind {
                        cord_core::member_list::ActivityKind::Listening => "Listening to",
                        cord_core::member_list::ActivityKind::Competing => "Competing in",
                        _ => "Playing",
                    };
                    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
                    h_flex()
                        .gap(u(12.))
                        .p(u(8.))
                        .items_center()
                        .child(match activity_icon.clone() {
                            Some(url) => img(url).size(u(64.)).flex_shrink_0().rounded(u(8.)).into_any_element(),
                            // No art anywhere: its first letter, in the profile's colours.
                            None => div()
                                .size(u(64.))
                                .flex_shrink_0()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(u(8.))
                                .bg(card_text.opacity(0.1))
                                .text_color(card_text)
                                .text_size(u(24.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(a.name.chars().next().map(|c| c.to_uppercase().to_string()).unwrap_or_default())
                                .into_any_element(),
                        })
                        .child(
                            v_flex()
                                .min_w_0()
                                .gap(u(2.))
                                .child(div().text_size(u(12.)).text_color(muted).child(verb))
                                .child(div().text_size(u(14.)).font_weight(FontWeight::SEMIBOLD).child(a.name.clone()))
                                .when_some(a.details.clone(), |d, v| d.child(div().text_size(u(13.)).child(v)))
                                .when_some(a.start_ms, |d, at| d.child(div().text_size(u(12.)).text_color(muted).child(crate::sidebar::elapsed_label(at, now)))),
                        )
                        .into_any_element()
                })
                .collect(),
            Tab::Friends => friends_list
                .iter()
                .map(|f| {
                    let id = f.id;
                    h_flex()
                        .id(gpui_kit::SharedString::from(format!("friend-{id}")))
                        .h(u(52.))
                        .flex_shrink_0()
                        .px(u(8.))
                        .gap(u(12.))
                        .items_center()
                        .rounded(u(8.))
                        .cursor_pointer()
                        .hover(move |d| d.bg(hover))
                        .on_click(cx.listener(move |_, _, _, cx| cx.emit(ProfileEvent::OpenFull(id))))
                        .child(Avatar::new().border_0().src(f.avatar_url.clone()).name(f.name.clone()).size(u(36.)))
                        .child(
                            v_flex()
                                .min_w_0()
                                .child(div().truncate().text_size(u(14.)).font_weight(FontWeight::MEDIUM).child(f.name.clone()))
                                .child(div().truncate().text_size(u(12.)).text_color(muted).child(f.username.clone())),
                        )
                        .into_any_element()
                })
                .collect(),
            Tab::Servers => servers_list
                .iter()
                .filter_map(|id| cache.guild(*id))
                .map(|g| {
                    let id = g.id;
                    let icon = g.icon.as_ref().map(|hash| cord_core::cdn::guild_icon_url(g.id, hash, 64));
                    h_flex()
                        .id(gpui_kit::SharedString::from(format!("mutual-{id}")))
                        .h(u(48.))
                        .px(u(8.))
                        .gap(u(12.))
                        .items_center()
                        .rounded(u(8.))
                        .cursor_pointer()
                        .hover(move |d| d.bg(hover))
                        .on_click(cx.listener(move |_, _, _, cx| {
                            cx.emit(ProfileEvent::Close);
                            crate::links::go(format!("/channels/{id}"), cx);
                        }))
                        .child(match icon {
                            Some(url) => img(url).size(u(36.)).rounded(u(10.)).into_any_element(),
                            None => div().size(u(36.)).rounded(u(10.)).bg(card_text.opacity(0.1)).into_any_element(),
                        })
                        .child(div().text_size(u(14.)).font_weight(FontWeight::MEDIUM).child(g.name.clone()))
                        .into_any_element()
                })
                .collect(),
        };
        // Only once their profile's here: until then, nobody knows.
        let nothing_mutual = rows.is_empty() && profile.is_some() && !is_me;
        let rows = if nothing_mutual {
            let line = if tab == Tab::Servers { "You have no mutual servers with this user" } else { "You have no mutual friends with this user" };
            vec![div().w_full().p(u(8.)).text_size(u(14.)).text_color(muted).child(line).into_any_element()]
        } else {
            rows
        };
        let viewport = window.viewport_size();
        let smooth = self.services.settings.read(cx).get().smooth_scrolling;
        let modal = h_flex()
            .id("profile-full")
            .occlude()
            .relative()
            .w(u(COLUMN + 520.))
            .max_w_full()
            .h(gpui_kit::relative(0.85))
            .p(u(16.))
            .gap(u(20.))
            .items_start()
            .rounded(u(16.))
            .border_1()
            .border_color(theme.border)
            .when(modal_surface.is_none(), |d| d.bg(theme.background))
            .children(modal_surface.map(|(top, bottom)| crate::gradient::fill(top, bottom, 12.)))
            .text_color(card_text)
            .shadow_lg()
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(left)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_w_0()
                    .h_full()
                    .child(
                        v_flex()
                            .id("full-right")
                            .size_full()
                            .pt(u(8.))
                            .gap(u(12.))
                            .overflow_y_scroll()
                            .track_scroll(&self.right_scroll)
                            .child(tabs)
                            .children(rows),
                    )
                    .when(smooth, |d| d.child(self.right_scroller.layer())),
            )
            .child(
                div()
                    .id("profile-full-close")
                    .absolute()
                    .top(u(12.))
                    .right(u(12.))
                    .size(u(32.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(u(8.))
                    .cursor_pointer()
                    .hover(move |d| d.bg(hover))
                    .on_click(cx.listener(|this, _, _, cx| this.begin_close(cx)))
                    .text_color(card_text)
                    .child(gpui_kit::component::Icon::new(gpui_kit::assets::IconName::Close).size(u(18.))),
            )
            .with_animation(
                ("profile-full-open", self.opening),
                gpui_kit::Animation::new(std::time::Duration::from_millis(200)).with_easing(gpui_kit::ease_out_quint()),
                |d, t| d.opacity(t).top(u(12. * (1. - t))),
            );
        deferred(
            anchored().position(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.))).child(
                div()
                    .id("profile-backdrop")
                    .track_focus(&self.focus)
                    .occlude()
                    .w(viewport.width)
                    .h(viewport.height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .bg(gpui_kit::black().opacity(0.7))
                    .when_some(self.closing_opacity(), |d, fade| d.opacity(fade).child(crate::transition::driver()))
                    .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                        if event.keystroke.key == "escape" {
                            this.begin_close(cx);
                        }
                    }))
                    .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| this.begin_close(cx)))
                    .child(modal),
            ),
        )
        .with_priority(1)
        .into_any_element()
    }
}


/// How many badges get shown and how many fold into "+N": all that fit in
/// the row, keeping room for the "+N" when some don't.
pub fn badges_shown(count: usize) -> (usize, usize) {
    const FIT: usize = 8;
    if count <= FIT { (count, 0) } else { (FIT - 2, count - (FIT - 2)) }
}

/// How many of `chips` fit in two rows of the card, by estimated pill widths.
fn roles_that_fit(chips: &[Chip]) -> usize {
    const ROOM: f32 = WIDTH - 32.;
    const RESERVE: f32 = 40.;
    let (mut row, mut used) = (0, 0f32);
    for (i, chip) in chips.iter().enumerate() {
        let width = 38. + chip.name.chars().count() as f32 * 6.6;
        if used + width > ROOM {
            row += 1;
            used = 0.;
        }
        // The last row keeps room for "+N" if anything comes after.
        let room = if row == 1 && i + 1 < chips.len() { ROOM - RESERVE } else { ROOM };
        if row > 1 || used + width > room {
            return i;
        }
        used += width + 4.;
    }
    chips.len()
}

/// "2 Mutual Friends • 5 Mutual Servers", leaving out the zeroes.
pub fn mutuals_line(friends: Option<u32>, servers: Option<usize>) -> String {
    let plural = |n: usize, one: &str| format!("{n} Mutual {one}{}", if n == 1 { "" } else { "s" });
    let parts: Vec<String> = [friends.map(|n| n as usize).filter(|n| *n > 0).map(|n| plural(n, "Friend")), servers.filter(|n| *n > 0).map(|n| plural(n, "Server"))]
        .into_iter()
        .flatten()
        .collect();
    parts.join(" \u{2022} ")
}

/// Markdown in a bio has no message around it: mentions and spoilers there
/// do nothing when clicked.
fn no_op_shared(parsed: Rc<RefCell<crate::markdown_view::ParseCache>>) -> crate::markdown_view::Shared {
    crate::markdown_view::Shared {
        poll_ui: Rc::default(),
        in_card: false,
        inert: false,
        emoji_cap: None,
        remeasure: Rc::new(|_, _| {}),
        vote: Rc::new(|_, _, _| {}),
        revealed: Rc::default(),
        reveal: Rc::new(|_, _, _| {}),
        jump: Rc::new(|_, _| {}),
        react: Rc::new(|_, _, _, _| {}),
        open_user: Rc::new(|_, _, _, _| {}),
        who_reacted: Rc::new(|_, _| {}),
        text_size: 14.,
        parsed,
    }
}

fn placeholder_message() -> cord_core::model::Message {
    serde_json::from_value(serde_json::json!({
        "id": "0", "channel_id": "0", "content": "", "timestamp": "2015-05-13T00:00:00+00:00", "author": {"id": "0", "username": ""}
    }))
    .expect("a minimal message decodes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_button_stays_visible_on_any_profile() {
        let (near_white, near_black) = (rgb_color(0xf5f5f5), rgb_color(0x101010));
        // Its colour is the card's: takes the text colour instead, with a readable label.
        let (fill, label) = button_colors(rgb_color(0xffffff), near_white, near_black);
        assert_eq!(fill, near_black);
        assert_eq!(label, gpui_kit::white());
        // A light colour on a dark card keeps its colour, with a dark label.
        let (fill, label) = button_colors(near_white, near_black, near_white);
        assert_eq!(fill, near_white);
        assert_eq!(label, gpui_kit::black());
        for (main, surface, text) in [(0xffffff, 0xf5f5f5, 0x101010), (0x5865f2, 0x101010, 0xf5f5f5), (0xffe000, 0x202020, 0xffffff)] {
            let (fill, label) = button_colors(rgb_color(main), rgb_color(surface), rgb_color(text));
            assert!(contrast(fill, label) >= 4.5, "{main:x}");
        }
    }

    fn role(id: u64, name: &str, color: u32, position: i32) -> Role {
        Role { id: Id(id), name: name.into(), color, position, permissions: 0, hoist: false, icon: None, unicode_emoji: None, colors: None, mentionable: false }
    }

    #[test]
    fn chips_are_the_members_roles_highest_first() {
        let roles = [role(300, "@everyone", 0, 0), role(1, "Mod", 0xff0000, 5), role(2, "Fan", 0, 1), role(3, "Other", 0, 9)];
        let chips = role_chips(Id(300), &roles, &[Id(2), Id(1), Id(300)]);
        assert_eq!(
            chips,
            [Chip { id: Id(1), name: "Mod".into(), color: 0xff0000, colors: vec![0xff0000] }, Chip { id: Id(2), name: "Fan".into(), color: 0, colors: vec![] }],
            "no @everyone, no roles they lack"
        );
        assert!(role_chips(Id(300), &roles, &[]).is_empty());
    }

    #[test]
    fn the_bios_stand_in_message_decodes() {
        assert_eq!(placeholder_message().content, "");
    }

    #[test]
    fn mutuals_skip_zeroes() {
        assert_eq!(mutuals_line(Some(2), Some(1)), "2 Mutual Friends \u{2022} 1 Mutual Server");
        assert_eq!(mutuals_line(Some(0), Some(3)), "3 Mutual Servers");
        assert_eq!(mutuals_line(None, None), "");
    }

    #[test]
    fn extra_badges_fold() {
        assert_eq!(badges_shown(6), (6, 0));
        assert_eq!(badges_shown(9), (6, 3));
    }

    #[test]
    fn message_button_names_the_user() {
        assert_eq!(message_label("lin"), "Message @lin");
    }
}
