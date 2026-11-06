//! The window's root view: the current page, a status bar that appears only
//! while there is something to say, notifications, and the Ctrl+I info panel.

use std::time::Instant;

use gpui_kit::component::{ActiveTheme, WindowExt, h_flex, notification::Notification, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription, Window, div, rems};

use cord_core::model::ChannelKind;
use cord_core::event::ConnectionState;
use cord_core::cdn;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::assets::IconName;
use gpui_kit::component::Icon;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::{AnyElement, FontWeight, Hsla, InteractiveElement, ObjectFit, ScrollHandle, SharedString, StatefulInteractiveElement, StyledImage, img};

use gpui_fps::FpsMonitor;
use gpui_kit::AnimationExt;

use crate::ui::u;
use crate::nav::{self, Location};
use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::router::{Router, navigate};
use crate::sidebar::{self, Collapsed};
use crate::store::{Services, StoreEvent};
use crate::toasts::{Toast, ToastKind};

gpui_kit::actions!(rustcord, [ToggleInfo, OpenSettings]);

pub struct Shell {
    /// The channel column's edge is being dragged.
    dragging_column: bool,
    router: Entity<Router>,
    pictures: Entity<crate::anim::PictureCache>,
    /// The place opened at start is still there: looked at once the data's in.
    start_checked: bool,
    started: Instant,
    /// The Ctrl+I panel's performance monitor; only exists while it's open,
    /// so nothing is sampled otherwise.
    info: Option<Entity<FpsMonitor>>,
    services: Services,
    /// The rail, channel column and user panel: their own view, drawn
    /// cached, so scrolling the chat doesn't rebuild them every frame.
    side: Entity<SidePane>,
    _subscriptions: Vec<Subscription>,
}

/// The server rail, the channel column and the user panel floating over
/// both. A view of its own so GPUI can reuse its last frame while only the
/// chat changes.
pub struct SidePane {
    router: Entity<Router>,
    services: Services,
    rail_scroll: ScrollHandle,
    rail_scroller: SmoothScroller,
    channels_scroll: ScrollHandle,
    channels_scroller: SmoothScroller,
    /// What the list was last drawn from: only then are its rows' places known.
    drawn_list: std::cell::Cell<Option<sidebar::SidebarKey>>,
    /// The channel whose row was last brought into view, and frames left until that's due.
    revealed_channel: std::cell::Cell<Option<cord_core::Id>>,
    reveal_wait: std::cell::Cell<u8>,
    /// The DM last open, for the Home button to go back to.
    last_dm: std::cell::Cell<Option<cord_core::Id>>,
    /// The channel each server was last left on.
    last_in_guild: std::cell::RefCell<std::collections::HashMap<cord_core::Id, cord_core::Id>>,
    /// The server icon under the pointer, and where each icon is, for the hover card.
    rail_hover: std::cell::RefCell<Option<SharedString>>,
    rail_bounds: std::rc::Rc<std::cell::RefCell<std::collections::HashMap<SharedString, (gpui_kit::Bounds<gpui_kit::Pixels>, SharedString)>>>,
    /// Servers whose voice statuses and timers have been asked for.
    voice_info_asked: std::collections::HashSet<cord_core::Id>,
    /// Which list the channel column last showed (a server, or None for DMs)
    /// and how far each was scrolled: each keeps its own place.
    list_shown: Option<Option<cord_core::Id>>,
    list_offsets: std::collections::HashMap<Option<cord_core::Id>, gpui_kit::Point<gpui_kit::Pixels>>,
    /// Categories folded in the channel list, for this session.
    collapsed: Collapsed,
    /// What the rail and channel column draw, rebuilt only when the cache's
    /// sidebar revision, the location or a fold changes.
    sidebar: Option<sidebar::Sidebar>,
    asked_voice_members: std::collections::HashSet<(cord_core::Id, cord_core::Id)>,
    /// Server folders opened in the rail, for this session.
    open_folders: std::collections::HashSet<i64>,
    _subscriptions: Vec<Subscription>,
}

impl SidePane {
    fn new(router: Entity<Router>, services: Services, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&router, |_, _, cx| cx.notify()),
            cx.observe(&services.store, |_, _, cx| cx.notify()),
            cx.observe(&services.settings, |_, _, cx| cx.notify()),
        ];
        let (rail_scroll, channels_scroll) = (ScrollHandle::new(), ScrollHandle::new());
        SidePane {
            router,
            services,
            rail_scroller: SmoothScroller::new(ScrollTarget::Handle(rail_scroll.clone())),
            rail_scroll,
            channels_scroller: SmoothScroller::new(ScrollTarget::Handle(channels_scroll.clone())).calm_hover().layered(),
            drawn_list: std::cell::Cell::new(None),
            revealed_channel: Default::default(),
            reveal_wait: Default::default(),
            last_dm: std::cell::Cell::new(crate::places::get().dm),
            last_in_guild: std::cell::RefCell::new(crate::places::get().guilds),
            rail_hover: Default::default(),
            rail_bounds: Default::default(),
            channels_scroll,
            list_shown: None,
            voice_info_asked: Default::default(),
            list_offsets: Default::default(),
            collapsed: Collapsed::default(),
            sidebar: None,
            asked_voice_members: Default::default(),
            open_folders: Default::default(),
            _subscriptions: subscriptions,
        }
    }

    /// The server shown at `location`: name and icon, for the top bar.
    fn guild_header(&self, location: Location) -> Option<(String, Option<String>)> {
        let g = self.sidebar.as_ref()?.guilds.iter().find(|g| Some(g.id) == location.guild)?;
        Some((g.name.clone(), g.icon.as_ref().map(|hash| cdn::guild_icon_url(g.id, hash, 32))))
    }

    /// Rebuilds the cached sidebar if what it is made from changed.
    fn refresh_sidebar(&mut self, location: Location, cx: &Context<Self>) {
        let cache = self.services.store.read(cx).cache();
        let key = sidebar::key(cache, location, &self.collapsed);
        if self.sidebar.as_ref().is_none_or(|s| s.key != key) {
            let built = sidebar::build(cache, location, &self.collapsed);
            // Server profiles of whoever's in voice: nothing else may have brought them.
            if let Some(guild) = location.guild {
                let missing = built.rows.iter().filter_map(|row| match row {
                    sidebar::Row::Participant(p) if cache.member(guild, p.user_id).is_none() => Some(p.user_id),
                    _ => None,
                });
                let new: Vec<_> = missing.filter(|user| self.asked_voice_members.insert((guild, *user))).collect();
                if !new.is_empty() {
                    self.services.store.read(cx).core().request_members(guild, &new);
                }
            }
            self.sidebar = Some(built);
        }
    }

    /// Beside the rail icon under the pointer: its name, and for a server its
    /// icon and what's new.
    fn rail_card(&self, guilds: &[sidebar::RailGuild], cx: &Context<Self>) -> Option<AnyElement> {
        let hovered = self.rail_hover.borrow().clone()?;
        let (at, label) = self.rail_bounds.borrow().get(&hovered)?.clone();
        let guild = guilds.iter().find(|g| hovered == format!("guild-{}", g.id));
        let theme = cx.theme();
        let icon = guild.and_then(|g| Some(img(cdn::guild_icon_url(g.id, g.icon.as_ref()?, 48)).size(u(20.)).flex_shrink_0().rounded(u(6.))));
        let position = gpui_kit::point(at.right() + gpui_kit::px(10.), at.top());
        Some(
            gpui_kit::deferred(
                gpui_kit::anchored().position(position).snap_to_window_with_margin(gpui_kit::px(8.)).child(
                    v_flex()
                        .max_w(u(280.))
                        .px(u(12.))
                        .py(u(8.))
                        .gap(u(2.))
                        .rounded(u(8.))
                        .border_1()
                        .border_color(theme.border)
                        .bg(theme.popover)
                        .shadow_lg()
                        .child(
                            h_flex()
                                .gap(u(8.))
                                .items_center()
                                .children(icon)
                                .child(div().min_w_0().truncate().text_size(u(15.)).font_weight(FontWeight::SEMIBOLD).text_color(theme.foreground).child(label)),
                        )
                        .when_some(guild.and_then(|g| g.activity.clone()), |d, activity| {
                            d.child(div().min_w_0().truncate().text_size(u(13.)).text_color(theme.muted_foreground).child(activity))
                        })
                        .with_animation(
                            SharedString::from(format!("rail-tip-{hovered}")),
                            gpui_kit::Animation::new(std::time::Duration::from_millis(150)).with_easing(crate::transition::ease),
                            |d, t| d.opacity(t),
                        ),
                ),
            )
            .with_priority(2)
            .into_any_element(),
        )
    }

    fn rail(&self, location: Location, cx: &Context<Self>) -> impl IntoElement {
        let smooth = self.services.settings.read(cx).get().smooth_scrolling;
        let guild_list = self.sidebar.as_ref().map(|s| s.guilds.as_slice()).unwrap_or_default();
        let theme = cx.theme();
        let pill_color = theme.sidebar_accent_foreground;
        let (idle_bg, idle_fg) = (raised(theme), theme.sidebar_accent_foreground);
        let (blurple, on_blurple) = (theme.primary, theme.primary_foreground);
        let green = Hsla::from(gpui_kit::rgb(GREEN));

        // Discord's shape in this theme: rounded squares, always. GPUI clips
        // children to rectangles, so images round themselves.
        let radius = u(RAIL_SQUIRCLE);
        // The white pill on the rail's left edge: tall when active, half as
        // tall while hovered (Discord's unread state would be 8px).
        let pill = |lift: f32, active: bool, unread: bool| {
            div().absolute().left(u(-RAIL_PAD)).top_0().h_full().flex().items_center().child(
                div()
                    .w(u(4.))
                    .h(u(if active {
                        PILL_ACTIVE
                    } else {
                        let rest = if unread { PILL_UNREAD } else { 0. };
                        rest + (PILL_HOVER - rest) * lift
                    }))
                    .rounded_tr(u(4.))
                    .rounded_br(u(4.))
                    .bg(pill_color)
            )
        };
        let sidebar_bg = theme.sidebar;
        let danger = theme.danger;
        // Discord's ping count: a red pill on the icon's bottom-right corner,
        // cut out of the rail by a ring of the rail's colour.
        let badge = move |count: u32| {
            div()
                .absolute()
                .right(u(-4.))
                .bottom(u(-4.))
                .min_w(u(16.))
                .h(u(16.))
                .px(u(4.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .border_3()
                .border_color(sidebar_bg)
                .bg(danger)
                .text_color(gpui_kit::white())
                .text_size(u(12.))
                .font_weight(FontWeight::BOLD)
                .child(if count > 99 { "99+".to_string() } else { count.to_string() })
        };
        // A hook for later: an unread count (a red pill at the bottom-right) is
        // another child of the item once the cache tracks mentions.
        let item = |id: SharedString,
                    active: bool,
                    unread: bool,
                    mentions: u32,
                    tooltip: SharedString,
                    colors: (Hsla, Hsla, Hsla, Hsla),
                    content: AnyElement| {
            let (bg, fg, hover_bg, hover_fg) = colors;
            let hovered = self.rail_hover.borrow().as_ref() == Some(&id);
            let (lift, lifting) = crate::transition::progress(&SharedString::from(format!("pill-{id}")), hovered);
            let key = id.clone();
            let place = (self.rail_bounds.clone(), id.clone(), tooltip);
            div()
                .id(id)
                .on_hover(cx.listener(move |this, over: &bool, _, cx| {
                    let now = if *over {
                        Some(key.clone())
                    } else if this.rail_hover.borrow().as_ref() == Some(&key) {
                        None
                    } else {
                        return;
                    };
                    this.rail_hover.replace(now);
                    cx.notify();
                }))
                .child(
                    gpui_kit::canvas(
                        move |at, _, _| {
                            place.0.borrow_mut().insert(place.1.clone(), (at, place.2.clone()));
                        },
                        |_, _, _, _| {},
                    )
                    .absolute()
                    .top_0()
                    .left_0()
                    .size_full(),
                )
                .relative()
                .flex_shrink_0()
                .size(u(RAIL_ICON))
                .flex()
                .items_center()
                .justify_center()
                .rounded(radius)
                .hover(|style| style.bg(hover_bg).text_color(hover_fg))
                .bg(if active { blurple } else { bg })
                .text_color(if active { on_blurple } else { fg })
                .cursor_pointer()
                .child(pill(lift, active, unread))
                .when(lifting, |d| d.child(crate::transition::driver()))
                .child(content)
                .when(mentions > 0, |d| d.child(badge(mentions)))
        };
        let idle = (idle_bg, idle_fg, blurple, on_blurple);
        // Folders are tinted, often with this very blurple: hover can't turn them it.
        let folder_idle = (idle_bg, idle_fg, theme.sidebar_accent, idle_fg);
        let clear = gpui_kit::transparent_black();

        let router = self.router.clone();
        crate::places::note(location.guild, location.channel);
        if location.guild.is_none() && location.channel.is_some() {
            self.last_dm.set(location.channel);
        }
        // One that's since been left or closed for good isn't anywhere to go back to.
        let last_dm = self.last_dm.get().filter(|dm| self.services.store.read(cx).cache().channel(*dm).is_some());
        let home_target = last_dm.map_or_else(|| "/channels/@me".to_string(), |dm| format!("/channels/@me/{dm}"));
        let home = item(
            "home".into(),
            location.guild.is_none(),
            false,
            0,
            "Direct Messages".into(),
            idle,
            Icon::new(IconName::MessageCircle).size(u(24.)).into_any_element(),
        )
        .on_click(move |_, _, cx| navigate(&router, home_target.clone(), cx));

        let dm_alerts: Vec<_> = self.sidebar.iter().flat_map(|s| &s.dm_alerts).map(|dm| {
            let (router, target) = (self.router.clone(), dm.target.clone());
            item(
                SharedString::from(format!("dm-alert-{}", dm.id)),
                false,
                true,
                dm.mentions,
                dm.name.clone().into(),
                (clear, idle_fg, clear, idle_fg),
                if dm.faces.len() == 2 {
                    stacked_faces(&dm.faces, RAIL_ICON, theme.sidebar)
                } else {
                    img(dm.picture.clone()).size(u(RAIL_ICON)).object_fit(ObjectFit::Cover).rounded_full().into_any_element()
                },
            )
            .on_click(move |_, _, cx| navigate(&router, target.clone(), cx))
            // Fades in as it arrives; once per DM.
            .with_animation(
                SharedString::from(format!("dm-alert-in-{}", dm.id)),
                gpui_kit::Animation::new(std::time::Duration::from_millis(256)).with_easing(crate::transition::ease),
                |d, t| d.opacity(t),
            )
        }).collect();

        let guild_icon = |guild: &sidebar::RailGuild, size: f32| -> AnyElement {
            match &guild.icon {
                Some(hash) => img(cdn::guild_icon_url(guild.id, hash, 96))
                    .size(u(size))
                    .object_fit(ObjectFit::Cover)
                    .rounded(u(size * RAIL_SQUIRCLE / RAIL_ICON))
                    .into_any_element(),
                None => div()
                    .size(u(size))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(u(size * RAIL_SQUIRCLE / RAIL_ICON))
                    .bg(idle_bg)
                    .text_size(u(if size < RAIL_ICON { 7. } else { 14. }))
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(initials(&guild.name))
                    .into_any_element(),
            }
        };
        // Coming back to a server lands on the channel you left it on.
        if let (Some(guild), Some(channel)) = (location.guild, location.channel) {
            self.last_in_guild.borrow_mut().insert(guild, channel);
        }
        let guild_item = |guild: &sidebar::RailGuild| {
            let selected = location.guild == Some(guild.id);
            let cache = self.services.store.read(cx).cache();
            let target = self
                .last_in_guild
                .borrow()
                .get(&guild.id)
                .filter(|channel| cache.channel(**channel).is_some())
                .map_or_else(|| guild.target.clone(), |channel| format!("/channels/{}/{channel}", guild.id));
            let router = self.router.clone();
            item(
                SharedString::from(format!("guild-{}", guild.id)),
                selected,
                guild.unread,
                guild.mentions,
                guild.name.clone().into(),
                idle,
                guild_icon(guild, RAIL_ICON),
            )
            .on_click(move |_, _, cx| navigate(&router, target.clone(), cx))
            .on_mouse_down(gpui_kit::MouseButton::Right, {
                let (store, id) = (self.services.store.clone(), guild.id);
                move |event: &gpui_kit::MouseDownEvent, _, cx: &mut gpui_kit::App| {
                    cx.stop_propagation();
                    let entries = server_menu(id, &store, cx);
                    crate::context_menu::show(event.position, entries, cx);
                }
            })
        };
        let (rail_entries, folders) = match &self.sidebar {
            Some(s) => (s.rail.as_slice(), s.folders.as_slice()),
            None => (Default::default(), Default::default()),
        };
        let guilds = rail_entries.iter().map(|entry| match entry {
            sidebar::RailEntry::Guild(i) => guild_item(&guild_list[*i]).into_any_element(),
            sidebar::RailEntry::Folder(f) => {
                let folder = &folders[*f];
                let members: Vec<&sidebar::RailGuild> = folder.guilds.iter().map(|i| &guild_list[*i]).collect();
                let open = self.open_folders.contains(&folder.id);
                let color = folder.color.map(|c| Hsla::from(gpui_kit::rgb(c)));
                let tint = color.unwrap_or(blurple);
                let id = folder.id;
                let folder_menu_opener = {
                    let (store, pane) = (self.services.store.clone(), cx.entity().downgrade());
                    let guilds: Vec<cord_core::Id> = members.iter().map(|g| g.id).collect();
                    move |event: &gpui_kit::MouseDownEvent, _: &mut Window, cx: &mut gpui_kit::App| {
                        cx.stop_propagation();
                        let entries = folder_menu(guilds.clone(), &store, pane.clone(), cx);
                        crate::context_menu::show(event.position, entries, cx);
                    }
                };
                let toggle = cx.listener(move |this, _, _, cx| {
                    if !this.open_folders.remove(&id) {
                        this.open_folders.insert(id);
                    }
                    cx.notify();
                });
                let name: SharedString = folder.name.clone().unwrap_or_else(|| {
                    members.iter().map(|g| g.name.as_str()).collect::<Vec<_>>().join(", ")
                }).into();
                let selected = members.iter().any(|g| location.guild == Some(g.id));
                let (opening, sliding) = crate::transition::progress(&SharedString::from(format!("folder-open-{id}")), open);
                if open || sliding {
                    // Open: the folder glyph on top, its servers below, on a tinted strip.
                    v_flex()
                        .flex_shrink_0()
                        .w(u(RAIL_ICON))
                        .gap(u(RAIL_GAP))
                        .rounded(radius)
                        .bg(color.map_or(idle_bg, |c| c.opacity(0.2)))
                        .child(
                            item(SharedString::from(format!("folder-{id}")), false, false, 0, name, folder_idle, {
                                Icon::new(IconName::Folder).size(u(20.)).text_color(tint).into_any_element()
                            })
                            .on_click(toggle)
                            .on_mouse_down(gpui_kit::MouseButton::Right, folder_menu_opener.clone()),
                        )
                        // Slides open and shut. Only clipped while moving: at rest,
                        // the servers' pills hang outside this strip.
                        .child(
                            v_flex()
                                .flex_shrink_0()
                                .gap(u(RAIL_GAP))
                                .when(sliding, |d| {
                                    let n = members.len() as f32;
                                    let full = n * RAIL_ICON + (n - 1.).max(0.) * RAIL_GAP;
                                    d.h(u(full * opening)).overflow_hidden()
                                })
                                .children(members.iter().map(|g| guild_item(g))),
                        )
                        .when(sliding, |d| d.child(crate::transition::driver()))
                        .into_any_element()
                } else {
                    let unread = members.iter().any(|g| g.unread);
                    let mentions = members.iter().map(|g| g.mentions).sum();
                    // Closed: up to four servers in a 2×2 grid.
                    // Rows of their own, not wrapping: at some zooms rounding made two icons
                    // not quite fit on a line, which pushed the rest out of the square.
                    let shown: Vec<_> = members.iter().take(4).collect();
                    let grid = v_flex()
                        .size_full()
                        .p(u(4.))
                        .gap(u(2.))
                        .overflow_hidden()
                        .rounded(radius)
                        .bg(color.map_or(idle_bg, |c| c.opacity(0.4)))
                        .children(shown.chunks(2).map(|pair| h_flex().flex_shrink_0().gap(u(2.)).children(pair.iter().map(|g| div().flex_shrink_0().child(guild_icon(g, 15.))))));
                    item(SharedString::from(format!("folder-{id}")), selected, unread, mentions, name, folder_idle, grid.into_any_element())
                        .on_click(toggle)
                        .on_mouse_down(gpui_kit::MouseButton::Right, folder_menu_opener.clone())
                        .into_any_element()
                }
            }
        });

        // Inert for now; white icons that turn the tile green on hover, as Discord does.
        let action = |id: &'static str, tooltip: &'static str, icon: IconName| {
            item(
                id.into(),
                false,
                false,
                0,
                tooltip.into(),
                (idle_bg, idle_fg, green, on_blurple),
                Icon::new(icon).size(u(21.)).into_any_element(),
            )
        };

        div()
            .relative()
            .w(u(RAIL_WIDTH))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .child(
                v_flex()
                    .id("rail")
                    .size_full()
                    .items_start()
                    .pl(u(RAIL_PAD))
                    .gap(u(RAIL_GAP))
                    .pt_3()
                    .pb(u(PANEL_HEIGHT + 16.))
                    .overflow_y_scroll()
                    .track_scroll(&self.rail_scroll)
                    .child(home)
                    .children(dm_alerts)
                    .child(div().ml(u(8.)).w(u(24.)).h(u(2.)).rounded(u(1.)).bg(theme.border))
                    .children(guilds)
                    .child(action("add-server", "Add a Server", IconName::Plus))
                    .child(action("discover", "Discover", IconName::Compass)),
            )
            .when(smooth, |this| this.child(self.rail_scroller.layer()))
            .children(self.rail_card(guild_list, cx))
    }

    fn channel_column(&self, location: Location, cx: &Context<Self>) -> impl IntoElement {
        let smooth = self.services.settings.read(cx).get().smooth_scrolling;
        let theme = cx.theme();
        let (muted, bright) = (theme.sidebar_foreground, theme.sidebar_accent_foreground);
        let hover_bg = theme.sidebar_accent.opacity(0.6);
        // Small inert icon buttons, as on the selected channel and category rows.
        let mini = |id: SharedString, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .flex_shrink_0()
                .cursor_pointer()
                .text_color(theme.sidebar_foreground)
                .hover(|this| this.text_color(bright))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .on_click(|_, _, cx| cx.stop_propagation())
                .child(Icon::new(icon).size(u(16.)))
        };

        let row_shell = |id: SharedString, active: bool, clickable: bool, path: String| {
            let router = self.router.clone();
            let (fade_id, list_view) = (id.clone(), cx.entity_id());
            h_flex()
                .id(id)
                .flex_shrink_0()
                .ml(u(ROW_LEFT))
                .mr(u(ROW_RIGHT))
                .px(u(9.5))
                .gap(u(9.5))
                .items_center()
                .rounded(u(5.))
                .when(active, |this| this.bg(theme.sidebar_accent).text_color(bright))
                .when(!active, |this| this.text_color(muted))
                .when(clickable, |this| {
                    this.cursor_pointer()
                        .when(!active && !crate::smooth_scroll::list_scrolling(), |this| this.hover(|this| this.bg(hover_bg).text_color(bright)))
                        // Only this list redraws, and not at all for a row just passing by.
                        .on_hover(move |over: &bool, _: &mut Window, cx: &mut gpui_kit::App| {
                            crate::transition::set_pointer(&fade_id, *over);
                            if !crate::smooth_scroll::list_scrolling() {
                                cx.notify(list_view);
                            }
                        })
                        .on_click(move |_, _, cx| navigate(&router, path.clone(), cx))
                })
        };

        let channel_row = |channel: &sidebar::ChannelRow| {
            let active = location.channel == Some(channel.id);
            let group = SharedString::from(format!("channel-row-{}", channel.id));
            // Invite and settings: always on the selected row, on hover elsewhere.
            let voice = matches!(channel.kind, ChannelKind::Voice | ChannelKind::Stage);
            let can_join = location.guild.is_some() && self.services.store.read(cx).cache().has_permission(channel.id, cord_core::permissions::CONNECT);
            let actions = h_flex()
                .flex_shrink_0()
                .gap(u(6.5))
                .when(voice, |this| {
                    let (router, path) = (self.router.clone(), channel.path.clone());
                    this.child(
                        mini(SharedString::from(format!("chat-{}", channel.id)), IconName::MessageCircle, "Open Chat")
                            .on_click(move |_, _, cx| navigate(&router, path.clone(), cx)),
                    )
                })
                .when(channel.can_invite, |this| this.child(mini(SharedString::from(format!("invite-{}", channel.id)), IconName::UsersRound, "Create Invite")))
                .when(channel.can_edit, |this| this.child(mini(SharedString::from(format!("edit-{}", channel.id)), IconName::Settings, "Edit Channel")));
            let has_actions = voice || channel.can_invite || channel.can_edit;
            // A clock or a ping sits at the row's end, even on the selected row:
            // the buttons take its place on hover, floating over the row
            // instead of holding empty space beside it.
            let floating = channel.started.is_some() || channel.mentions > 0;
            let row_bg = if active { theme.sidebar_accent } else { hover_bg };
            let actions = div()
                .flex_shrink_0()
                .when(!active || floating, |this| this.invisible().group_hover(group.clone(), |style| style.visible()))
                .when(floating, |this| this.absolute().top_0().bottom_0().right_0().rounded_r(u(5.)).bg(theme.sidebar))
                .child(actions.when(floating, |this| {
                    this.h_full().pl(u(6.)).pr(u(9.5)).rounded_r(u(5.)).when(active || channel.clickable || (voice && can_join), |this| this.bg(row_bg))
                }));
            let green = Hsla::from(gpui_kit::rgb(GREEN));
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis() as u64;
            let sidebar_bg = theme.sidebar;
            let icon = div()
                .relative()
                .flex_shrink_0()
                .when(channel.occupied, |d| d.text_color(green))
                .child(if channel.rules { Icon::default().path("icons/rules.svg") } else { Icon::new(sidebar::channel_icon(channel.kind)) }.size(u(19.)))
                .when(channel.locked, |d| {
                    d.child(
                        div()
                            .absolute()
                            .top(u(-3.))
                            .right(u(-3.))
                            .size(u(11.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(sidebar_bg)
                            .child(Icon::default().path("icons/lock.svg").size(u(9.))),
                    )
                });
            // With a status under the name, the icon and clock stay level with the name.
            let has_status = channel.status.is_some();
            let row_height = if has_status { 46. } else { 32. };
            row_shell(SharedString::from(format!("channel-{}", channel.id)), active, channel.clickable, channel.path.clone())
                // Clicking a voice channel joins it, if allowed. Locked ones don't even look it.
                .when(voice && can_join, |this| {
                    let (store, guild, id) = (self.services.store.clone(), location.guild.unwrap_or_default(), channel.id);
                    this.cursor_pointer()
                        .when(!active, |this| this.hover(|this| this.bg(hover_bg).text_color(bright)))
                        .on_click(move |_, _, cx| store.update(cx, |store, cx| store.join_voice(guild, id, cx)))
                })
                .when(voice && !can_join, |this| this.opacity(0.6))
                .group(group)
                .relative()
                .h(u(row_height))
                .when(has_status, |this| this.items_start().pt(u(4.)))
                .mb(u(3.5))
                .text_size(rems(18. / 16.))
                .font_weight(FontWeight::MEDIUM)
                // Unread: bright and bold, with Discord's small white pill at the column's edge.
                .when(channel.unread && !active, |this| {
                    this.text_color(bright).font_weight(FontWeight::SEMIBOLD).child(
                        div()
                            .absolute()
                            .left(u(-ROW_LEFT))
                            .top(u(12.))
                            .w(u(4.))
                            .h(u(8.))
                            .rounded_tr(u(4.))
                            .rounded_br(u(4.))
                            .bg(bright),
                    )
                })
                .child(div().flex_shrink_0().when(has_status, |d| d.h(u(24.)).flex().items_center()).child(icon))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().min_w_0().truncate().when(has_status, |d| d.h(u(24.)).line_height(u(24.))).child(channel.label.clone()))
                        .when_some(channel.status.clone(), |d, status| {
                            d.child(div().min_w_0().truncate().text_size(u(12.)).font_weight(FontWeight::NORMAL).text_color(theme.muted_foreground).child(status))
                        }),
                )
                // A ping takes the clock's place until it's read.
                .when_some(channel.started.filter(|_| channel.mentions == 0), |this, started| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .when(has_status, |d| d.h(u(24.)).flex().items_center())
                            .font_family(theme.mono_font_family.clone())
                            .text_size(u(12.))
                            .font_weight(FontWeight::NORMAL)
                            .text_color(green)
                            .when(has_actions, |d| d.group_hover(SharedString::from(format!("channel-row-{}", channel.id)), |d| d.invisible()))
                            .child(sidebar::elapsed_label(started, now)),
                    )
                })
                .when(channel.mentions > 0, |this| {
                    this.child(
                        div()
                            .flex_shrink_0()
                            .when(has_status, |d| d.mt(u(4.)))
                            .min_w(u(16.))
                            .h(u(16.))
                            .px(u(4.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_full()
                            .bg(theme.danger)
                            .text_color(gpui_kit::white())
                            .text_size(u(12.))
                            .font_weight(FontWeight::BOLD)
                            .when(has_actions, |d| d.group_hover(SharedString::from(format!("channel-row-{}", channel.id)), |d| d.invisible()))
                            .child(channel.mentions.to_string()),
                    )
                })
                .when(has_actions, |this| this.child(actions))
        };

        // A thread under its channel: Discord's └ connector, then the name.
        let thread_row = |t: &sidebar::ChannelRow| {
            let active = location.channel == Some(t.id);
            row_shell(SharedString::from(format!("thread-{}", t.id)), active, true, t.path.clone())
                .h(u(32.))
                .mb(u(3.5))
                .pl(u(30.))
                .text_size(rems(1.))
                .font_weight(FontWeight::MEDIUM)
                .when(t.unread && !active, |this| this.text_color(bright).font_weight(FontWeight::SEMIBOLD))
                .child(
                    div()
                        .w(u(12.))
                        .h(u(16.))
                        .mt(u(-12.))
                        .flex_shrink_0()
                        .border_l_1()
                        .border_b_1()
                        .rounded_bl(u(6.))
                        .border_color(muted.opacity(0.6)),
                )
                .child(div().flex_1().min_w_0().truncate().child(t.label.clone()))
        };

        let dm_row = |dm: &sidebar::DmRow| {
            let active = location.channel == Some(dm.id);
            let avatar: AnyElement = match (&dm.group_icon, dm.group) {
                (Some(url), _) => img(url.clone()).size(u(32.)).flex_shrink_0().rounded_full().object_fit(ObjectFit::Cover).into_any_element(),
                (None, true) if dm.faces.len() == 2 => stacked_faces(&dm.faces, 32., theme.sidebar),
                (None, true) => div()
                    .size(u(32.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded_full()
                    .bg(theme.primary)
                    .text_color(theme.primary_foreground)
                    .child(Icon::new(IconName::UsersRound).size(u(14.5)))
                    .into_any_element(),
                (None, false) => {
                    // Typing to us: three dots where their status would be. Not animated,
                    // since that would redraw the whole list on every frame.
                    let typing = !self.services.store.read(cx).typing(dm.id).is_empty();
                    let dots = typing.then(|| {
                        h_flex()
                            .absolute()
                            .right(u(-3.))
                            .bottom(u(-3.))
                            .h(u(15.))
                            .px(u(3.))
                            .gap(u(2.))
                            .items_center()
                            .rounded_full()
                            // In their status's colour, cut out of the avatar as the dot is.
                            .border_2()
                            .border_color(theme.sidebar)
                            .bg(crate::members::status_color(dm.status.unwrap_or(cord_core::member_list::Status::Offline)))
                            .children((0..3).map(|_| div().size(u(3.)).rounded_full().bg(theme.sidebar)))
                            .into_any_element()
                    });
                    // Still, until the pointer is on the row.
                    let row_hovered = !crate::smooth_scroll::list_scrolling() && crate::transition::pointer_on(&SharedString::from(format!("channel-{}", dm.id)));
                    let decoration = dm.decoration.clone().map(|url| if row_hovered { crate::decorations::animated(&url) } else { url });
                    crate::members::avatar_with_overlay(
                        Avatar::new().border_0().src(dm.avatar_url.clone()).name(dm.label.clone()),
                        32.,
                        if typing { None } else { dm.status },
                        !typing && dm.mobile,
                        decoration,
                        theme.sidebar,
                        dots,
                    )
                    .into_any_element()
                }
            };
            let line = crate::members::status_element(&dm.line, dm.in_voice, cx);
            // Measured in Discord: 42px rows, a 32px avatar, a 16/20 name over a 12/16 line.
            let unread = dm.unread && !active;
            let row = row_shell(SharedString::from(format!("channel-{}", dm.id)), active, true, dm.path.clone())
                .when(unread, |this| this.text_color(bright))
                .on_mouse_down(gpui_kit::MouseButton::Right, {
                    let (dm, store) = (dm.clone(), self.services.store.clone());
                    move |event: &gpui_kit::MouseDownEvent, _, cx: &mut gpui_kit::App| {
                        cx.stop_propagation();
                        let entries = dm_menu(&dm, &store, event.position, cx);
                        crate::context_menu::show(event.position, entries, cx);
                    }
                })
                .h(u(42.))
                .flex_shrink_0()
                .relative()
                .overflow_hidden()
                // Their nameplate, behind the row, as in the member list.
                // Plays while hovered, once it's ready, so there's no gap between the two.
                .on_hover({
                    let (id, video) = (SharedString::from(format!("channel-{}", dm.id)), dm.nameplate_video.clone());
                    // This list alone: redrawing the window here redrew the chat for every
                    // row that passed under the pointer.
                    let view = cx.entity_id();
                    move |over: &bool, _: &mut Window, cx: &mut gpui_kit::App| {
                        crate::transition::set_pointer(&id, *over);
                        if crate::smooth_scroll::list_scrolling() {
                            if *over {
                                let video = video.clone();
                                crate::smooth_scroll::after_scrolling(id.clone(), move |cx| {
                                    if let Some(url) = &video {
                                        crate::nameplates::warm(url, cx);
                                    }
                                    cx.notify(view);
                                });
                            } else {
                                crate::smooth_scroll::cancel_after_scrolling(&id);
                            }
                            return;
                        }
                        if *over && let Some(url) = &video {
                            crate::nameplates::warm(url, cx);
                        }
                        cx.notify(view);
                    }
                })
                .children(dm.nameplate.clone().map(|url| {
                    let hovered = !crate::smooth_scroll::list_scrolling() && crate::transition::pointer_on(&SharedString::from(format!("channel-{}", dm.id)));
                    let playing = dm.nameplate_video.as_deref().filter(|_| hovered).and_then(|video| crate::nameplates::ready(video, cx));
                    let picture = match playing {
                        Some(image) => crate::anim::image(image, ObjectFit::Cover, 5.).absolute().top_0().left_0().size_full().into_any_element(),
                        None => img(url).absolute().top_0().left_0().size_full().rounded(u(5.)).object_fit(ObjectFit::Cover).into_any_element(),
                    };
                    div().absolute().top_0().left_0().size_full().opacity(0.6).child(picture)
                }))
                .when(dm.nameplate.is_some(), |d| d.children(crate::nameplates::backdrop(dm.palette.as_deref(), theme.sidebar, theme.mode.is_dark(), 5.)))
                .child(avatar)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .justify_center()
                        .child(
                            h_flex()
                                .min_w_0()
                                .gap(u(5.))
                                .items_center()
                                .child(
                                    div()
                                        .min_w_0()
                                        .text_size(u(16.))
                                        .line_height(u(20.))
                                        .font_weight(if unread { FontWeight::SEMIBOLD } else { FontWeight::MEDIUM })
                                        .map(|d| crate::badges::name_clip(d, &dm.stops))
                                        .when_some(dm.font, |d, f| d.font_family(f))
                                        .when_some(dm.color, |d, c| d.text_color(c))
                                        .child(crate::badges::name_effect(
                                            crate::badges::name_text(SharedString::from(format!("dm-name-{}", dm.id)), &dm.label, &dm.stops, crate::transition::pointer_on(&SharedString::from(format!("channel-{}", dm.id)))),
                                            &dm.label,
                                            &crate::badges::NameLook { effect: dm.effect, effect_color: dm.effect_color, font: dm.font, color: dm.color, stops: Vec::new() },
                                            !crate::smooth_scroll::list_scrolling() && crate::transition::pointer_on(&SharedString::from(format!("channel-{}", dm.id))),
                                        )),
                                )
                                .when_some(dm.tag.clone(), |d, (tag, badge)| {
                                    d.child(crate::badges::tag_pill(SharedString::from(format!("dm-tag-{}", dm.id)), &tag, badge.as_deref(), cx))
                                })
                                .when(dm.official, |d| d.child(crate::badges::official_badge()))
                                .when(dm.bot && !dm.official, |d| d.child(crate::badges::bot_badge(dm.verified_bot))),
                        )
                        .map(|d| match line {
                            Some(line) => d.child(line),
                            None if !dm.subtitle.is_empty() => d.child(div().text_size(u(12.)).line_height(u(16.)).truncate().child(dm.subtitle.clone())),
                            None => d,
                        }),
                );
            // Discord's small white pill, outside the row, which clips to its own corners.
            div().relative().w_full().flex_shrink_0().mb(u(2.)).child(row).when(unread, |this| {
                this.child(div().absolute().left_0().top(u(17.)).w(u(4.)).h(u(8.)).rounded_tr(u(4.)).rounded_br(u(4.)).bg(bright))
            })
        };

        let category_row = |id: cord_core::Id, label: &str, folded: bool, can_add: bool| {
            h_flex()
                .id(SharedString::from(format!("category-{id}")))
                .flex_shrink_0()
                .ml(u(ROW_LEFT))
                .mr(u(ROW_RIGHT))
                .px(u(9.5))
                .pt(u(17.5))
                .pb(u(6.5))
                .gap(u(3.))
                .items_center()
                .text_size(rems(1.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(theme.muted_foreground)
                .cursor_pointer()
                .hover(|this| this.text_color(bright))
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.collapsed.toggle(id);
                    cx.notify();
                }))
                .child(div().min_w_0().truncate().child(label.to_string()))
                .child(
                    Icon::new(if folded { IconName::ChevronRight } else { IconName::ChevronDown })
                        .size(u(11.))
                        .flex_shrink_0(),
                )
                .child(div().flex_1())
                .when(can_add, |d| d.child(mini(SharedString::from(format!("add-channel-{id}")), IconName::Plus, "Create Channel")))
        };

        let (title, in_guild, rows) = match &self.sidebar {
            Some(s) => (s.title.clone(), s.in_guild, s.rows.as_slice()),
            None => (String::new(), location.guild.is_some(), Default::default()),
        };
        const BANNER_HEIGHT: f32 = 135.;
        let guild_banner = guild_list_banner(self.sidebar.as_ref(), location);
        let has_banner = guild_banner.is_some();
        let mut body: Vec<AnyElement> = Vec::with_capacity(rows.len() + 1);
        if !in_guild {
            body.push(
                div()
                    .flex_shrink_0()
                    .px(u(17.))
                    .pt(u(17.5))
                    .pb(u(6.5))
                    .text_size(rems(1.))
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme.muted_foreground)
                    .child("Direct Messages")
                    .into_any_element(),
            );
        } else {
            // The server's banner, with the header over it until it's scrolled away.
            if let Some(url) = guild_banner.clone() {
                body.push(div().w_full().h(u(BANNER_HEIGHT)).flex_shrink_0().child(crate::anim::hover_banner("server-banner", url, 0.)).into_any_element());
                body.push(div().flex_shrink_0().h(u(12.)).into_any_element());
            }
            // Discord's server shortcuts above the categories, then a divider.
            let settings = self.services.settings.clone();
            let shortcut = |id: &'static str, icon: Icon, label: &'static str| {
                h_flex()
                    .id(id)
                    .flex_shrink_0()
                    .h(u(32.))
                    .mb(u(3.5))
                    .ml(u(ROW_LEFT))
                    .mr(u(ROW_RIGHT))
                    .px(u(9.5))
                    .gap(u(9.5))
                    .items_center()
                    .rounded(u(5.))
                    .text_color(muted)
                    .text_size(rems(18. / 16.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|this| this.bg(hover_bg).text_color(bright))
                    .child(icon.size(u(19.)).flex_shrink_0())
                    .child(label)
            };
            body.push(
                shortcut("guild-members", Icon::new(IconName::Users), "Members")
                    .on_click(move |_, _, cx| {
                        settings.update(cx, |s, cx| s.update(cx, |s| s.show_members = !s.show_members))
                    })
                    .into_any_element(),
            );
            body.push(
                shortcut("guild-boosts", Icon::default().path("icons/boost.svg"), "Server Boosts")
                    .tooltip(|window, cx| Tooltip::new("Server Boosts — arrives later").build(window, cx))
                    .into_any_element(),
            );
            body.push(
                div().flex_shrink_0().mx(u(16.)).my(u(8.)).h(u(1.)).bg(theme.border).into_any_element(),
            );
        }
        // Channels that ping us, for the "new mention" pills when scrolled away.
        let mut pinged: Vec<(usize, u32)> = Vec::new();
        let mut selected_row = None;
        // The list is built in full once whenever it changes, which tells us every row's place.
        let list_key = self.sidebar.as_ref().map(|s| s.key);
        let places_known = list_key.is_some() && self.drawn_list.get() == list_key;
        self.drawn_list.set(list_key);
        for row in rows {
            if let sidebar::Row::Channel(c) | sidebar::Row::Thread(c) = row
                && location.channel == Some(c.id)
            {
                selected_row = Some(body.len());
            }
            if let sidebar::Row::Channel(c) = row
                && c.mentions > 0
            {
                pinged.push((body.len(), c.mentions));
            }
            // Far off screen, a row is just its height: laying out every row of a long
            // list each frame is what made scrolling it slow.
            if let Some(height) = room_for_unseen_row(&self.channels_scroll, body.len(), places_known) {
                body.push(div().w_full().flex_shrink_0().h(gpui_kit::px(height)).into_any_element());
                continue;
            }
            body.push(match row {
                sidebar::Row::Category { id, label, folded, can_add } => category_row(*id, label, *folded, *can_add).into_any_element(),
                sidebar::Row::Channel(c) => channel_row(c).into_any_element(),
                sidebar::Row::Participant(p) => self.participant_row(p.clone(), cx).into_any_element(),
                sidebar::Row::Dm(d) => dm_row(d).into_any_element(),
                sidebar::Row::Thread(t) => thread_row(t).into_any_element(),
            });
        }

        let guild_list = self.sidebar.as_ref().map(|s| s.guilds.as_slice()).unwrap_or_default();
        let header_icon = guild_list
            .iter()
            .find(|g| Some(g.id) == location.guild)
            .and_then(|g| g.icon.as_ref().map(|hash| cdn::guild_icon_url(g.id, hash, 64)));
        let pills = mention_pills(&self.channels_scroll, &self.channels_scroller, smooth, &pinged, cx);
        // Besides the list, the pills and the header over a banner follow its scroll. Moving the
        // list as a layer leaves those alone, so it only will while they'd look the same.
        {
            let scroll = self.channels_scroll.clone();
            let pills_now = pills_shown(&scroll, -f32::from(scroll.offset().y), &pinged);
            let fade = move |top: f32| (top / (BANNER_HEIGHT - 49.)).clamp(0., 1.);
            let fade_now = fade(-f32::from(scroll.offset().y));
            self.channels_scroller.outside_stays_right(move |top| {
                (!has_banner || fade(top) == fade_now) && pills_shown(&scroll, top, &pinged) == pills_now
            });
        }
        // A jump to another channel brings its row into view. Its place is only
        // known once the new list has been laid out, so that waits a frame.
        if self.revealed_channel.get() != location.channel {
            self.revealed_channel.set(location.channel);
            self.reveal_wait.set(2);
        }
        let revealing = self.reveal_wait.get() > 0;
        if revealing {
            self.reveal_wait.set(self.reveal_wait.get() - 1);
            if self.reveal_wait.get() == 0
                && let Some(distance) = selected_row.and_then(|ix| distance_to_show(&self.channels_scroll, ix))
            {
                if smooth {
                    self.channels_scroller.glide_by(distance);
                } else {
                    let mut offset = self.channels_scroll.offset();
                    offset.y -= gpui_kit::px(distance);
                    self.channels_scroll.set_offset(offset);
                }
            }
        }
        // Voice timers tick: once a second is all this view needs for them.
        let ticking = rows.iter().any(|r| matches!(r, sidebar::Row::Channel(c) if c.started.is_some()));
        // Over the banner it's see-through, turning solid as the banner scrolls away.
        let scrolled = (f32::from(-self.channels_scroll.offset().y) / (BANNER_HEIGHT - 49.)).clamp(0., 1.);
        // GPUI can't blur what's behind: a shade that fades out downward keeps the
        // name readable with no hard edge, going solid as the banner scrolls away.
        let on_picture = has_banner && scrolled < 0.6;
        let header_shade: gpui_kit::Background = {
            let toward = |from: f32| -> Hsla {
                let c = theme.sidebar.to_rgb();
                let dark = 1. - scrolled;
                gpui_kit::Rgba { r: c.r * scrolled, g: c.g * scrolled, b: c.b * scrolled, a: from * dark + scrolled }.into()
            };
            gpui_kit::linear_gradient(180., gpui_kit::linear_color_stop(toward(0.65), 0.), gpui_kit::linear_color_stop(toward(0.), 1.))
        };
        let header = h_flex()
                    .h(u(49.))
                    .px(u(16.))
                    .gap(u(9.5))
                    .items_center()
                    .flex_shrink_0()
                    .border_b_1()
                    .border_color(theme.border)
                    .text_size(rems(18. / 16.))
                    .font_weight(FontWeight::SEMIBOLD)
                    .when_some(header_icon, |this, url| {
                        this.child(img(url).size(u(19.)).flex_shrink_0().object_fit(ObjectFit::Cover).rounded_full())
                    })
                    .when(in_guild, |this| this.child(div().min_w_0().truncate().child(title.clone())))
                    // Direct Messages: Discord's search pill (the list below has its own heading).
                    .when(!in_guild, |this| {
                        this.child(
                            div()
                                .flex_1()
                                .h(u(28.))
                                .px(u(8.))
                                .flex()
                                .items_center()
                                .rounded(u(4.))
                                .bg(theme.background)
                                .text_size(u(14.))
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(theme.muted_foreground)
                                .child("Find or start a conversation"),
                        )
                    })
                    .when(in_guild, |this| {
                        this.child(
                            div()
                                .id("server-menu")
                                .flex_shrink_0()
                                .cursor_default()
                                .tooltip(|window, cx| Tooltip::new("Server menu — arrives later").build(window, cx))
                                .child(Icon::new(IconName::ChevronDown).size(u(14.5))),
                        )
                        .child(div().flex_1())
                        .child(
                            div()
                                .id("server-invite")
                                .flex_shrink_0()
                                .cursor_default()
                                .text_color(if on_picture { gpui_kit::white() } else { theme.sidebar_foreground })
                                .tooltip(|window, cx| Tooltip::new("Invite People").build(window, cx))
                                .child(Icon::new(IconName::UsersRound).size(u(19.))),
                        )
                    })
            .when(has_banner, |this| this.absolute().top_0().left_0().right_0().occlude().bg(header_shade).when(scrolled < 1., |d| d.border_color(gpui_kit::transparent_black())).when(on_picture, |d| d.text_color(gpui_kit::white())))
            .into_any_element();
        let (header_above, header_over) = if has_banner { (None, Some(header)) } else { (Some(header), None) };

        v_flex()
            .w(u(self.services.settings.read(cx).get().column_width))
            .h_full()
            .flex_shrink_0()
            .bg(theme.sidebar)
            .border_l_1()
            .border_color(theme.border)
            .relative()
            .children(header_above)
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .when(smooth, |this| this.child(self.channels_scroller.layer()))
                    .child(
                        v_flex()
                            .id("channel-list")
                            .size_full()
                            .when(!has_banner, |d| d.pt(u(6.5)))
                            // The card floats over the end of the list: keep room for all of it.
                            .pb(u(if self.services.store.read(cx).voice_status().is_some() { 77. + VOICE_PANEL_ROOM } else { 77. }))
                            .overflow_y_scroll()
                            .track_scroll(&self.channels_scroll)
                            .children(body),
                    )
                    .when(revealing, |this| this.child(gpui_kit::canvas(|_, _, _| {}, |_, _, window, _| window.request_animation_frame())))
                    .children(pills)
                    .when(ticking, |this| {
                        this.child(gpui_kit::canvas(|_, _, _| {}, |_, _, window, cx| {
                            alhazen_gpui::redraw_after(std::time::Duration::from_secs(1), window, cx);
                        }))
                    }),
            )
            .children(header_over)
    }

    /// Someone in a voice channel: avatar, server name, and on the right
    /// what they have switched off, or a red LIVE pill while streaming.
    /// The server whose voice channels are being shown.
    fn voice_row_guild(&self, cx: &Context<Self>) -> Option<cord_core::Id> {
        nav::parse_location(self.router.read(cx).current_path())?.guild
    }

    fn participant_row(&self, p: sidebar::Participant, cx: &Context<Self>) -> impl IntoElement {
        let row_top = std::rc::Rc::new(std::cell::Cell::new(gpui_kit::px(0.)));
        let theme = cx.theme();
        let locally_muted = crate::places::is_muted(p.user_id);
        let speaking = self.services.store.read(cx).speaking().contains(&p.user_id);
        h_flex()
            .id(SharedString::from(format!("voice-{}", p.user_id)))
            .on_mouse_down(gpui_kit::MouseButton::Right, {
                let (services, router, user, name, guild) = (self.services.clone(), self.router.clone(), p.user_id, p.name.clone(), self.voice_row_guild(cx));
                let (top, column) = (row_top.clone(), self.services.settings.read(cx).get().column_width);
                move |event: &gpui_kit::MouseDownEvent, window: &mut Window, cx: &mut gpui_kit::App| {
                    cx.stop_propagation();
                    if let Some(guild) = guild {
                        // Where clicking them opens it: just right of the column, level with the row.
                        let card_at = gpui_kit::point(window.rem_size() * ((RAIL_WIDTH + column + 8.) / 16.), top.get());
                        voice_person_menu(&services, &router, guild, user, name.clone(), event.position, card_at, cx);
                    }
                }
            })
            .flex_shrink_0()
            .h(u(32.))
            .mb(u(1.5))
            // Lit under the pointer from a little before the avatar, not the column's edge.
            .ml(u(ROW_LEFT + 29.))
            .mr(u(ROW_RIGHT))
            .pl(u(8.5))
            .pr(u(9.5))
            .gap(u(8.))
            .items_center()
            .rounded(u(5.))
            .text_color(theme.sidebar_foreground)
            .hover(|this| this.bg(theme.sidebar_accent.opacity(0.6)))
            .cursor_pointer()
            .relative()
            .child({
                let top = row_top.clone();
                gpui_kit::canvas(move |bounds, _, _| top.set(bounds.top()), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
            })
            .on_click({
                let (user, column) = (p.user_id, self.services.settings.read(cx).get().column_width);
                // Their card opens just right of the column, level with the row's top.
                move |_, window, cx| {
                    let x = window.rem_size() * ((RAIL_WIDTH + column + 8.) / 16.);
                    crate::links::open_profile(user, gpui_kit::point(x, row_top.get()), cx);
                }
            })
            .child(
                div()
                    .relative()
                    .flex_shrink_0()
                    .child(Avatar::new().border_0().src(p.avatar_url).name(p.name.clone()).size(u(25.5)))
                    .when(speaking, |d| {
                        d.child(div().absolute().top(u(-2.)).left(u(-2.)).size(u(29.5)).rounded_full().border_2().border_color(Hsla::from(gpui_kit::rgb(GREEN))))
                    }),
            )
            .child(
                h_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(u(5.))
                    .child(
                        div()
                            .min_w_0()
                            .truncate()
                            .text_size(rems(17. / 16.))
                            .font_weight(FontWeight::MEDIUM)
                            .when_some(p.font, |d, f| d.font_family(f))
                            .child(p.name),
                    )
                    .when_some(p.tag_badge.as_deref(), |d, url| d.child(crate::badges::tag_icon(url, cx))),
            )
            .when(p.streaming, |this| {
                this.child(
                    div()
                        .px(u(5.))
                        .rounded(u(3.))
                        .bg(theme.danger)
                        .text_color(theme.primary_foreground)
                        .text_size(u(8.))
                        .font_weight(FontWeight::BOLD)
                        .child("LIVE"),
                )
            })
            // Muted by us alone: a speaker, not a microphone, since they can still talk.
            .when(locally_muted, |this| this.child(Icon::new(IconName::VolumeX).size(u(16.)).flex_shrink_0().text_color(theme.muted_foreground.opacity(0.7))))
            .when(p.muted, |this| this.child(Icon::new(IconName::MicOff).size(u(16.)).flex_shrink_0().when(p.server_muted, |d| d.text_color(gpui_kit::rgb(0xf23f43)))))
            .when(p.deafened, |this| this.child(Icon::new(IconName::HeadphoneOff).size(u(16.)).flex_shrink_0().when(p.server_deafened, |d| d.text_color(gpui_kit::rgb(0xf23f43)))))
    }

    /// What sits at the bottom of the column: who you are and, in a call, the call.
    fn bottom_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        v_flex()
            // The list runs on underneath: clicks here stay here.
            .occlude()
            .w_full()
            .rounded(u(8.))
            .border_1()
            .border_color(theme.sidebar_accent)
            .bg(panel(theme))
            .children(self.voice_panel(cx))
            .child(self.user_panel(cx))
    }

    /// "Voice Connected", with the channel, while in a call.
    fn voice_panel(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let store = self.services.store.read(cx);
        let status = store.voice_status()?;
        let cache = store.cache();
        let theme = cx.theme();
        let channel = cache.channel(status.channel).map_or_else(|| "Voice".to_string(), |c| c.name.clone().unwrap_or_default());
        let guild = cache.guild(status.guild).map_or_else(String::new, |g| g.name.clone());
        let green = gpui_kit::Hsla::from(gpui_kit::rgb(0x23a55a));
        let yellow = gpui_kit::Hsla::from(gpui_kit::rgb(0xf0b232));
        let red = gpui_kit::Hsla::from(gpui_kit::rgb(0xf23f43));
        let color = match (status.connected, status.ping_ms.map(crate::store::ping_band)) {
            (false, _) => yellow,
            (true, Some(crate::store::PingBand::Fair)) => yellow,
            (true, Some(crate::store::PingBand::Bad)) => red,
            (true, _) => green,
        };
        let ping = status.ping_ms;
        let leave = self.services.store.clone();
        let round = |id: &'static str| {
            div().id(id).size(u(32.)).flex_shrink_0().flex().items_center().justify_center().rounded(u(6.)).cursor_pointer().hover(|d| d.bg(theme.sidebar_accent))
        };
        Some(
            h_flex()
                .flex_shrink_0()
                .px(u(8.))
                .py(u(8.))
                .gap(u(8.))
                .items_center()
                .border_b_1()
                .border_color(theme.sidebar_accent)
                .child(
                    div()
                        .id("voice-signal")
                        .size(u(32.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(u(6.))
                        .text_color(color)
                        .tooltip(move |window, cx| {
                            let tip = ping.map_or_else(|| "Measuring".to_string(), |ms| format!("{ms} ms"));
                            gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx)
                        })
                        .child(Icon::new(IconName::Wifi).size(u(20.))),
                )
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_size(u(14.)).font_weight(FontWeight::SEMIBOLD).text_color(color).child(if status.connected { "Voice Connected" } else { "Connecting" }))
                        .child(div().truncate().text_size(u(12.)).text_color(theme.muted_foreground).child(format!("{channel} / {guild}"))),
                )
                .child(
                    round("voice-leave")
                        .text_color(theme.sidebar_foreground)
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Disconnect").build(window, cx))
                        .on_click(move |_, _, cx| leave.update(cx, |store, cx| store.leave_voice(cx)))
                        .child(Icon::new(IconName::PhoneOff).size(u(18.))),
                )
                .into_any_element(),
        )
    }

    fn user_panel(&self, cx: &Context<Self>) -> impl IntoElement {
        let me = self.services.store.read(cx).cache().me().cloned();
        let theme = cx.theme();
        let router = self.router.clone();
        let call = self.services.store.read(cx).voice_status();
        let deaf = call.as_ref().is_some_and(|c| c.deaf);
        let deafen = self.services.store.clone();
        let cache = self.services.store.read(cx).cache();
        let own = me.as_ref().and_then(|m| cache.presence(m.id)).cloned().unwrap_or(cord_core::member_list::UserPresence { status: cord_core::member_list::Status::Online, ..Default::default() });
        let voice = me.as_ref().is_some_and(|m| cache.in_voice(m.id));
        let own_line = crate::members::status_element(&own.line, voice, cx);
        let button = |id: &'static str, icon: IconName, tooltip: &'static str| {
            div()
                .id(id)
                .size(u(29.))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(5.))
                .text_color(theme.sidebar_foreground)
                .cursor_pointer()
                .hover(|this| this.bg(theme.sidebar_accent).text_color(theme.sidebar_accent_foreground))
                .tooltip(move |window, cx| Tooltip::new(tooltip).build(window, cx))
                .child(Icon::new(icon).size(u(19.)))
        };
        // Mic and headphones carry a small device-menu chevron (inert for now).
        let with_menu = |id: &'static str, icon: IconName, tooltip: &'static str| {
            h_flex().flex_shrink_0().items_center().gap(u(3.)).child(button(id, icon, tooltip)).child(
                Icon::new(IconName::ChevronDown).size(u(11.)).text_color(theme.sidebar_foreground).flex_shrink_0(),
            )
        };
        h_flex()
            .h(u(PANEL_HEIGHT))
            .w_full()
            .flex_shrink_0()
            .pl(u(11.))
            .pr(u(8.))
            .gap(u(9.5))
            .items_center()
            .when_some(me, |this, me| {
                let avatar = Avatar::new().border_0().src(cdn::avatar_url(me.id, me.avatar.as_deref(), 64)).name(me.display_name().to_string());
                this.child(div().flex_shrink_0().child(crate::members::avatar_with_status(avatar, 32., Some(own.status), false, None, panel(theme))))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().text_size(rems(18. / 16.)).font_weight(FontWeight::SEMIBOLD).truncate().child(me.display_name().to_string()))
                        .map(|d| match own_line {
                            Some(line) => d.child(line),
                            None => d.child(div().text_size(rems(14. / 16.)).text_color(theme.muted_foreground).truncate().child(me.username.clone())),
                        }),
                )
            })
            .child(with_menu("voice-mic", if call.is_some() { IconName::MicOff } else { IconName::Mic }, if call.is_some() { "Listening only for now" } else { "Join a voice channel to listen" }))
            // Deafen: only does something in a call.
            .child(
                h_flex()
                    .flex_shrink_0()
                    .items_center()
                    .gap(u(3.))
                    .child(
                        button("voice-headphones", if deaf { IconName::HeadphoneOff } else { IconName::Headphones }, if deaf { "Undeafen" } else { "Deafen" })
                            .when(deaf, |d| d.text_color(gpui_kit::rgb(0xf23f43)))
                            .on_click(move |_, _, cx| deafen.update(cx, |store, cx| store.toggle_deaf(cx))),
                    )
                    .child(Icon::new(IconName::ChevronDown).size(u(11.)).text_color(theme.sidebar_foreground).flex_shrink_0()),
            )
            .child(
                button("open-settings", IconName::Settings, "User Settings (Ctrl+,)")
                    .on_click(move |_, _, cx| navigate(&router, "/settings", cx)),
            )
    }

}

impl Render for SidePane {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::count("side");
        let path = self.router.read(cx).current_path().to_string();
        let Some(location) = nav::parse_location(&path) else { return div().into_any_element() };
        self.refresh_sidebar(location, cx);
        // What Discord's client asks on opening a server: voice statuses and timers.
        if let Some(guild) = location.guild {
            if self.services.store.read(cx).cache().me().is_some() && self.voice_info_asked.insert(guild) {
                self.services.core.request_channel_info(guild);
            }
        }
        // Moved to another server or the DMs: that list's own scroll position.
        if self.list_shown != Some(location.guild) {
            if let Some(old) = self.list_shown {
                self.list_offsets.insert(old, self.channels_scroll.offset());
            }
            self.list_shown = Some(location.guild);
            self.channels_scroller.stop();
            self.channels_scroll.set_offset(self.list_offsets.get(&location.guild).copied().unwrap_or_default());
        }
        h_flex()
            .relative()
            .h_full()
            .flex_shrink_0()
            .child(self.rail(location, cx))
            .child(self.channel_column(location, cx))
            .child(
                div()
                    .absolute()
                    .left(u(8.))
                    .bottom(u(8.))
                    .w(u(RAIL_WIDTH + self.services.settings.read(cx).get().column_width - 16.))
                    .child(self.bottom_card(cx)),
            )
            .into_any_element()
    }
}

impl Shell {
    pub fn new(router: Entity<Router>, services: Services, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let subscriptions = vec![
            cx.observe(&router, |_, _, cx| cx.notify()),
            cx.observe(&services.store, |_, _, cx| cx.notify()),
            cx.observe(&services.settings, |_, _, cx| cx.notify()),
            cx.subscribe(&services.store, |this, _, event: &StoreEvent, cx| match event {
                StoreEvent::SignedOut => navigate(&this.router, "/login", cx),
                StoreEvent::MemberList => {}
            }),
            cx.subscribe_in(&services.toasts, window, |_, _, toast: &Toast, window, cx| {
                let notification = match toast.kind {
                    ToastKind::Info => Notification::info(toast.message.clone()),
                    ToastKind::Success => Notification::success(toast.message.clone()),
                    ToastKind::Warning => Notification::warning(toast.message.clone()),
                    ToastKind::Error => Notification::error(toast.message.clone()),
                };
                window.push_notification(notification.title(toast.title.clone()), cx);
            }),
        ];
        window.set_window_title("Rustcord");
        // RUSTCORD_INFO=1 opens the Ctrl+I panel at start (for profiling).
        if std::env::var_os("RUSTCORD_INFO").is_some() {
            cx.defer_in(window, |this: &mut Self, window, cx| this.toggle_info(window, cx));
        }
        let side = cx.new(|cx| SidePane::new(router.clone(), services.clone(), cx));
        Shell { dragging_column: false, pictures: cx.new(crate::anim::PictureCache::new), start_checked: false, router, started: Instant::now(), info: None, services, side, _subscriptions: subscriptions }
    }

    /// Discord's 32px top bar: history arrows on the left, where you are in
    /// the middle, inbox and help on the right. Dragging it moves the window.
    fn discord_bar(&self, location: Location, window: &Window, cx: &Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let router = self.router.read(cx);
        let (can_back, can_forward) = (router.can_go_back(), router.can_go_forward());
        let (muted, bright) = (theme.muted_foreground, theme.foreground);
        let guild = self.side.read(cx).guild_header(location);
        let title: SharedString = match &guild {
            Some((name, _)) => name.clone().into(),
            None => "Direct Messages".into(),
        };
        let icon = guild.and_then(|(_, icon)| icon);
        let arrow = |id: &'static str, icon: IconName, enabled: bool, router: Entity<Router>, back: bool| {
            div()
                .id(id)
                .size(u(24.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(u(4.))
                .text_color(if enabled { muted } else { muted.opacity(0.4) })
                .when(enabled, |d| {
                    d.cursor_pointer().hover(|d| d.text_color(bright)).on_click(move |_, _, cx| {
                        router.update(cx, |r, cx| if back { r.back("/channels/@me", cx) } else { r.forward(cx) })
                    })
                })
                .child(Icon::new(icon).size(u(18.)))
        };
        let tool = |id: &'static str, icon: IconName, tip: &'static str| {
            div()
                .id(id)
                .size(u(24.))
                .flex()
                .items_center()
                .justify_center()
                .text_color(muted)
                .hover(|d| d.text_color(bright))
                .tooltip(move |window, cx| Tooltip::new(tip).build(window, cx))
                .child(Icon::new(icon).size(u(20.)))
        };
        h_flex()
            .id("discord-bar")
            .relative()
            .h(u(32.))
            .flex_shrink_0()
            .px(u(8.))
            .items_center()
            .bg(theme.sidebar)
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, window, _| window.start_window_move())
            .child(arrow("bar-back", IconName::ArrowLeft, can_back, self.router.clone(), true))
            .child(arrow("bar-forward", IconName::ArrowRight, can_forward, self.router.clone(), false))
            .child(
                h_flex()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top_0()
                    .bottom_0()
                    .justify_center()
                    .items_center()
                    .gap(u(8.))
                    .text_size(u(14.))
                    .font_weight(FontWeight::MEDIUM)
                    .when_some(icon, |d, url| d.child(img(url).size(u(16.)).rounded(u(4.)).object_fit(ObjectFit::Cover)))
                    .child(title),
            )
            .child(div().flex_1())
            .child(h_flex().gap(u(12.)).child(tool("bar-inbox", IconName::Inbox, "Inbox")).child(tool("bar-help", IconName::CircleQuestionMark, "Help")))
            .when(cfg!(target_os = "windows"), |this| {
                this.window_control_area(gpui_kit::WindowControlArea::Drag).child(windows_controls(window, cx))
            })
    }
}

impl Shell {
    fn toggle_info(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.info = match self.info.take() {
            Some(_) => None,
            None => Some(cx.new(|cx| FpsMonitor::new(window, cx))),
        };
        cx.notify();
    }

    /// Version, connection and cache figures under the performance HUD.
    fn info_card(&self, cx: &Context<Self>) -> impl IntoElement {
        let store = self.services.store.read(cx);
        let stats = store.cache().stats();
        let connection = match store.connection() {
            None => "Signed out".to_string(),
            Some(ConnectionState::Connecting) => "Connecting".into(),
            Some(ConnectionState::Connected) => "Connected".into(),
            Some(ConnectionState::Reconnecting { attempt }) => format!("Reconnecting (attempt {attempt})"),
        };
        let theme = cx.theme();
        let row = |label: &str, value: String| {
            h_flex()
                .justify_between()
                .gap_4()
                .child(div().text_color(theme.muted_foreground).child(label.to_string()))
                .child(value)
        };
        v_flex()
            .w_full()
            .p_3()
            .gap_1()
            .rounded(theme.radius)
            .bg(theme.popover.opacity(0.92))
            .border_1()
            .border_color(theme.border)
            .text_xs()
            .child(div().font_weight(FontWeight::SEMIBOLD).child(format!("Rustcord {}", env!("CARGO_PKG_VERSION"))))
            .child(row("Discord build", self.services.core.http().identity().build_number.to_string()))
            .child(row("Gateway", connection))
            .child(row("Uptime", nav::format_uptime(self.started.elapsed())))
            .child(row("Servers", stats.guilds.to_string()))
            .child(row("Channels", stats.channels.to_string()))
            .child(row("Users", stats.users.to_string()))
            .child(row("Cached messages", stats.messages.to_string()))
            .child(row("Memory in use", format!("{} MB", crate::perf::live_mb())))
            .child(row("Memory allocated", format!("{} MB", crate::perf::private_mb())))
            .child(row("Pictures on GPU", crate::perf::gpu_line()))
            .child(div().pt_1().text_color(theme.muted_foreground).child("Ctrl+I to close"))
    }
}

/// How much taller the bottom card is during a call.
const VOICE_PANEL_ROOM: f32 = 66.;

impl Shell {
    /// A mouse's side buttons go back and forward, wherever the pointer is.
    fn mouse_side_buttons(&self) -> impl IntoElement {
        let router = self.router.clone();
        gpui_kit::canvas(
            |_, _, _| {},
            move |_, _, window, _| {
                let router = router.clone();
                window.on_mouse_event(move |event: &gpui_kit::MouseDownEvent, phase, _, cx| {
                    if phase != gpui_kit::DispatchPhase::Capture {
                        return;
                    }
                    match event.button {
                        gpui_kit::MouseButton::Navigate(gpui_kit::NavigationDirection::Back) => router.update(cx, |router, cx| router.back("/channels/@me", cx)),
                        gpui_kit::MouseButton::Navigate(gpui_kit::NavigationDirection::Forward) => router.update(cx, |router, cx| router.forward(cx)),
                        _ => {}
                    }
                });
            },
        )
        .absolute()
        .size_0()
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::perf::count("shell");
        // Zoom scales the whole UI through the window's rem size.
        let rem = gpui_kit::px(self.services.settings.read(cx).get().rem_px());
        if window.rem_size() != rem {
            window.set_rem_size(rem);
        }
        crate::ui::note_shell_view(cx.entity_id());
        crate::emoji_card::set_store(self.services.store.clone());
        // The app's zoom counts as much as the display's: pixels are what's asked for.
        cord_core::cdn::set_device_scale(window.scale_factor() * f32::from(window.rem_size()) / 16.);
        crate::perf::set_big_from(self.services.settings.read(cx).get().big_memory_kb);
        // Left the server or the group since: home instead of a page of nothing.
        if !self.start_checked && self.services.store.read(cx).cache().stats().guilds > 0 {
            self.start_checked = true;
            let channel = nav::parse_location(self.router.read(cx).current_path()).and_then(|l| l.channel);
            if channel.is_some_and(|c| self.services.store.read(cx).cache().channel(c).is_none()) {
                let router = self.router.clone();
                cx.defer(move |cx| navigate(&router, "/channels/@me", cx));
            }
        }
        v_flex()
            .relative()
            .image_cache(self.pictures.clone())
            .on_modifiers_changed(|event: &gpui_kit::ModifiersChangedEvent, _, cx: &mut gpui_kit::App| crate::ui::note_shift(event.modifiers.shift, cx))
            .on_action(cx.listener(|this, _: &ToggleInfo, window, cx| this.toggle_info(window, cx)))
            .on_action(cx.listener(|this, _: &OpenSettings, _, cx| navigate(&this.router, "/settings", cx)))
            .size_full()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .when_some(nav::parse_location(self.router.read(cx).current_path()), |this, location| {
                this.child(self.discord_bar(location, window, cx))
            })
            .child({
                let path = self.router.read(cx).current_path().to_string();
                let body = h_flex().flex_1().min_h_0().border_t_1().border_color(cx.theme().border);
                let location = nav::parse_location(&path);
                match location {
                    Some(_) => body
                        .child(gpui_kit::AnyView::from(self.side.clone()).cached(
                            gpui_kit::StyleRefinement::default().h_full().w(u(RAIL_WIDTH + self.services.settings.read(cx).get().column_width)).flex_shrink_0(),
                        ))
                        .child(
                            // No width of its own: the handle overlaps the edge.
                            div().relative().w_0().h_full().flex_shrink_0().child(
                                div()
                                    .id("column-edge")
                                    .absolute()
                                    .left(u(-2.))
                                    .top_0()
                                    .w(u(5.))
                                    .h_full()
                                    .cursor_col_resize()
                                    .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                                        this.dragging_column = true;
                                        cx.stop_propagation();
                                    })),
                            ),
                        )
                        .child(div().flex_1().min_w_0().h_full().pl(gpui_kit::px(1.)).child(gpui_kit::AnyView::from(self.router.clone()).cached(gpui_kit::StyleRefinement::default().size_full()))),
                    None => body.child(div().size_full().child(gpui_kit::AnyView::from(self.router.clone()).cached(gpui_kit::StyleRefinement::default().size_full()))),
                }
            })
            .when(self.dragging_column, |d| d.cursor_col_resize())
            .on_mouse_move(cx.listener(|this, event: &gpui_kit::MouseMoveEvent, window, cx| {
                if !this.dragging_column {
                    return;
                }
                let scale = f32::from(window.rem_size()) / 16.;
                let width = f32::from(event.position.x) / scale - RAIL_WIDTH;
                this.services.settings.update(cx, |settings, cx| settings.update_unsaved(cx, |s| s.column_width = width));
            }))
            .on_mouse_up(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.dragging_column {
                    this.dragging_column = false;
                    this.services.settings.read(cx).save();
                }
            }))
            .on_key_down(crate::viewer::on_key)
            .on_key_down(crate::videos::on_key)
            .on_key_down(|event: &gpui_kit::KeyDownEvent, _, cx: &mut gpui_kit::App| {
                if event.keystroke.key == "escape" && crate::context_menu::is_open(cx) {
                    crate::context_menu::close(cx);
                }
            })
            .children(crate::viewer::overlay(window, cx))
            .children(crate::videos::overlay(cx))
            .children(crate::context_menu::overlay(cx))
            .child(self.mouse_side_buttons())
            .children(crate::confirm::overlay(window, cx))
            .children(crate::add_people::overlay(window, cx))
            .children(crate::panels::overlay(window, cx))
            .children(crate::emoji_card::overlay(cx))
            .when_some(self.info.clone(), |this, monitor| {
                this.child(
                    v_flex()
                        .absolute()
                        .top(u(9.5))
                        .right(u(9.5))
                        .w(u(224.))
                        .gap_2()
                        .child(monitor)
                        .child(self.info_card(cx)),
                )
            })
    }
}

/// Discord's red "1 NEW MENTION" pills, over the top and bottom of the channel
/// list while a pinged channel is scrolled out of view. A click scrolls to it.
/// Windows' minimize, maximize or restore, and close, at the bar's right end.
fn windows_controls(window: &Window, cx: &gpui_kit::App) -> impl IntoElement {
    use gpui_kit::WindowControlArea as Area;
    let theme = cx.theme();
    let (fg, hover) = (theme.foreground, theme.foreground.opacity(0.1));
    let maximized = window.is_maximized();
    let button = |id: &'static str, icon: &'static str, area: Area, close: bool| {
        div()
            .id(id)
            .occlude()
            .w(u(46.))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .text_color(fg)
            .window_control_area(area)
            .on_mouse_down(gpui_kit::MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .hover(move |d| if close { d.bg(gpui_kit::rgb(0xc42b1c)).text_color(gpui_kit::white()) } else { d.bg(hover) })
            .child(Icon::default().path(icon).size(u(10.)))
    };
    h_flex()
        .flex_shrink_0()
        .h_full()
        .ml(u(8.))
        .mr(u(-8.))
        .child(button("win-minimize", "icons/win-minimize.svg", Area::Min, false))
        .child(button("win-maximize", if maximized { "icons/win-restore.svg" } else { "icons/win-maximize.svg" }, Area::Max, false))
        .child(button("win-close", "icons/win-close.svg", Area::Close, true))
}

/// The banner of the server being looked at.
fn guild_list_banner(sidebar: Option<&sidebar::Sidebar>, location: Location) -> Option<String> {
    sidebar?.guilds.iter().find(|g| Some(g.id) == location.guild)?.banner.clone()
}

/// Right click on a server. What isn't built yet shows dimmed; what we've no
/// permission for isn't there at all.
fn server_menu(guild: cord_core::Id, store: &gpui_kit::Entity<crate::store::Store>, cx: &gpui_kit::App) -> Vec<crate::context_menu::Entry> {
    use crate::context_menu::Entry;
    use cord_core::permissions as perm;
    let cache = store.read(cx).cache();
    let held = cache.guild_permissions(guild);
    let can = |p: u64| held & (p | perm::ADMINISTRATOR) != 0;
    let unread = !cache.unread_channels(guild).is_empty();
    let mut entries = vec![if unread {
        let store = store.clone();
        Entry::item("Mark As Read", move |_, cx| store.update(cx, |store, cx| store.mark_guilds_read(&[guild], cx)))
    } else {
        Entry::later("Mark As Read")
    }];
    entries.push(Entry::Separator);
    entries.push(Entry::later("Invite to Server"));
    entries.push(Entry::Separator);
    entries.push(Entry::later("Mute Server"));
    entries.push(Entry::later("Notification Settings"));
    entries.push(Entry::later("Hide Muted Channels"));
    entries.push(Entry::later("Show All Channels"));
    entries.push(Entry::Separator);
    if can(perm::MANAGE_GUILD) {
        entries.push(Entry::later("Server Settings"));
    }
    entries.push(Entry::later("Privacy Settings"));
    entries.push(Entry::later("Edit Per-server Profile"));
    entries.push(Entry::Separator);
    if can(perm::MANAGE_CHANNELS) {
        entries.push(Entry::later("Create Channel"));
        entries.push(Entry::later("Create Category"));
    }
    if can(perm::CREATE_EVENTS) {
        entries.push(Entry::later("Create Event"));
    }
    if can(perm::MANAGE_CHANNELS | perm::CREATE_EVENTS) {
        entries.push(Entry::Separator);
    }
    entries.push(Entry::later("Move to..."));
    entries.push(Entry::Separator);
    if can(perm::MANAGE_GUILD) {
        entries.push(Entry::later("Security Actions").danger());
    }
    entries.push(Entry::later("Leave Server").danger());
    entries.push(Entry::Separator);
    entries.push(Entry::copy("Copy Server ID", guild.to_string()).icon(IconName::Copy));
    entries
}

/// Right click on a server folder.
fn folder_menu(
    guilds: Vec<cord_core::Id>,
    store: &gpui_kit::Entity<crate::store::Store>,
    pane: gpui_kit::WeakEntity<SidePane>,
    cx: &gpui_kit::App,
) -> Vec<crate::context_menu::Entry> {
    use crate::context_menu::Entry;
    let unread = guilds.iter().any(|g| !store.read(cx).cache().unread_channels(*g).is_empty());
    let mut entries = vec![if unread {
        let store = store.clone();
        Entry::item("Mark Folder As Read", move |_, cx| store.update(cx, |store, cx| store.mark_guilds_read(&guilds, cx)))
    } else {
        Entry::later("Mark Folder As Read")
    }];
    entries.push(Entry::Separator);
    entries.push(Entry::later("Move to..."));
    entries.push(Entry::Separator);
    entries.push(Entry::later("Folder Settings"));
    entries.push(Entry::item("Close All Folders", move |_, cx| {
        let _ = pane.update(cx, |pane, cx| {
            pane.open_folders.clear();
            cx.notify();
        });
    }));
    entries
}

/// Right click on a DM or group in the list, as Discord's. What isn't built yet shows dimmed.
/// The right click menu of someone in a voice channel. Only what we may do; anything
/// done to them for everyone asks first.
fn voice_person_menu(services: &Services, router: &Entity<Router>, guild: cord_core::Id, user: cord_core::Id, name: String, position: gpui_kit::Point<gpui_kit::Pixels>, card_at: gpui_kit::Point<gpui_kit::Pixels>, cx: &mut gpui_kit::App) {
    use crate::context_menu::{self as menu, Entry};
    use cord_core::permissions as perms;
    let store = services.store.read(cx);
    let cache = store.cache();
    let me = cache.me().map(|u| u.id);
    let itself = me == Some(user);
    let in_channel = cache.voice_state(guild, user).and_then(|v| v.channel_id);
    let may = |permission: u64| in_channel.is_some_and(|c| cache.has_permission(c, permission));
    let (can_mute, can_deafen, can_move) = (may(perms::MUTE_MEMBERS), may(perms::DEAFEN_MEMBERS), may(perms::MOVE_MEMBERS));
    let can_kick = cache.moderation(guild, user).kick;
    let handle = cache.user(user).map_or_else(|| name.clone(), |u| u.username.clone());
    let state = cache.voice_state(guild, user).map(|v| (v.mute, v.deaf)).unwrap_or_default();
    let core = store.core().clone();
    let mut entries = vec![Entry::item("Profile", move |_, cx| crate::links::show_profile(user, card_at, cx))];
    entries.push({
        let mention = name.clone();
        Entry::item("Mention", move |_, cx| crate::links::request_mention(user, mention.clone(), cx))
    });
    if !itself {
        let (core, router) = (core.clone(), router.clone());
        entries.push(Entry::item("Message", move |_, cx| {
            let (core, router) = (core.clone(), router.clone());
            cx.spawn(async move |cx| {
                if let Ok(channel) = core.open_dm(user).await {
                    cx.update(|cx| navigate(&router, format!("/channels/@me/{}", channel.id), cx));
                }
            })
            .detach();
        }));
    }
    if !itself {
        let (store, who) = (services.store.clone(), name.clone());
        entries.push(Entry::item("Add Note", move |window, cx| {
            crate::panels::open(store.clone(), crate::panels::What::Note { user, name: who.clone() }, window, cx);
        }).hint("Only visible to you"));
    }
    if !itself {
        entries.push(Entry::Separator);
        let (volume, set) = (crate::places::get(), services.store.clone());
        let level = volume.volumes.get(&user).copied().unwrap_or(1.);
        entries.push(Entry::slider("User Volume", level, move |value, cx| {
            set.update(cx, |store, _| store.set_user_volume(user, value));
        }));
        let store = services.store.clone();
        let muted = volume.muted.contains(&user);
        entries.push(Entry::check("Mute", muted, false, move |_, cx| {
            let now = !crate::places::get().muted.contains(&user);
            store.update(cx, |store, _| store.set_user_muted(user, now));
        }));
    }
    entries.push(Entry::Separator);
    let mods = cache.moderation(guild, user);
    if mods.nickname {
        let (store, who, current) = (services.store.clone(), name.clone(), cache.member(guild, user).and_then(|m| m.nick.clone()).unwrap_or_default());
        entries.push(Entry::item("Change Nickname", move |window, cx| {
            crate::panels::open(store.clone(), crate::panels::What::Nickname { guild, user, me: itself, name: who.clone(), current: current.clone() }, window, cx);
        }));
    }
    if mods.roles {
        entries.push(Entry::later("Roles"));
    }
    entries.push(Entry::later("Move To"));
    if can_mute || can_deafen || can_move || (can_kick && !itself) {
        entries.push(Entry::Separator);
    }
    let change = |label: &'static str, field: &'static str, value: bool| {
        let core = core.clone();
        Entry::check(label, !value, true, move |_, cx| {
            let core = core.clone();
            cx.spawn(async move |_| {
                let _ = core.update_member_voice(guild, user, serde_json::json!({ field: value })).await;
            })
            .detach();
        })
    };
    if can_mute {
        entries.push(change("Server Mute", "mute", !state.0));
    }
    if can_deafen {
        entries.push(change("Server Deafen", "deaf", !state.1));
    }
    if can_move {
        let core = core.clone();
        entries.push(Entry::item("Disconnect", move |_, cx| {
            let core = core.clone();
            cx.spawn(async move |_| {
                let _ = core.update_member_voice(guild, user, serde_json::json!({ "channel_id": null })).await;
            })
            .detach();
        }).danger());
    }
    if can_kick {
        let (store, handle, who) = (services.store.clone(), handle, name.clone());
        entries.push(Entry::item(format!("Kick {name}"), move |window, cx| {
            crate::panels::open(store.clone(), crate::panels::What::Kick { guild, user, name: who.clone(), handle: handle.clone() }, window, cx);
        }).danger());
    }
    entries.push(Entry::Separator);
    entries.push(Entry::copy("Copy User ID", user.to_string()).icon(IconName::Copy));
    menu::show(position, entries, cx);
}

fn dm_menu(
    dm: &sidebar::DmRow,
    store: &gpui_kit::Entity<crate::store::Store>,
    position: gpui_kit::Point<gpui_kit::Pixels>,
    cx: &gpui_kit::App,
) -> Vec<crate::context_menu::Entry> {
    use crate::context_menu::Entry;
    let channel = dm.id;
    let unread = {
        let cache = store.read(cx).cache();
        cache.is_unread(channel) || cache.mentions(channel) > 0
    };
    let mut entries = vec![if unread {
        let store = store.clone();
        Entry::item("Mark As Read", move |_, cx| store.update(cx, |store, cx| store.mark_read(channel, cx)))
    } else {
        Entry::later("Mark As Read")
    }];
    entries.push(Entry::Separator);
    entries.push(Entry::later("Pin").picture("icons/toolbar-pin.svg"));
    entries.push(Entry::later("Add to Favorites").icon(IconName::Star));
    entries.push(Entry::Separator);
    match dm.user {
        Some(user) => {
            entries.push(Entry::item("Profile", move |_, cx| crate::links::show_profile(user, position, cx)));
            if !dm.bot {
                entries.push(Entry::later("Start a Call"));
            }
            let name = dm.label.clone();
            {
                let (store, name) = (store.clone(), name.clone());
                entries.push(Entry::item("Add Note", move |window, cx| {
                    crate::panels::open(store.clone(), crate::panels::What::Note { user, name: name.clone() }, window, cx);
                }).hint("Only visible to you"));
            }
            if dm.friend {
                let current = store.read(cx).cache().friend_nickname(user).map(str::to_string);
                let (store, name) = (store.clone(), name.clone());
                let label = if current.is_some() { "Change Friend Nickname" } else { "Add Friend Nickname" };
                entries.push(Entry::item(label, move |window, cx| {
                    crate::panels::open(store.clone(), crate::panels::What::FriendNickname { user, name: name.clone(), current: current.clone().unwrap_or_default() }, window, cx);
                }));
            }
            let closing = store.clone();
            entries.push(Entry::item("Close DM", move |_, cx| closing.update(cx, |store, cx| store.close_dm(channel, cx))));
            entries.push(Entry::Separator);
            entries.push(Entry::later("Apps"));
            if dm.friend {
                entries.push(Entry::later("Invite to Server"));
                let (store, who) = (store.clone(), name.clone());
                entries.push(Entry::item("Remove Friend", move |_, cx| {
                    let store = store.clone();
                    crate::confirm::ask(format!("Remove {who}"), format!("Remove {who} from your friends?"), "Remove Friend", move |cx| {
                        store.update(cx, |store, cx| store.remove_friend(user, cx));
                    }, cx);
                }));
            }
            let ignored = crate::places::is_ignored(user);
            entries.push(Entry::item(if ignored { "Unignore" } else { "Ignore" }, move |_, cx| {
                crate::places::set_ignored(user, !ignored);
                crate::ui::redraw_chat(cx);
            }));
            let (store, who) = (store.clone(), name.clone());
            entries.push(Entry::item("Block", move |_, cx| {
                let store = store.clone();
                crate::confirm::ask(format!("Block {who}"), format!("Block {who}? They won't be able to message you, and you won't see them in your friends."), "Block", move |cx| {
                    store.update(cx, |store, cx| store.block_user(user, cx));
                }, cx);
            }).danger());
            entries.push(Entry::Separator);
            entries.push(Entry::later(format!("Mute @{}", dm.label)));
            entries.push(Entry::Separator);
            entries.push(Entry::copy("Copy User ID", user.to_string()).icon(IconName::Copy));
        }
        None => {
            {
                let current = store.read(cx).cache().channel(channel).and_then(|c| c.name.clone()).unwrap_or_default();
                let store = store.clone();
                entries.push(Entry::item("Edit Group", move |window, cx| {
                    crate::panels::open(store.clone(), crate::panels::What::GroupName { channel, current: current.clone() }, window, cx);
                }));
            }
            entries.push(Entry::Separator);
            entries.push(Entry::later("Mute Conversation"));
            entries.push(Entry::Separator);
            let (store, label) = (store.clone(), dm.label.clone());
            entries.push(Entry::item("Leave Group", move |_, cx| {
                let store = store.clone();
                crate::confirm::ask(format!("Leave {label}"), format!("Leave {label}? You won't see its messages unless someone adds you back."), "Leave Group", move |cx| {
                    store.update(cx, |store, cx| store.close_dm(channel, cx));
                }, cx);
            }).danger());
            entries.push(Entry::Separator);
        }
    }
    entries.push(Entry::copy("Copy Channel ID", channel.to_string()).icon(IconName::Copy));
    entries
}

/// How far to scroll for item `ix` to be in view, clear of the panel over the
/// bottom of the list. `None` when it already is, or isn't laid out.
fn distance_to_show(scroll: &ScrollHandle, ix: usize) -> Option<f32> {
    let (view, offset) = (scroll.bounds(), scroll.offset().y);
    let item = scroll.bounds_for_item(ix)?;
    let (top, bottom) = (f32::from(item.top() + offset - view.top()), f32::from(item.bottom() + offset - view.top()));
    reveal_distance(top, bottom, f32::from(view.size.height))
}

/// `top` and `bottom` are the item's edges from the top of a `height` tall view.
fn reveal_distance(top: f32, bottom: f32, height: f32) -> Option<f32> {
    // Clear of the header too, when it sits over a server banner.
    const CLEAR_ABOVE: f32 = 60.;
    const CLEAR_BELOW: f32 = PANEL_HEIGHT + 32.;
    if top < CLEAR_ABOVE {
        Some(top - CLEAR_ABOVE)
    } else if bottom > height - CLEAR_BELOW {
        Some(bottom - (height - CLEAR_BELOW))
    } else {
        None
    }
}

/// How far outside the view rows stay fully built: the list moves as a layer, painted this far
/// and a bit, between rebuilds. So none shows up empty mid-glide.
const ROWS_KEPT_BEYOND_VIEW: f32 = crate::smooth_scroll::LAYER_OVERSCAN + crate::smooth_scroll::KEPT_BEYOND_LAYER;

/// Whether a row spanning `top..bottom` (as drawn, scrolled) is out of reach of the view.
fn out_of_reach(top: f32, bottom: f32, view_top: f32, view_bottom: f32) -> bool {
    bottom < view_top - ROWS_KEPT_BEYOND_VIEW || top > view_bottom + ROWS_KEPT_BEYOND_VIEW
}

/// The height to keep for row `ix` if it's out of reach; `None` to build it for real.
/// Measured up to the next row, so that its margin stays too.
fn room_for_unseen_row(scroll: &ScrollHandle, ix: usize, places_known: bool) -> Option<f32> {
    if !places_known {
        return None;
    }
    let item = scroll.bounds_for_item(ix)?;
    let height = f32::from(scroll.bounds_for_item(ix + 1).map_or(item.size.height, |next| next.top() - item.top()));
    if height <= 0. {
        return None;
    }
    let (view, offset) = (scroll.bounds(), scroll.offset().y);
    let top = f32::from(item.top() + offset);
    out_of_reach(top, top + height, f32::from(view.top()), f32::from(view.bottom())).then_some(height)
}

/// How many mentions each pill would show with the list's top at `top`, and which row each goes to.
fn pills_shown(scroll: &ScrollHandle, top: f32, pinged: &[(usize, u32)]) -> ((u32, Option<usize>), (u32, Option<usize>)) {
    let view = scroll.bounds();
    let offset = gpui_kit::px(-top);
    let (mut above, mut below) = ((0, None), (0, None));
    for (ix, count) in pinged {
        let Some(item) = scroll.bounds_for_item(*ix) else { continue };
        if item.bottom() + offset <= view.top() {
            above = (above.0 + count, Some(*ix));
        } else if item.top() + offset >= view.bottom() {
            below = (below.0 + count, below.1.or(Some(*ix)));
        }
    }
    (above, below)
}

fn mention_pills(scroll: &ScrollHandle, scroller: &SmoothScroller, smooth: bool, pinged: &[(usize, u32)], cx: &Context<SidePane>) -> Vec<AnyElement> {
    let (above, below) = pills_shown(scroll, -f32::from(scroll.offset().y), pinged);
    let danger = cx.theme().danger;
    let pill = |id: &'static str, (count, ix): (u32, Option<usize>), top: bool| {
        let ix = ix?;
        let (scroll, scroller) = (scroll.clone(), scroller.clone());
        let label = format!("{count} NEW MENTION{}", if count == 1 { "" } else { "S" });
        Some(
            h_flex()
                .absolute()
                .left_0()
                .right_0()
                .justify_center()
                .map(|d| if top { d.top(u(8.)) } else { d.bottom(u(72.)) })
                .child(
                    div()
                        .id(id)
                        .h(u(24.))
                        .px(u(12.))
                        .flex()
                        .items_center()
                        .rounded_full()
                        .bg(danger)
                        .text_color(gpui_kit::white())
                        .text_size(u(12.))
                        .font_weight(FontWeight::BOLD)
                        .shadow_md()
                        .cursor_pointer()
                        .on_click(move |_, window, _| {
                            match distance_to_show(&scroll, ix).filter(|_| smooth) {
                                Some(distance) => scroller.glide_by(distance),
                                None => scroll.scroll_to_item(ix),
                            }
                            window.refresh();
                        })
                        .child(label),
                )
                .into_any_element(),
        )
    };
    [pill("mentions-above", above, true), pill("mentions-below", below, false)].into_iter().flatten().collect()
}

/// Server icon size in the rail, and its corner radius when hovered or active.
const RAIL_ICON: f32 = 40.;
const RAIL_SQUIRCLE: f32 = 12.;
/// Rail width, the icons' left inset, and the gap between icons (measured).
const RAIL_WIDTH: f32 = 72.;
const RAIL_PAD: f32 = 16.;
const RAIL_GAP: f32 = 8.;
/// Height of the white pill on the rail's edge: the active server, and hover.
const PILL_ACTIVE: f32 = 40.;
const PILL_HOVER: f32 = 20.;
const PILL_UNREAD: f32 = 8.;
/// Discord's online green (also the rail's hover green).
/// Channel rows: inset from the column's left, and from the right (scrollbar room).
/// Two faces filling a `size` square, as Discord shows a group with no picture: one at the top
/// left, the other over it at the bottom right, cut out of it by a ring of `ring`.
fn stacked_faces(faces: &[String], size: f32, ring: gpui_kit::Hsla) -> AnyElement {
    let face = size * 0.625;
    let edge = (size / 16.).max(1.5);
    let picture = |url: &String| img(url.clone()).size_full().rounded_full().object_fit(ObjectFit::Cover);
    div()
        .relative()
        .size(u(size))
        .flex_shrink_0()
        .child(div().absolute().top_0().left_0().size(u(face)).child(picture(&faces[0])))
        .child(
            div()
                .absolute()
                .right(u(-edge))
                .bottom(u(-edge))
                .size(u(face + edge * 2.))
                .rounded_full()
                .border(u(edge))
                .border_color(ring)
                .bg(ring)
                .child(picture(&faces[1])),
        )
        .into_any_element()
}

const ROW_LEFT: f32 = 8.;
const ROW_RIGHT: f32 = 8.;
const GREEN: u32 = 0x23a55a;

/// The raised surface of the rail's tiles: halfway up to the selection colour.
fn raised(theme: &gpui_kit::component::Theme) -> Hsla {
    theme.sidebar.blend(theme.sidebar_accent.opacity(0.5))
}

/// The user panel's surface: a little darker than the rail's tiles.
fn panel(theme: &gpui_kit::component::Theme) -> Hsla {
    theme.sidebar.blend(theme.sidebar_accent.opacity(0.4))
}
const PANEL_HEIGHT: f32 = 58.;

/// "Rust Friends" → "RF", as Discord shows servers without an icon.
fn initials(name: &str) -> String {
    name.split_whitespace().filter_map(|w| w.chars().next()).take(3).collect()
}

#[cfg(test)]
mod reveal_tests {
    use super::reveal_distance;

    #[test]
    fn scrolls_only_as_far_as_it_takes() {
        assert_eq!(reveal_distance(100., 132., 800.), None);
        assert_eq!(reveal_distance(-50., -18., 800.), Some(-110.));
        let below = reveal_distance(900., 932., 800.).unwrap();
        assert!(below > 132.);
        assert_eq!(reveal_distance(100. - below, 132. - below, 800.).map(|_| ()), Some(()));
        assert_eq!(reveal_distance(900. - below, 932. - below, 800.), None);
    }
}

#[cfg(test)]
mod reach_tests {
    #[test]
    fn only_rows_well_outside_the_view_are_out_of_reach() {
        assert!(!super::out_of_reach(100., 140., 0., 800.), "on screen");
        assert!(!super::out_of_reach(1300., 1340., 0., 800.), "just below: kept for the glide");
        assert!(!super::out_of_reach(1590., 1630., 0., 800.), "still within what a layer may move into, and a bit");
        assert!(super::out_of_reach(1700., 1740., 0., 800.));
        assert!(super::out_of_reach(-900., -860., 0., 800.));
    }
}
