//! The main pane under /channels/…: a channel's messages and the composer,
//! or a short explanation when there is nothing to chat in.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use cord_core::Id;
use cord_core::cache::{Cache, ChatMessage, SendState};
use cord_core::model::{ChannelKind, Message, ReactionEmoji};
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::button::Button;
use gpui_kit::component::input::{InlineToken, InlineTokenContext, InputEvent, Textarea, TextareaState};
use gpui_kit::component::button::ButtonVariants;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, Selectable, Sizable, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnimationExt, AnyElement, AnyView, App, AppContext, ClickEvent, Context, Entity, FollowMode, FontWeight, InteractiveElement,
    IntoElement, Hsla, ListAlignment, ListState, ParentElement, Pixels, Point, Render, StatefulInteractiveElement, Styled,
    SharedString, StyledImage, Subscription, WeakEntity, Window, div, img, list,
};

use crate::ui::u;
use crate::chat_format;
use crate::markdown_view::{Env, ParseCache, Shared};
use crate::members::{self, MembersEvent, MembersPanel};
use crate::nav;
use crate::profile_card::{ProfileCard, ProfileEvent};
use crate::rich_view;
use crate::router::{RouteContext, navigate};
use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::store::{History, Store};
use crate::toasts::Toast;

pub struct ChannelPage {
    ctx: RouteContext,
    channel: Option<Id>,
    list: ListState,
    scroller: SmoothScroller,
    rendered: Vec<Id>,
    /// What each rendered row looked like, to find rows whose content changed.
    revisions: HashMap<Id, u64>,
    input: Entity<TextareaState>,
    /// Spoilers the viewer has clicked open: (message, spoiler group).
    revealed: Rc<RefCell<HashSet<(Id, u16)>>>,
    /// Markdown parsed for the rows drawn so far. Goes with the page.
    parsed: Rc<RefCell<ParseCache>>,
    /// The server this channel belongs to; none in a DM.
    guild: Option<Id>,
    /// The member list on the right, while it is shown.
    members: Option<(Entity<MembersPanel>, Subscription)>,
    /// The open profile card and what listens to it.
    profile: Option<(Entity<ProfileCard>, Subscription)>,
    /// The `:shortcode` emoji popup over the composer, while one is typed.
    suggest: Option<Suggest>,
    /// The blue "new messages" banner was dismissed with Mark As Read.
    banner_closed: bool,
    /// Scrolled up from the newest message since opening the channel.
    left_tail: bool,
    /// A linked message to scroll to once it's loaded, and how many older
    /// pages were fetched looking for it.
    jump_target: Option<(Id, u16)>,
    /// The message lit up after jumping to it, and which jump that was: a
    /// repeat click replays the glow.
    highlight: Rc<std::cell::Cell<Option<(Id, usize)>>>,
    jumps: usize,
    /// The row under the pointer: its gradient name moves.
    hovered_row: Rc<std::cell::Cell<Option<Id>>>,
    /// The row whose quick actions are under the pointer: still "hovered",
    /// though they hang outside it, over the row above.
    hovered_tools: Rc<std::cell::Cell<Option<Id>>>,
    /// The message being replied to, and whose it is.
    replying: Option<(Id, String, Option<Id>)>,
    /// The reply pings them: Discord's "@ ON". Starts on for each reply.
    reply_ping: bool,
    /// Who wrote the message being replied to is us: no ping to offer.
    replying_to_self: bool,
    /// Authors already asked of the gateway, so each is asked once.
    asked_members: HashSet<Id>,
    /// Messages that arrived below while scrolled up.
    unseen: usize,
    /// In a voice channel: whether its chat panel is open, how wide, and
    /// whether its edge is being dragged.
    vc_chat_open: bool,
    vc_scroll: (gpui_kit::ScrollHandle, crate::smooth_scroll::SmoothScroller),
    poll_ui: Rc<RefCell<crate::markdown_view::PollUi>>,
    /// The voice stage's width last frame, in pixels.
    vc_width: Rc<std::cell::Cell<f32>>,
    /// Where the view was last frame: its top row, and how far into it.
    anchor: Option<Option<(Id, f32)>>,
    was_gliding: std::cell::Cell<bool>,
    /// A row being jumped to, and how many more times to check it landed in full.
    jump_settle: Option<(usize, u8)>,
    vc_chat_width: f32,
    resizing: bool,
    /// The member list's edge is being dragged.
    resizing_members: bool,
    /// Files waiting above the message box.
    pending: Vec<crate::uploads::Pending>,
    /// A row to glide to, once the list has drawn it.
    /// The flag: to its bottom edge, at the newest message.
    glide_to: Option<(usize, bool)>,
    /// Whose card closed last, and when: see `toggled_shut`.
    last_closed: Option<(Id, std::time::Instant)>,
    /// The message box is editing this message instead of writing a new one.
    editing: Option<Id>,
    /// Asking "Delete this message?" about this one.
    confirm_delete: Option<Id>,
    /// That question is fading out.
    delete_closing: bool,
    /// The composer says "Message #name" (not just "Message") yet.
    placeholder_named: bool,
    /// RUSTCORD_BENCH=1: scroll on its own every frame and log the frame rate.
    bench: Option<Bench>,
    _subscriptions: Vec<Subscription>,
}

/// How long the delete question takes to fade in or out.
const DELETE_FADE: std::time::Duration = std::time::Duration::from_millis(180);

struct Bench {
    since: std::time::Instant,
    frames: u32,
    step: u32,
    last: Option<std::time::Instant>,
    intervals: crate::perf::Intervals,
}

impl Bench {
    fn new() -> Self {
        Bench { since: std::time::Instant::now(), frames: 0, step: 0, last: None, intervals: Default::default() }
    }

    /// One frame: scroll 40px (600 frames up, then 600 down) and log every 2s.
    fn tick(&mut self, list: &ListState, scroller: &SmoothScroller, window: &mut Window) {
        self.step += 1;
        let now = std::time::Instant::now();
        if let Some(last) = self.last.replace(now) {
            self.intervals.push((now - last).as_secs_f32());
        }
        // Far trips (600 frames each way) so new rows, images and older pages keep arriving.
        let up = (self.step / 600) % 2 == 0;
        let dy = if up { -40. } else { 40. };
        let mode = std::env::var("RUSTCORD_BENCH").unwrap_or_default();
        // RUSTCORD_BENCH=wheel: the way a person scrolls, a notch about every 110 ms
        // (20 frames at 180 Hz), through the real glide. =idle: not scrolling at all.
        if mode == "idle" {
        } else if mode == "wheel" {
            if self.step % 20 == 0 {
                let up = (self.step / 1200) % 2 == 0;
                scroller.glide_by(if up { -150. } else { 150. });
            }
        // RUSTCORD_BENCH=scrollbar scrolls the way the smooth scroller used to.
        } else if mode == "scrollbar" {
            let top = -list.scroll_px_offset_for_scrollbar().y;
            list.set_offset_from_scrollbar(gpui_kit::point(gpui_kit::px(0.), -(top + gpui_kit::px(dy))));
        } else {
            list.scroll_by(gpui_kit::px(dy));
        }
        self.frames += 1;
        let elapsed = self.since.elapsed();
        if elapsed.as_secs_f32() >= 2. {
            let size = window.viewport_size();
            tracing::info!(
                fps = format!("{:.1}", self.frames as f32 / elapsed.as_secs_f32()),
                width = f32::from(size.width),
                height = f32::from(size.height),
                frames = %self.intervals.take_summary(1000. / 180.),
                gpui = %crate::perf::frame_line(),
                "bench"
            );
            self.since = std::time::Instant::now();
            self.frames = 0;
        }
        window.request_animation_frame();
    }
}

struct Suggest {
    /// Byte offset of the query's `:` or `@`, and of the cursor after it.
    start: usize,
    cursor: usize,
    query: String,
    mention: bool,
    items: Vec<SuggestItem>,
    selected: usize,
}

/// One row of the composer popup: an emoji or a person.
#[derive(Clone, PartialEq)]
struct SuggestItem {
    /// The emoji, or the person's avatar URL.
    emoji: Option<&'static str>,
    avatar: Option<String>,
    label: String,
    /// `:code:`, or the username beside a display name.
    detail: String,
    /// What gets inserted when it's a mention.
    mention: Option<PingTarget>,
    /// A server's emoji: its name, id, and whether it's animated.
    custom: Option<(String, Id, bool)>,
}

/// Who a mention in the popup pings.
#[derive(Clone, Copy, PartialEq)]
enum PingTarget {
    User(Id),
    Role(Id),
    /// `@everyone` or `@here`.
    Everyone(&'static str),
}

impl ChannelPage {
    fn new(ctx: RouteContext, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let channel = ctx.param("channel").and_then(|s| s.parse().ok()).map(Id);
        // The placeholder lives on the input state; the page is rebuilt per channel.
        let named = channel.and_then(|id| placeholder_for(ctx.services.store.read(cx).cache(), id));
        let placeholder_named = named.is_some();
        let input = cx.new(|cx| {
            // Enter sends, Shift+Enter starts a new line, and the box grows with them.
            TextareaState::new(window, cx).auto_grow(1, 8).submit_on_enter(true).placeholder(named.unwrap_or_else(|| "Message".into()))
        });
        // Rows are measured this far beyond the screen, so they've settled their
        // height before scrolling in, and the chat doesn't shift as they do.
        let list = ListState::new(0, ListAlignment::Bottom, gpui_kit::px(1500.));
        if let Some(channel) = channel {
            let (store, guild_id) = (ctx.services.store.clone(), ctx.param("guild").and_then(|g| g.parse().ok()).map(Id));
            crate::viewer::watch_gallery(std::rc::Rc::new(move |cx: &App| {
                let store = store.read(cx);
                let cache = store.cache();
                let Some(messages) = cache.messages(channel) else { return Vec::new() };
                let now = chrono::Local::now();
                let mut pictures = Vec::new();
                for m in messages.iter() {
                    let message = &m.message;
                    let mut add = |src: &str, open: &str, dims: Option<(u32, u32)>| {
                        pictures.push(crate::viewer::Shown {
                            src: src.to_string(),
                            open: open.to_string(),
                            dims,
                            author: cache.display_name(guild_id, &message.author).to_string(),
                            avatar: cache.avatar_url(guild_id, &message.author, 64),
                            when: nav::format_message_time(message.id.timestamp_ms(), &now),
                        });
                    };
                    for a in message.attachments.iter().filter(|a| a.is_image()) {
                        add(a.display_url(), &a.url, a.dimensions());
                    }
                    for e in &message.embeds {
                        if let Some(media) = e.image.as_ref().or(e.thumbnail.as_ref()) {
                            add(media.display_url(), &media.url, media.dimensions());
                        }
                    }
                }
                pictures
            }));
        }
        if let Some(channel) = channel {
            let (rows, store) = (list.clone(), ctx.services.store.clone());
            crate::anim::watch_rows(std::rc::Rc::new(move |screens: f32, cx: &App| {
                let extra = rows.viewport_bounds().size.height * screens;
                let store = store.read(cx);
                let Some(messages) = store.cache().messages(channel) else { return Vec::new() };
                rows.rows_near_view(extra)
                    .filter_map(|ix| messages.get(ix.checked_sub(1)?).map(|m| m.message.id))
                    .collect()
            }));
        }
        list.set_follow_mode(FollowMode::Tail);

        crate::file_preview::set_core(ctx.services.core.clone());
        let subscriptions = vec![
            cx.observe_window_activation(window, |_, window, _| {
                crate::store::WINDOW_ACTIVE.store(window.is_window_active(), std::sync::atomic::Ordering::Relaxed);
            }),
            // GIFs rest while the window isn't focused.
            cx.observe_window_activation(window, |_, window, cx| crate::gifs::set_playing(window.is_window_active(), cx)),
            cx.observe_in(&ctx.services.store, window, |this, _, window, cx| {
                // The cache may not have been ready when the page opened.
                // Opened before READY: name the channel once it's known.
                if !this.placeholder_named {
                    if let Some(text) = this.channel.and_then(|id| placeholder_for(this.ctx.services.store.read(cx).cache(), id)) {
                        this.placeholder_named = true;
                        this.input.update(cx, |input, cx| input.set_placeholder(text, window, cx));
                    }
                }
                let guild = this.server(cx);
                if guild != this.guild {
                    this.guild = guild;
                    this.sync_members(window, cx);
                }
                this.sync_list(cx);
                // Members found by the gateway search show up as they arrive.
                if this.suggest.as_ref().is_some_and(|s| s.mention) {
                    this.refresh_suggestion(false, cx);
                }
                cx.notify();
            }),
            // Text size and compact mode change every row's height.
            cx.observe_in(&ctx.services.settings, window, |this, _, window, cx| {
                this.list.remeasure();
                this.sync_members(window, cx);
                cx.notify();
            }),
            cx.subscribe_in(&input, window, |this, _, event, window, cx| match event {
                InputEvent::PressEnter { shift: true, .. } => {}
                InputEvent::PressEnter { .. } if this.suggest.is_some() => this.accept_suggestion(window, cx),
                InputEvent::PressEnter { .. } => this.send(window, cx),
                InputEvent::Change => {
                    this.keep_draft(cx);
                    // A conversion changes the text, which lands back here.
                    if !this.convert_typed(window, cx) {
                        this.style_composer(cx);
                        this.update_suggestion(cx);
                    }
                }
                InputEvent::Blur => {
                    this.suggest = None;
                    cx.notify();
                }
                _ => {}
            }),
            // Arrow keys, Tab and Escape steer the emoji popup before the
            // input sees them (Enter arrives above as PressEnter).
            {
                let page = cx.entity().downgrade();
                cx.intercept_keystrokes(move |event, window, cx| {
                    let key = event.keystroke.key.as_str();
                    if !matches!(key, "up" | "down" | "tab" | "escape") || event.keystroke.modifiers.number_of_modifiers() != 0 {
                        return;
                    }
                    let _ = page.update(cx, |page, cx| {
                        if page.suggest.is_none() {
                            // Up in an empty box: edit our latest message.
                            let in_box = gpui_kit::Focusable::focus_handle(page.input.read(cx), cx).is_focused(window);
                            // Whatever's on top of the page gets Esc first, and handles it itself.
                            if key == "escape" && (page.profile.is_some() || crate::viewer::is_open(cx) || crate::context_menu::is_open(cx) || crate::confirm::is_open(cx) || crate::add_people::is_open(cx) || crate::panels::is_open(cx)) {
                                return;
                            }
                            if key == "up" && in_box && page.editing.is_none() && page.input.read(cx).value().is_empty() && page.pending.is_empty() {
                                let last = page.channel.and_then(|channel| {
                                    let cache = page.ctx.services.store.read(cx).cache();
                                    let me = cache.me()?.id;
                                    let messages = cache.messages(channel)?;
                                    messages.iter().rev().find(|m| m.message.author.id == me && m.state == SendState::Sent && matches!(m.message.kind, 0 | 19)).map(|m| (m.message.id, m.message.content.clone()))
                                });
                                if let Some((id, content)) = last {
                                    page.start_edit(id, content, window, cx);
                                    cx.notify();
                                    cx.stop_propagation();
                                }
                            } else if key == "escape" && page.confirm_delete.is_some() {
                                page.close_delete(cx);
                                cx.stop_propagation();
                            } else if key == "escape" && page.editing.is_some() {
                                page.leave_edit(window, cx);
                                cx.notify();
                                cx.stop_propagation();
                            } else if key == "escape" && page.replying.take().is_some() {
                                cx.notify();
                                cx.stop_propagation();
                            } else if key == "escape" && in_box && !page.pending.is_empty() {
                                // Only what's waiting to be sent: the text stays.
                                for file in std::mem::take(&mut page.pending) {
                                    file.discard();
                                }
                                page.keep_files(cx);
                                cx.notify();
                                cx.stop_propagation();
                            } else if key == "escape" && !page.list.is_following_tail() {
                                // Discord's Esc: back to the newest message, unread cleared.
                                page.banner_closed = true;
                                page.jump_to_present(cx);
                                cx.stop_propagation();
                            }
                            return;
                        }
                        match key {
                            "up" => page.move_suggestion(-1, cx),
                            "down" => page.move_suggestion(1, cx),
                            "tab" => page.accept_suggestion(window, cx),
                            _ => {
                                page.suggest = None;
                                cx.notify();
                            }
                        }
                        cx.stop_propagation();
                    });
                })
            },
        ];

        if let Some(channel) = channel {
            // Forum and voice channels have no chat to load (an uncached one may
            // just mean READY hasn't arrived yet).
            let chat = ctx.services.store.read(cx).cache().channel(channel).is_none_or(|c| has_chat(c.kind));
            if chat {
                ctx.services.store.update(cx, |store, cx| store.open_channel(channel, cx));
            }
            input.update(cx, |input, cx| input.focus(window, cx));
        }

        let mut page = ChannelPage {
            ctx,
            channel,
            scroller: SmoothScroller::new(ScrollTarget::List(list.clone())).calm_hover().layered(),
            list,
            rendered: Vec::new(),
            revisions: HashMap::new(),
            input,
            revealed: Rc::default(),
            parsed: Rc::default(),
            guild: None,
            members: None,
            profile: None,
            suggest: None,
            banner_closed: false,
            left_tail: false,
            placeholder_named,
            jump_target: None,
            hovered_row: Rc::default(),
            editing: None,
            confirm_delete: None,
            delete_closing: false,
            last_closed: None,
            glide_to: None,
            pending: Vec::new(),
            hovered_tools: Rc::default(),
            asked_members: HashSet::new(),
            vc_chat_open: true,
            vc_scroll: {
                let handle = gpui_kit::ScrollHandle::new();
                (handle.clone(), crate::smooth_scroll::SmoothScroller::new(crate::smooth_scroll::ScrollTarget::Handle(handle)))
            },
            poll_ui: Rc::default(),
            vc_width: Rc::default(),
            anchor: None,
            was_gliding: Default::default(),
            jump_settle: None,
            vc_chat_width: cx.try_global::<VoiceChatWidth>().map_or(VC_CHAT_DEFAULT, |w| w.0),
            resizing: false,
            resizing_members: false,
            unseen: 0,
            replying: None,
            reply_ping: true,
            replying_to_self: false,
            highlight: Rc::default(),
            jumps: 0,
            bench: std::env::var_os("RUSTCORD_BENCH").map(|_| Bench::new()),
            _subscriptions: subscriptions,
        };
        page.guild = page.server(cx);
        page.sync_members(window, cx);
        page.restore_draft(window, cx);
        page.sync_list(cx);
        page
    }

    fn is_voice(&self, cx: &App) -> bool {
        let store = self.ctx.services.store.read(cx);
        self.channel.and_then(|c| store.cache().channel(c)).is_some_and(|c| matches!(c.kind, ChannelKind::Voice | ChannelKind::Stage))
    }

    /// The left side of a voice channel's page: who's in it, as tiles.
    fn voice_stage(&self, channel: Id, label: &str, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let store = self.ctx.services.store.read(cx);
        let people = self.guild.map(|g| crate::sidebar::participants(store.cache(), g, channel)).unwrap_or_default();
        let (muted, tile) = (theme.muted_foreground, theme.popover);
        // As many 16:9 tiles to a row as fit, never squeezed below the minimum.
        let rem = f32::from(window.rem_size()) / 16.;
        let room = (self.vc_width.get() / rem).max(320.) - 32.;
        let (columns, width, height) = voice_tile_size(room, people.len());
        let core = self.ctx.services.core.clone();
        let speaking_now = self.ctx.services.store.read(cx).speaking().clone();
        let mut tiles = people.into_iter().map(|p| {
            let speaking = speaking_now.contains(&p.user_id);
            let color = crate::avatar_colors::color(&p.avatar_url, &core, cx);
            let (user, name) = (p.user_id, p.name.clone());
            div()
                .relative()
                .flex()
                .items_center()
                .justify_center()
                .w(u(width))
                .h(u(height))
                .flex_shrink_0()
                .rounded(u(8.))
                .overflow_hidden()
                .bg(color.map_or(tile, |c| gpui_kit::rgb(c).into()))
                .when(speaking, |d| {
                    d.child(div().absolute().top_0().left_0().size_full().rounded(u(8.)).border_3().border_color(Hsla::from(gpui_kit::rgb(0x23a55a))))
                })
                .on_mouse_down(gpui_kit::MouseButton::Right, cx.listener(move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.user_menu(user, name.clone(), event.position, cx);
                }))
                .child(members::avatar_with_status(
                    Avatar::new().border_0().src(p.avatar_url.replace("size=64", "size=160")).name(p.name.clone()),
                    80.,
                    None,
                    false,
                    p.decoration.clone(),
                    tile,
                ))
                .child(
                    h_flex()
                        .absolute()
                        .left(u(8.))
                        .bottom(u(8.))
                        .max_w(gpui_kit::relative(0.9))
                        .gap(u(5.))
                        .px(u(8.))
                        .py(u(4.))
                        .items_center()
                        .rounded(u(6.))
                        .bg(gpui_kit::black().opacity(0.7))
                        .text_color(gpui_kit::white())
                        .text_size(u(14.))
                        .when(p.muted, |d| d.child(Icon::new(IconName::MicOff).size(u(15.)).flex_shrink_0()))
                        .when(p.deafened, |d| d.child(Icon::new(IconName::HeadphoneOff).size(u(15.)).flex_shrink_0()))
                        .child(div().min_w_0().truncate().when_some(p.font, |d, font| d.font_family(font)).child(p.name)),
                )
        });
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .bg(gpui_kit::black())
            .child(
                h_flex()
                    .h(u(49.))
                    .flex_shrink_0()
                    .px(u(16.))
                    .gap(u(8.))
                    .items_center()
                    .child(Icon::new(IconName::Volume2).size(u(24.)).text_color(muted))
                    .child(div().flex_1().min_w_0().truncate().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(label.to_string()))
                    .when(!self.vc_chat_open, |d| {
                        d.child(tool_button("open-vc-chat", IconName::MessageCircle, "Open Chat", cx).on_click(cx.listener(|this, _, _, cx| {
                            this.vc_chat_open = true;
                            cx.notify();
                        })))
                    }),
            )
            .child(
                // Many people: the tiles scroll instead of running off the page.
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div().id("vc-stage").size_full().overflow_y_scroll().track_scroll(&self.vc_scroll.0).child(
                            // Rows of our own, not wrapping: the width is last frame's,
                            // and while resizing wrapping would redo them back and forth.
                            v_flex().w_full().p(u(16.)).gap(u(8.)).children(std::iter::from_fn(|| {
                                let row: Vec<_> = tiles.by_ref().take(columns).collect();
                                (!row.is_empty()).then(|| h_flex().w_full().flex_shrink_0().gap(u(8.)).justify_center().children(row))
                            })),
                        ),
                    )
                    .when(self.ctx.services.settings.read(cx).get().smooth_scrolling, |d| d.child(self.vc_scroll.1.layer()))
                    .child({
                        let width = self.vc_width.clone();
                        gpui_kit::canvas(
                            move |bounds, window, _| {
                                let now = f32::from(bounds.size.width);
                                if width.replace(now) != now {
                                    window.request_animation_frame();
                                }
                            },
                            |_, _, _, _| {},
                        )
                        .absolute()
                        .top_0()
                        .left_0()
                        .size_full()
                    }),
            )
            .into_any_element()
    }

    /// The server this channel belongs to, once the cache knows the channel.
    fn server(&self, cx: &App) -> Option<Id> {
        let channel = self.ctx.services.store.read(cx).cache().channel(self.channel?)?;
        has_chat(channel.kind).then_some(channel.guild_id).flatten()
    }

    /// Shows or hides the member list to match the setting and the channel.
    /// Only server text channels have one; elsewhere nothing is subscribed.
    fn sync_members(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // A voice channel's chat is a side panel, with no member list.
        let show = self.ctx.services.settings.read(cx).get().show_members && !self.is_voice(cx);
        let (Some(guild), Some(channel), true) = (self.guild, self.channel, show) else {
            if self.members.take().is_some() || self.guild.is_none() {
                self.ctx.services.store.update(cx, |store, cx| store.leave_members(cx));
            }
            return;
        };
        if self.members.is_some() {
            return;
        }
        let panel = members::new(self.ctx.services.clone(), guild, channel, cx);
        let subscription = cx.subscribe_in(&panel, window, |this, _, event: &MembersEvent, window, cx| match event {
            // Clicked in the list: the card opens beside it, not over the names.
            MembersEvent::OpenProfile { user, position } => {
                let list = this.ctx.services.settings.read(cx).get().members_width;
                let edge = window.viewport_size().width - window.rem_size() * (list / 16.) - gpui_kit::px(8.);
                if !this.toggled_shut(*user, window, cx) {
                    this.show_profile(*user, gpui_kit::point(edge, position.y), true, window, cx)
                }
            }
            MembersEvent::OpenMenu { user, name, position } => this.user_menu(*user, name.clone(), *position, cx),
        });
        self.members = Some((panel, subscription));
    }

    fn toggle_members(&mut self, cx: &mut Context<Self>) {
        let show = !self.ctx.services.settings.read(cx).get().show_members;
        self.ctx.services.settings.update(cx, |settings, cx| settings.update(cx, |s| s.show_members = show));
    }

    /// A plain click on a name or avatar: the profile card, at the click.
    fn open_profile(&mut self, user: Id, position: Point<Pixels>, window: &mut Window, cx: &mut Context<Self>) {
        // Clicking the same person again closes it.
        if self.toggled_shut(user, window, cx) {
            return;
        }
        self.show_profile(user, position, false, window, cx);
    }

    fn show_profile(&mut self, user: Id, position: Point<Pixels>, beside: bool, window: &mut Window, cx: &mut Context<Self>) {
        let (services, guild) = (self.ctx.services.clone(), self.guild);
        let card = cx.new(|cx| ProfileCard::new(services, user, guild, position, beside, window, cx));
        let subscription = cx.subscribe_in(&card, window, |this, _, event: &ProfileEvent, window, cx| match event {
            ProfileEvent::Close => this.close_profile(window, cx),
            ProfileEvent::Message(user) => this.message(*user, cx),
            ProfileEvent::OpenFull(user) => {
                let position = window.mouse_position();
                this.show_profile(*user, position, false, window, cx);
                if let Some((card, _)) = &this.profile {
                    card.update(cx, |card, _| card.open_full());
                }
            }
        });
        self.profile = Some((card, subscription));
        cx.notify();
    }

    /// A click on whoever's card is open closes it, in the member and voice
    /// lists. Pressing outside the card has usually closed it already, so a
    /// card closed moments ago for the same person counts too.
    fn toggled_shut(&mut self, user: Id, _window: &mut Window, cx: &mut Context<Self>) -> bool {
        // The press that closed a card can end after it's gone, if it was a long one.
        const SAME_PRESS: std::time::Duration = std::time::Duration::from_millis(300);
        if let Some((card, _)) = &self.profile {
            let same = card.read(cx).user() == user;
            // Already told to go by an earlier click: this one's a new request.
            if same && !card.read(cx).toggled_off() {
                card.update(cx, |card, cx| card.toggle_off(cx));
                return true;
            }
        }
        self.last_closed.take().is_some_and(|(who, when)| who == user && when.elapsed() < SAME_PRESS)
    }

    fn close_profile(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some((card, _)) = &self.profile {
            // Closed by clicking its name again, that click is over and done with.
            if !card.read(cx).toggled_off() {
                self.last_closed = Some((card.read(cx).user(), std::time::Instant::now()));
            }
        }
        if self.profile.take().is_some() {
            // Escape and clicks moved focus to the card; give it back to the composer.
            self.input.update(cx, |input, cx| input.focus(window, cx));
            cx.notify();
        }
    }

    /// The card's Message button: find or create the DM, then go there.
    fn message(&mut self, user: Id, cx: &mut Context<Self>) {
        let services = self.ctx.services.clone();
        let router = self.ctx.router.clone();
        cx.spawn(async move |this, cx| {
            match services.core.open_dm(user).await {
                Ok(channel) => {
                    let path = nav::channel_path(&channel);
                    let _ = services.store.update(cx, |store, cx| store.add_channel(channel, cx));
                    let _ = this.update(cx, |this, cx| {
                        this.profile = None;
                        cx.notify();
                    });
                    let _ = cx.update(|cx| navigate(&router, path, cx));
                }
                Err(error) => {
                    let _ = cx.update(|cx| {
                        services.toast(Toast::error("Couldn't open the conversation", error.to_string()), cx)
                    });
                }
            }
        })
        .detach();
    }

    /// Keeps the virtual list's item count in step with the cache.
    fn sync_list(&mut self, cx: &mut Context<Self>) {
        let Some(channel) = self.channel else { return };
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();
        let messages: Vec<(Id, u64)> = cache
            .messages(channel)
            .map(|m| m.iter().map(|m| (m.message.id, m.revision)).collect())
            .unwrap_or_default();
        // Row 0 is the top of the channel: "loading older" or "the beginning".
        let top = (TOP_ROW, nav::top_row_state(store.is_loading_older(channel), cache.messages(channel).is_some_and(|m| m.has_older), cache.scrollback_full(channel)));
        let rows: Vec<(Id, u64)> = std::iter::once(top).chain(messages).collect();
        let ids: Vec<Id> = rows.iter().map(|(id, _)| *id).collect();
        if let Some(gone) = nav::trimmed_from_top(&self.rendered, &ids, 1) {
            self.list.splice(1..1 + gone, 0);
            self.rendered.drain(1..1 + gone);
        }
        if let Some((range, count)) = nav::list_splice(&self.rendered, &ids) {
            // Landed below while scrolled up: the jump bar says how many.
            let appended = range.start == self.rendered.len() && !self.rendered.is_empty();
            let following = self.list.is_following_tail();
            if appended && !following {
                self.unseen += count;
            }
            let after = nav::row_after_splice(&range, count, ids.len());
            // An older page arriving under the top row: keep the message that
            // was on screen there, or the view stays at the top and asks for
            // page after page.
            let top = self.list.logical_scroll_top();
            let prepended = range.start == 1 && range.is_empty() && !self.rendered.is_empty();
            self.list.splice(range, count);
            // Only when scrolled there: following the newest, the list is yet to be
            // measured when this comes in, and it'd be pinned to the older messages.
            if prepended && top.item_ix <= 1 && !self.list.is_following_tail() {
                let offset = if top.item_ix == 1 { top.offset_in_item } else { gpui_kit::px(0.) };
                self.list.scroll_to(gpui_kit::ListOffset { item_ix: 1 + count, offset_in_item: offset });
            }
            if let Some(ix) = after {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        // The newest row carries the gap above the composer: when the newest
        // message changes, the row that used to be last loses it.
        if let Some(old_last) = self.rendered.last().filter(|id| ids.last() != Some(*id)) {
            if let Some(ix) = ids.iter().position(|id| id == old_last) {
                self.list.remeasure_items(ix..ix + 1);
            }
        }
        // Edits and unfurls change a row's height; nothing else needs measuring again.
        for ix in nav::edited_rows(&self.revisions, &rows) {
            self.list.remeasure_items(ix..ix + 1);
        }
        self.revisions = rows.into_iter().collect();
        self.rendered = ids;
        self.seek_jump_target(cx);
        self.ask_for_members(channel, cx);
        if self.list.is_following_tail() {
            self.ctx.services.store.update(cx, |store, cx| store.trim_channel(channel, cx));
        }
    }

    /// Fetched messages don't say who has which roles or nickname: asks the
    /// gateway about authors the cache hasn't met, so names get their colours.
    fn ask_for_members(&mut self, channel: Id, cx: &mut Context<Self>) {
        let Some(guild) = self.guild else { return };
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();
        let Some(messages) = cache.messages(channel) else { return };
        let authors = messages.iter().flat_map(|m| {
            let replied = m.message.referenced_message.as_ref().map(|r| r.author.id);
            std::iter::once(m.message.author.id).chain(replied)
        });
        let missing: Vec<Id> = authors.filter(|id| cache.member(guild, *id).is_none()).collect();
        let new: Vec<Id> = missing.into_iter().filter(|id| self.asked_members.insert(*id)).collect();
        if !new.is_empty() {
            store.core().request_members(guild, &new);
        }
    }

    /// Scrolls to the linked message once it's loaded, fetching older pages
    /// (a few at most) until it turns up.
    fn seek_jump_target(&mut self, cx: &mut Context<Self>) {
        // One page at a time, each waiting for the last: quite far back, but not forever.
        const MAX_PAGES: u16 = 400;
        let (Some((target, pages)), Some(channel)) = (self.jump_target, self.channel) else { return };
        if let Some(ix) = self.rendered.iter().position(|r| *r == target) {
            self.jump_target = None;
            self.flash(target, cx);
            // Already fully on screen: the glow says it all, nothing need move
            // (and at the bottom of the chat, nothing could).
            let view = self.list.viewport_bounds();
            if self.row_bounds(ix).is_some_and(|row| row.top() >= view.top() && row.bottom() <= view.bottom()) {
                return;
            }
            // `scroll_to` stops following the tail; merely revealing a row lets
            // the list snap straight back when you were at the bottom.
            self.leave_tail();
            let top = self.list.logical_scroll_top();
            if !self.ctx.services.settings.read(cx).get().smooth_scrolling {
                self.list.scroll_to(gpui_kit::ListOffset { item_ix: ix.saturating_sub(2), offset_in_item: gpui_kit::px(0.) });
                return;
            }
            self.list.scroll_to(top);
            // Far off rows have no position until drawn: land a short way from
            // it now, and the frame after that glides the rest.
            // A row just above has no bounds either, but it's no run-up away.
            let far = if ix < top.item_ix { top.item_ix - ix > JUMP_RUN_UP } else { self.list.bounds_for_item(ix).is_none() };
            if far {
                let near = if ix < top.item_ix { ix + JUMP_RUN_UP } else { ix.saturating_sub(JUMP_RUN_UP) };
                self.list.scroll_to(gpui_kit::ListOffset { item_ix: near.min(self.rendered.len().saturating_sub(1)), offset_in_item: gpui_kit::px(0.) });
            }
            self.glide_to = Some((ix, false));
            cx.notify();
            return;
        }
        let store = self.ctx.services.store.read(cx);
        if store.history(channel) != Some(&History::Loaded) || store.is_loading_older(channel) {
            return;
        }
        let cache = store.cache();
        let older = cache.messages(channel).is_some_and(|m| m.has_older) && !cache.scrollback_full(channel);
        // Older than everything loaded, and still more to fetch.
        let before_loaded = cache.oldest(channel).is_some_and(|oldest| target < oldest);
        if older && before_loaded && pages < MAX_PAGES {
            self.jump_target = Some((target, pages + 1));
            self.ctx.services.store.update(cx, |store, cx| store.load_older(channel, cx));
        } else {
            self.jump_target = None;
            self.ctx.services.toast(Toast::warning("Couldn't find that message", "It may have been deleted, or is too far back."), cx);
        }
    }

    /// Lights `message` up for a moment, as Discord does after a jump.
    fn flash(&mut self, message: Id, cx: &mut Context<Self>) {
        self.jumps += 1;
        let jump = self.jumps;
        self.highlight.set(Some((message, jump)));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor().timer(std::time::Duration::from_millis(2000)).await;
            let _ = this.update(cx, |this, cx| {
                if this.highlight.get() == Some((message, jump)) {
                    this.highlight.set(None);
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// The top row came into view (or within the list's overdraw of it).
    fn reached_top(&mut self, cx: &mut Context<Self>) {
        if let Some(channel) = self.channel {
            self.ctx.services.store.update(cx, |store, cx| store.load_older(channel, cx));
        }
    }

    /// A click on a reply's preview: scroll to the original, fetching it if
    /// need be, and light it up so it's plain where you landed.
    fn jump_to(&mut self, id: Id, cx: &mut Context<Self>) {
        tracing::info!(%id, loaded = self.rendered.contains(&id), "jump: asked");
        self.jump_target = Some((id, 0));
        self.seek_jump_target(cx);
    }

    /// Right click on a person, in Discord's order: what to do with them, what to change
    /// about them, what a moderator may do, then their ID. Only what we may do is listed.
    fn user_menu(&mut self, user: Id, name: String, position: Point<Pixels>, cx: &mut Context<Self>) {
        use crate::context_menu::{self as menu, Entry};
        use crate::panels::{self, What};
        let page = cx.entity().downgrade();
        let store = self.ctx.services.store.clone();
        let guild = self.guild;
        let (is_me, friend, friend_nick, mods, owned_group, handle, bot, server_nick) = {
            let cache = store.read(cx).cache();
            let me = cache.me().map(|u| u.id);
            let owned = self.channel.filter(|channel| {
                cache.channel(*channel).is_some_and(|c| c.kind == ChannelKind::GroupDm && c.owner_id.is_some() && c.owner_id == me && c.recipient_ids.contains(&user))
            });
            let known = cache.user(user);
            (
                me == Some(user),
                cache.is_friend(user),
                cache.friend_nickname(user).map(str::to_string),
                guild.map(|g| cache.moderation(g, user)).unwrap_or_default(),
                owned,
                known.map_or_else(|| name.clone(), |u| u.username.clone()),
                known.is_some_and(|u| u.bot),
                guild.and_then(|g| cache.member(g, user)).and_then(|m| m.nick.clone()).unwrap_or_default(),
            )
        };
        let with = |f: fn(&mut ChannelPage, Id, String, Point<Pixels>, &mut Window, &mut Context<ChannelPage>)| {
            let (page, name) = (page.clone(), name.clone());
            move |window: &mut Window, cx: &mut App| {
                let _ = page.update(cx, |page, cx| f(page, user, name.clone(), position, window, cx));
            }
        };
        let panel = |what: What| {
            let store = store.clone();
            move |window: &mut Window, cx: &mut App| panels::open(store.clone(), what.clone(), window, cx)
        };
        let mut entries = vec![
            Entry::item("Profile", with(|p, u, _, at, w, cx| {
                p.last_closed = None;
                p.show_profile(u, at, false, w, cx);
            })),
            Entry::item("Mention", with(|p, u, n, _, w, cx| p.mention(u, n, w, cx))),
        ];
        if !is_me {
            entries.push(Entry::item("Message", with(|p, u, _, _, _, cx| p.message(u, cx))));
            if !bot {
                entries.push(Entry::later("Start a Call"));
            }
            entries.push(Entry::item("Add Note", panel(What::Note { user, name: name.clone() })).hint("Only visible to you"));
            if friend {
                let label = if friend_nick.is_some() { "Change Friend Nickname" } else { "Add Friend Nickname" };
                entries.push(Entry::item(label, panel(What::FriendNickname { user, name: name.clone(), current: friend_nick.unwrap_or_default() })));
            }
        }
        entries.push(Entry::Separator);
        // A group's owner may remove people or hand it over. Both ask first.
        if let Some(channel) = owned_group {
            let (remove, give) = (store.clone(), store.clone());
            let (who, whom) = (name.clone(), name.clone());
            entries.push(Entry::item("Remove From Group", move |_, cx| {
                let store = remove.clone();
                crate::confirm::ask(format!("Remove {who}"), format!("Remove {who} from this group?"), "Remove", move |cx| {
                    store.update(cx, |store, cx| store.remove_from_group(channel, user, cx));
                }, cx);
            }).danger());
            entries.push(Entry::item("Make Group Owner", move |_, cx| {
                let store = give.clone();
                crate::confirm::ask(format!("Make {whom} the owner"), format!("Hand this group over to {whom}? You won't own it anymore."), "Make Owner", move |cx| {
                    store.update(cx, |store, cx| store.make_group_owner(channel, user, cx));
                }, cx);
            }).danger());
            entries.push(Entry::Separator);
        }
        if let Some(guild) = guild.filter(|_| mods.nickname) {
            entries.push(Entry::item("Change Nickname", panel(What::Nickname { guild, user, me: is_me, name: name.clone(), current: server_nick })));
        }
        if !is_me {
            entries.push(Entry::later("Apps"));
            if friend {
                entries.push(Entry::later("Invite to Server"));
                let (store, who) = (store.clone(), name.clone());
                entries.push(Entry::item("Remove Friend", move |_, cx| {
                    let store = store.clone();
                    crate::confirm::ask(format!("Remove {who}"), format!("Remove {who} from your friends?"), "Remove Friend", move |cx| {
                        store.update(cx, |store, cx| store.remove_friend(user, cx));
                    }, cx);
                }));
            } else if !bot {
                let store = store.clone();
                entries.push(Entry::item("Add Friend", move |_, cx| store.update(cx, |store, cx| store.add_friend(user, cx))));
            }
            entries.push(Entry::later("Ignore"));
            let (store, who) = (store.clone(), name.clone());
            entries.push(Entry::item("Block", move |_, cx| {
                let store = store.clone();
                crate::confirm::ask(format!("Block {who}"), format!("Block {who}? They won't be able to message you, and you won't see them in your friends."), "Block", move |cx| {
                    store.update(cx, |store, cx| store.block_user(user, cx));
                }, cx);
            }).danger());
        }
        // Server moderation: each opens its own panel, which is the confirmation.
        if let Some(guild) = guild {
            if mods.roles {
                entries.push(Entry::Separator);
                entries.push(Entry::later("Roles"));
            }
            if mods.mod_view || mods.timeout || mods.kick || mods.ban {
                entries.push(Entry::Separator);
                if mods.mod_view {
                    entries.push(Entry::later("Open in Mod View"));
                }
                if mods.timeout {
                    entries.push(Entry::item(format!("Timeout {name}"), panel(What::Timeout { guild, user, name: name.clone() })).danger());
                }
                if mods.kick {
                    entries.push(Entry::item(format!("Kick {name}"), panel(What::Kick { guild, user, name: name.clone(), handle: handle.clone() })).danger());
                }
                if mods.ban {
                    entries.push(Entry::item(format!("Ban {name}"), panel(What::Ban { guild, user, handle: name.clone() })).danger());
                }
            }
        }
        entries.push(Entry::Separator);
        entries.push(Entry::copy("Copy User ID", user.to_string()).icon(IconName::Copy));
        menu::show(position, entries, cx);
    }

    /// Right click on a message.
    fn message_menu(&mut self, id: Id, position: Point<Pixels>, cx: &mut Context<Self>) {
        use crate::context_menu::{self as menu, Action, Entry};
        let Some(channel) = self.channel else { return };
        let cache = self.ctx.services.store.read(cx).cache();
        let Some(message) = cache.messages(channel).and_then(|m| m.iter().find(|m| m.message.id == id)).map(|m| m.message.clone()) else { return };
        let own = cache.me().is_some_and(|me| me.id == message.author.id);
        let author = cache.display_name(self.guild, &message.author).to_string();
        let place = self.guild.map_or("@me".to_string(), |g| g.to_string());
        let (page, store) = (cx.entity().downgrade(), self.ctx.services.store.clone());
        let reactions = QUICK_REACTIONS
            .iter()
            .map(|emoji| {
                let (store, name) = (store.clone(), emoji.to_string());
                let action: Action = Rc::new(move |_, cx| {
                    let emoji = ReactionEmoji { name: Some(name.clone()), ..Default::default() };
                    store.update(cx, |store, cx| store.set_reaction(channel, id, emoji, true, cx));
                });
                (emoji.to_string(), action)
            })
            .collect();
        use cord_core::permissions as perms;
        let can_react = cache.has_permission(channel, perms::ADD_REACTIONS);
        // Nobody moderates a DM or a group, whatever permissions it nominally gives.
        let in_server = cache.channel(channel).is_some_and(|c| c.guild_id.is_some());
        let can_manage = in_server && cache.has_permission(channel, perms::MANAGE_MESSAGES);
        let can_pin = !in_server || can_manage || cache.has_permission(channel, perms::PIN_MESSAGES);
        let can_send = cache.can_send(channel);
        let mut entries = Vec::new();
        if can_react {
            entries.push(Entry::Reactions(reactions));
            entries.push(Entry::later("Add Reaction").icon(IconName::FaceSlightlySmilingPlus));
        }
        if !message.reactions.is_empty() {
            entries.push(Entry::later("View Reactions"));
        }
        entries.push(Entry::Separator);
        if own {
            let (page, content) = (page.clone(), message.content.clone());
            entries.push(
                Entry::item("Edit Message", move |window, cx| {
                    let _ = page.update(cx, |page, cx| page.start_edit(id, content.clone(), window, cx));
                })
                .icon(IconName::Pencil),
            );
        }
        let reply_page = page.clone();
        if can_send {
            entries.push(
                Entry::item("Reply", move |window, cx| {
                    let _ = reply_page.update(cx, |page, cx| page.reply_to(id, author.clone(), window, cx));
                })
                .icon(IconName::Reply),
            );
        }
        entries.push(Entry::later("Forward"));
        entries.push(Entry::later("Create Thread"));
        entries.push(Entry::Separator);
        if !message.content.is_empty() {
            entries.push(Entry::copy("Copy Text", message.content.clone()).icon(IconName::Copy));
        }
        let pin_store = store.clone();
        let pinned = message.pinned;
        if can_pin {
            entries.push(
                Entry::item(if pinned { "Unpin Message" } else { "Pin Message" }, move |_, cx| {
                    pin_store.update(cx, |store, cx| {
                        if pinned {
                            store.unpin_message(channel, id, cx)
                        } else {
                            store.pin_message(channel, id, cx)
                        }
                    });
                })
                .picture(if pinned { "icons/pin-off.svg" } else { "icons/toolbar-pin.svg" }),
            );
        }
        entries.push(Entry::later("Mark Unread"));
        entries.push(Entry::copy("Copy Message Link", format!("https://discord.com/channels/{place}/{channel}/{id}")).icon(IconName::Link));
        // Only for who may; each still needs a confirm step when it's built.
        if !message.reactions.is_empty() && can_manage {
            entries.push(Entry::Separator);
            entries.push(Entry::later("Remove Reactions").danger());
            entries.push(Entry::later("Remove All Reactions").danger());
        }
        if let Some((user, _)) = message.command_use() {
            let (who, name) = (user.id, cache.display_name(self.guild, user).to_string());
            entries.push(Entry::item(format!("Triggered by {name}"), move |_, cx| crate::links::open_profile(who, position, cx)));
        }
        if let Some(a) = message.attachments.first() {
            entries.push(Entry::Separator);
            entries.push(Entry::copy("Copy Media Link", a.url.clone()).icon(IconName::Link));
            let url = a.url.clone();
            entries.push(Entry::item("Open Media in Browser", move |_, cx| cx.open_url(&url)));
        }
        entries.push(Entry::Separator);
        if own || can_manage {
            let page = page.clone();
            entries.push(
                Entry::item("Delete Message", move |_, cx| {
                    let _ = page.update(cx, |page, cx| {
                        page.confirm_delete = Some(id);
                        cx.notify();
                    });
                })
                .icon(IconName::Trash)
                .danger(),
            );
        }
        entries.push(Entry::later("Report Message").danger());
        entries.push(Entry::Separator);
        entries.push(Entry::copy("Copy Message ID", id.to_string()).icon(IconName::Copy));
        menu::show(position, entries, cx);
    }

    /// Puts a message of ours in the box to edit; Enter saves, Escape drops it.
    fn start_edit(&mut self, message: Id, content: String, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some(message);
        self.replying = None;
        self.fill_input(content, window, cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
        self.keep_draft(cx);
        cx.notify();
    }

    /// Puts `text` in the message box, its pings showing as they do when typed.
    fn fill_input(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let cache = self.ctx.services.store.read(cx).cache();
        let mut pieces: Vec<(String, Option<InlineToken>)> = Vec::new();
        let mut at = 0;
        for (range, ping) in composer_pings(&text) {
            pieces.push((text[at..range.start].to_string(), Some(ping_token(cache, self.guild, ping))));
            at = range.end;
        }
        pieces.push((text[at..].to_string(), None));
        // Written out in order, which leaves the cursor at the end, ready to carry on.
        self.input.update(cx, |input, cx| {
            input.set_value("", window, cx);
            for (plain, token) in pieces {
                if !plain.is_empty() {
                    input.insert(plain, window, cx);
                }
                if let Some(token) = token {
                    let here = input.cursor();
                    let _ = input.replace_range_with_token(here..here, token, window, cx);
                }
            }
        });
    }

    /// Done editing: back to whatever message was being written before.
    fn leave_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        let Some(channel) = self.channel else { return };
        crate::drafts::set_edit(channel, None, cx);
        self.fill_input(crate::drafts::get(channel).text, window, cx);
    }

    /// What's in the message box goes in this channel's draft.
    fn keep_draft(&self, cx: &App) {
        let Some(channel) = self.channel else { return };
        let text = self.input.read(cx).value().to_string();
        match self.editing {
            Some(message) => crate::drafts::set_edit(channel, Some((message, text)), cx),
            None => crate::drafts::set_text(channel, text, cx),
        }
    }

    /// Back on this channel: whatever was left unsent, as it was.
    fn restore_draft(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel else { return };
        let draft = crate::drafts::get(channel);
        let (editing, text) = match draft.edit {
            Some((message, text)) => (Some(message), text),
            None => (None, draft.text),
        };
        self.editing = editing;
        if !text.is_empty() {
            self.fill_input(text, window, cx);
        }
        // Those still there: a file may have been moved or deleted since.
        self.pending = draft.files.into_iter().filter(|file| file.path.exists()).collect();
    }

    /// Remembers the files waiting in the box along with the text.
    fn keep_files(&self, cx: &App) {
        if let Some(channel) = self.channel {
            crate::drafts::set_files(channel, self.pending.clone(), cx);
        }
    }

    /// A group's people, in place of a server's member list.
    fn group_members(&self, channel: &cord_core::model::Channel, width: f32, cx: &Context<Self>) -> AnyElement {
        use crate::members::avatar_with_status;
        use cord_core::member_list::Status;
        let theme = cx.theme();
        let owner = channel.owner_id;
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();
        let mut people: Vec<cord_core::model::User> = channel
            .recipient_ids
            .iter()
            .filter_map(|id| cache.user(*id).or_else(|| channel.recipients.iter().find(|u| u.id == *id)).cloned())
            .chain(cache.me().cloned())
            .collect();
        // A channel update can list us among its recipients as well.
        let mut seen = std::collections::HashSet::new();
        people.retain(|u| seen.insert(u.id));
        people.sort_by_cached_key(|u| {
            let offline = cache.presence(u.id).is_none_or(|p| p.status == Status::Offline);
            (offline, cache.display_name(None, u).to_lowercase())
        });
        let count = people.len();
        let rows = people.into_iter().map(|user| {
            let presence = cache.presence(user.id).cloned();
            let status = presence.as_ref().map_or(Status::Offline, |p| p.status);
            let name = cache.display_name(None, &user).to_string();
            let look = crate::badges::name_look(&user, &[]);
            let id = user.id;
            let menu_name = name.clone();
            let line = presence.as_ref().and_then(|p| members::status_element(&p.line, cache.in_voice(id), cx));
            h_flex()
                .id(SharedString::from(format!("group-member-{id}")))
                .relative()
                .flex_shrink_0()
                .w_full()
                .h(u(42.))
                .mb(u(2.))
                .px_2()
                .gap_3()
                .items_center()
                .rounded(u(8.))
                .cursor_pointer()
                .hover(|d| d.bg(theme.secondary_hover))
                .when_some(user.nameplate_url(), |d, url| {
                    d.child(
                        div()
                            .absolute()
                            .top_0()
                            .left_0()
                            .size_full()
                            .opacity(0.6)
                            .child(gpui_kit::img(url).absolute().top_0().left_0().size_full().rounded(u(8.)).object_fit(gpui_kit::ObjectFit::Cover)),
                    )
                    .children(crate::nameplates::backdrop(user.nameplate_palette(), theme.secondary, theme.mode.is_dark(), 8.))
                })
                .when(status == Status::Offline, |d| d.opacity(0.45))
                .on_mouse_down(gpui_kit::MouseButton::Right, cx.listener(move |this, event: &gpui_kit::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.user_menu(id, menu_name.clone(), event.position, cx);
                }))
                .on_click(cx.listener(move |this, event: &ClickEvent, window, cx| {
                    let edge = window.viewport_size().width - window.rem_size() * (width / 16.) - gpui_kit::px(8.);
                    if !this.toggled_shut(id, window, cx) {
                        this.show_profile(id, gpui_kit::point(edge, event.position().y), true, window, cx);
                    }
                }))
                .child(avatar_with_status(
                    Avatar::new().border_0().src(cache.avatar_url(None, &user, 64)).name(name.clone()),
                    32.,
                    Some(status),
                    presence.as_ref().is_some_and(|p| p.mobile),
                    user.decoration_url(),
                    theme.secondary,
                ))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(
                            h_flex()
                                .gap(u(5.))
                                .items_center()
                                .child(
                                    crate::badges::name_clip(div().min_w_0(), &look.stops)
                                        .text_size(u(16.))
                                        .line_height(u(20.))
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(look.color.unwrap_or(theme.foreground))
                                        .when_some(look.font, |d, font| d.font_family(font))
                                        .child(crate::badges::gradient_text(&name, &look.stops, 0.)),
                                )
                                .when(owner == Some(id), |d| d.child(crate::badges::owner_crown())),
                        )
                        .children(line),
                )
        });
        v_flex()
            .id("group-members")
            .h_full()
            .w(u(width))
            .flex_shrink_0()
            .p(u(8.))
            .overflow_y_scroll()
            .bg(theme.secondary)
            .border_l_1()
            .border_color(theme.border)
            .child(div().flex_shrink_0().h(u(32.)).px_2().flex().items_center().text_size(u(14.)).font_weight(FontWeight::MEDIUM).text_color(theme.muted_foreground).child(format!("Members \u{2014} {count}")))
            .children(rows)
            .child(self.group_invite_button(channel, count, cx))
            .into_any_element()
    }

    /// Adds a friend to the group. A group holds ten at most.
    fn group_invite_button(&self, channel: &cord_core::model::Channel, count: usize, cx: &Context<Self>) -> AnyElement {
        const MOST: usize = 10;
        let theme = cx.theme();
        let room = count < MOST;
        let (id, inside) = (channel.id, channel.recipient_ids.clone());
        let store = self.ctx.services.store.clone();
        div()
            .id("group-invite")
            .flex_shrink_0()
            .mx(u(8.))
            .my(u(8.))
            .h(u(38.))
            .flex()
            .items_center()
            .justify_center()
            .gap(u(8.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .text_size(u(14.))
            .font_weight(FontWeight::MEDIUM)
            .when(!room, |d| d.opacity(0.5))
            .when(room, |d| {
                d.cursor_pointer().hover(|d| d.bg(theme.foreground.opacity(0.06))).on_click(move |_, window, cx| {
                    crate::add_people::open(store.clone(), id, inside.clone(), MOST.saturating_sub(count), window, cx);
                })
            })
            .child(Icon::new(IconName::UserPlus).size(u(18.)))
            .child(if room { "Invite to Group DM" } else { "This group is full" })
            .into_any_element()
    }

    /// "Delete this message?", over the page.
    /// Fades the question out, then it's gone.
    fn close_delete(&mut self, cx: &mut Context<Self>) {
        if self.delete_closing {
            return;
        }
        self.delete_closing = true;
        cx.notify();
        cx.spawn(async move |page, cx| {
            cx.background_executor().timer(DELETE_FADE).await;
            let _ = page.update(cx, |page, cx| {
                page.delete_closing = false;
                page.confirm_delete = None;
                cx.notify();
            });
        })
        .detach();
    }

    fn delete_dialog(&self, message: Id, window: &Window, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let Some(channel) = self.channel else { return div().into_any_element() };
        let button = |id: &'static str, label: &'static str, bg: Hsla, fg: Hsla| {
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
        let viewport = window.viewport_size();
        let closing = self.delete_closing;
        let backdrop = div()
            .id("delete-backdrop")
            .occlude()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui_kit::black().opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| this.close_delete(cx)))
            .child(
                v_flex()
                    .id("delete-dialog")
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
                    .child(div().text_size(u(20.)).font_weight(FontWeight::SEMIBOLD).child("Delete Message"))
                    .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child("Are you sure you want to delete this message? This can't be undone."))
                    .child(
                        h_flex()
                            .justify_end()
                            .gap(u(8.))
                            .child(button("delete-cancel", "Cancel", theme.foreground.opacity(0.08), theme.foreground).on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_delete(cx);
                            })))
                            .child(button("delete-confirm", "Delete", theme.danger, gpui_kit::white()).on_click(cx.listener(move |this, _, _, cx| {
                                cx.stop_propagation();
                                this.close_delete(cx);
                                this.ctx.services.store.update(cx, |store, cx| store.delete_message(channel, message, cx));
                                cx.notify();
                            }))),
                    )
                .with_animation(
                    ("delete-card", closing as usize),
                    gpui_kit::Animation::new(DELETE_FADE).with_easing(gpui_kit::ease_out_quint()),
                    move |d, t| d.top(u(if closing { t * 8. } else { (1. - t) * 12. })),
                )
            );
        // Over the whole window, not just the chat: it's about everything.
        gpui_kit::deferred(
            gpui_kit::anchored().position(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.))).child(backdrop.with_animation(
                ("delete-fade", closing as usize),
                gpui_kit::Animation::new(DELETE_FADE).with_easing(gpui_kit::ease_out_quint()),
                move |d, t| d.opacity(if closing { 1. - t } else { t }),
            )),
        )
        .with_priority(5)
        .into_any_element()
    }

    /// Starts a reply: the next message sent answers `message`.
    fn reply_to(&mut self, message: Id, author: String, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();
        let me = cache.me().map(|u| u.id);
        let author_id = self.channel.and_then(|c| cache.messages(c)).and_then(|m| m.iter().find(|m| m.message.id == message)).map(|m| m.message.author.id);
        self.replying_to_self = author_id.is_some() && author_id == me;
        self.replying = Some((message, author, author_id));
        self.editing = None;
        // Nobody needs pinging about their own message.
        self.reply_ping = !self.replying_to_self;
        self.input.update(cx, |input, cx| input.focus(window, cx));
        cx.notify();
    }

    /// Discord's blurple bar over the top of the chat: how many messages came
    /// in unread and since when; clicking jumps there, Mark As Read hides it.
    fn new_messages_bar(&self, channel: Id, cx: &Context<Self>) -> Option<AnyElement> {
        if self.banner_closed {
            return None;
        }
        let store = self.ctx.services.store.read(cx);
        let read = store.new_after()?;
        let cache = store.cache();
        let me = cache.me().map(|u| u.id);
        let messages = cache.messages(channel)?;
        let unread: Vec<&cord_core::model::Message> =
            messages.iter().map(|m| &m.message).filter(|m| m.id > read && Some(m.author.id) != me).collect();
        let first = unread.first()?;
        // Older unread messages than the loaded page may exist.
        let more = if messages.has_older && messages.iter().next().is_some_and(|m| m.message.id > read) { "+" } else { "" };
        let label = format!(
            "{}{more} new message{} since {}",
            unread.len(),
            if unread.len() == 1 && more.is_empty() { "" } else { "s" },
            nav::short_time(first.id.timestamp_ms(), &chrono::Local)
        );
        let first_id = first.id;
        let blurple = Hsla::from(gpui_kit::rgb(0x5865f2));
        Some(
            h_flex()
                .id("new-messages-bar")
                .absolute()
                .top_0()
                .left(u(16.))
                .right(u(16.))
                .h(u(32.))
                .px(u(12.))
                .items_center()
                .rounded_b(u(8.))
                .bg(blurple)
                .text_color(gpui_kit::white())
                .text_size(u(14.))
                .font_weight(FontWeight::SEMIBOLD)
                .cursor_pointer()
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.jump_to(first_id, cx);
                    cx.notify();
                }))
                .child(div().flex_1().min_w_0().truncate().child(label))
                .child(
                    div()
                        .id("mark-as-read")
                        .flex_shrink_0()
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .on_click(cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.banner_closed = true;
                            cx.notify();
                        }))
                        .child("Mark As Read"),
                )
                .into_any_element(),
        )
    }

    /// How many unread messages there are, past 10 and while scrolled up from
    /// the newest: worth a "Jump to New Messages".
    fn unread_below(&self, channel: Id, cx: &App) -> Option<usize> {
        const JUMP_AFTER: usize = 10;
        if self.list.is_following_tail() || self.banner_closed {
            return None;
        }
        let store = self.ctx.services.store.read(cx);
        let read = store.new_after()?;
        let cache = store.cache();
        let me = cache.me().map(|u| u.id);
        let count = cache.messages(channel)?.iter().map(|m| &m.message).filter(|m| m.id > read && Some(m.author.id) != me).count();
        (count > JUMP_AFTER).then_some(count)
    }

    /// Logs the view moving with nobody scrolling: which row it's anchored to,
    /// and by how much, to catch what nudges it while reading further up.
    fn note_unasked_move(&mut self) {
        let top = self.list.logical_scroll_top();
        let now = self.rendered.get(top.item_ix).map(|id| (*id, f32::from(top.offset_in_item)));
        let was = self.anchor.replace(now);
        // A glide's last step lands the frame after it ends, so that frame doesn't count either.
        let gliding = self.scroller.is_gliding() || self.glide_to.is_some() || self.jump_settle.is_some();
        let still = gliding || self.was_gliding.replace(gliding) || self.list.is_following_tail();
        let was = was.flatten();
        if !still && was != now && let (Some(was), Some(now)) = (was, now) {
            tracing::info!(row_was = %was.0, row_now = %now.0, offset_was = was.1, offset_now = now.1, rows = self.rendered.len(), "chat view moved by itself");
        }
    }

    /// How far down row `ix`'s top is from the top of the view; negative above it.
    /// The list gives no bounds for rows above its top row, so those are found
    /// by putting the list on the row, reading where that is, and putting it back.
    fn row_distance(&self, ix: usize) -> Option<f32> {
        self.at_real_top(|page| page.row_distance_from_top(ix))
    }

    /// Following the newest, the list has no top row of its own: it reports one
    /// past the last, and no bounds for anything on screen. A little up from
    /// there, it does; within a pixel of the end the list just carries on following.
    fn leave_tail(&self) {
        if self.list.is_following_tail() {
            let max = self.list.max_offset_for_scrollbar().y;
            self.list.set_offset_from_scrollbar(gpui_kit::point(gpui_kit::px(0.), -(max - gpui_kit::px(2.)).max(gpui_kit::px(0.))));
        }
    }

    /// Runs `f` with the list's top row real, even while following the newest.
    fn at_real_top<R>(&self, f: impl FnOnce(&Self) -> R) -> R {
        let following = self.list.is_following_tail();
        self.leave_tail();
        let result = f(self);
        if following {
            self.list.set_follow_mode(FollowMode::Tail);
        }
        result
    }

    /// Where row `ix` is now, following the newest or not.
    fn row_bounds(&self, ix: usize) -> Option<gpui_kit::Bounds<gpui_kit::Pixels>> {
        self.at_real_top(|page| page.list.bounds_for_item(ix))
    }

    fn row_distance_from_top(&self, ix: usize) -> Option<f32> {
        let top = self.list.logical_scroll_top();
        if ix >= top.item_ix {
            let view = self.list.viewport_bounds();
            return self.list.bounds_for_item(ix).map(|row| f32::from(row.top() - view.top()));
        }
        let now = self.list.scroll_px_offset_for_scrollbar().y;
        self.list.scroll_to(gpui_kit::ListOffset { item_ix: ix, offset_in_item: gpui_kit::px(0.) });
        let there = self.list.scroll_px_offset_for_scrollbar().y;
        self.list.scroll_to(top);
        Some(f32::from(now - there))
    }

    /// At the newest message, the unread ones have been seen: either the first
    /// of them is on screen, or we scrolled up and came back down past them.
    fn close_banner_once_read(&mut self, cx: &App) {
        if !self.near_newest() {
            // Only once there's a layout to go by, not while the list is still filling.
            if self.list.bounds_for_item(self.list.logical_scroll_top().item_ix).is_some() {
                self.left_tail = true;
            }
            return;
        }
        if self.banner_closed {
            return;
        }
        let first_unread = self.channel.and_then(|channel| {
            let store = self.ctx.services.store.read(cx);
            let read = store.new_after()?;
            // List rows start with the one at the top of the history.
            store.cache().messages(channel)?.iter().position(|m| m.message.id > read).map(|i| i + 1)
        });
        let Some(row) = first_unread else { return };
        let view = self.list.viewport_bounds();
        // Any of it showing counts: the row starts above its message, with the NEW line.
        if self.left_tail || self.row_bounds(row).is_some_and(|r| r.bottom() > view.top() + gpui_kit::px(24.)) {
            self.banner_closed = true;
        }
    }

    /// The newest message is on screen or nearly: nothing worth a jump pill.
    fn near_newest(&self) -> bool {
        const NEARLY: f32 = 200.;
        let view = self.list.viewport_bounds();
        self.list.is_following_tail()
            || self.list.bounds_for_item(self.rendered.len().saturating_sub(1)).is_some_and(|last| last.bottom() <= view.bottom() + gpui_kit::px(NEARLY))
    }

    /// Glides down to the newest message, where the list follows the tail again.
    fn jump_to_present(&mut self, cx: &mut Context<Self>) {
        self.unseen = 0;
        self.banner_closed = true;
        let last = self.rendered.len().saturating_sub(1);
        if !self.ctx.services.settings.read(cx).get().smooth_scrolling {
            self.list.set_follow_mode(FollowMode::Tail);
        } else {
            if self.list.bounds_for_item(last).is_none() {
                self.list.scroll_to(gpui_kit::ListOffset { item_ix: last.saturating_sub(JUMP_RUN_UP), offset_in_item: gpui_kit::px(0.) });
            }
            self.glide_to = Some((last, true));
        }
        cx.notify();
    }

    /// The pill over the message box, while there's somewhere to jump to: new
    /// messages off screen, or simply being a good way up.
    fn jump_bar(&self, unread: Option<usize>, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let blurple = Hsla::from(gpui_kit::rgb(0x5865f2));
        h_flex()
            .absolute()
            .bottom(u(28.))
            .left_0()
            .right_0()
            .justify_center()
            .child(
                h_flex()
                    .max_w_full()
                    .mx(u(8.))
                    .p(u(6.))
                    .pl(u(16.))
                    .gap(u(12.))
                    .items_center()
                    .rounded(u(8.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_lg()
                    .text_size(u(14.))
                    .font_weight(FontWeight::MEDIUM)
                    // Narrow: the words give way, the button stays.
                    .child(div().flex_shrink(1.).min_w_0().truncate().child(match (unread, self.unseen) {
                        (Some(n), _) => format!("{n} New Messages"),
                        (None, 0) => "You're Viewing Older Messages".to_string(),
                        (None, 1) => "1 New Message".to_string(),
                        (None, n) => format!("{n} New Messages"),
                    }))
                    .child(
                        div()
                            .id("jump-to-present")
                            .flex_shrink_0()
                            .h(u(32.))
                            .px(u(16.))
                            .flex()
                            .items_center()
                            .rounded(u(6.))
                            .bg(blurple)
                            .text_color(gpui_kit::white())
                            .font_weight(FontWeight::SEMIBOLD)
                            .cursor_pointer()
                            .hover(|d| d.opacity(0.9))
                            .on_click(cx.listener(|this, _, _, cx| this.jump_to_present(cx)))
                            .child(if unread.is_some() { "Jump to New Messages" } else { "Jump To Present" }),
                    ),
            )
            .into_any_element()
    }

    /// The emoji / mention popup, styled like a macOS menu: a translucent
    /// rounded panel whose highlighted row is system blue.
    fn suggestion_popup(&self, cx: &Context<Self>) -> Option<AnyElement> {
        let s = self.suggest.as_ref()?;
        let theme = cx.theme();
        let blue = Hsla::from(gpui_kit::rgb(0x0a84ff));
        let (fg, muted) = (theme.foreground, theme.muted_foreground);
        let rows = s.items.iter().enumerate().map(|(i, item)| {
            let selected = i == s.selected;
            let icon = match (&item.emoji, &item.avatar) {
                _ if item.custom.is_some() => {
                    let (_, id, animated) = item.custom.as_ref().expect("just checked");
                    img(crate::message_text::custom_emoji_url(*id, *animated)).size(u(22.)).flex_shrink_0().into_any_element()
                }
                (Some(emoji), _) => crate::badges::emoji_image(emoji, 22.).into_any_element(),
                (None, Some(url)) => div().size(u(22.)).flex_shrink_0().child(Avatar::new().border_0().src(url.clone()).name(item.label.clone()).size(u(22.))).into_any_element(),
                // Roles and @everyone: an @ where a face would be.
                (None, None) => div()
                    .size(u(22.))
                    .flex_shrink_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(if selected { gpui_kit::white() } else { muted })
                    .child(Icon::new(IconName::AtSign).size(u(16.)))
                    .into_any_element(),
            };
            h_flex()
                .id(("emoji-suggest", i))
                .h(u(28.))
                .px(u(8.))
                .gap(u(8.))
                .items_center()
                .rounded(u(5.))
                .cursor_pointer()
                .text_color(if selected { gpui_kit::white() } else { fg })
                .when(selected, |d| d.bg(blue))
                .when(!selected, |d| d.hover(|d| d.bg(fg.opacity(0.08))))
                .on_click(cx.listener(move |this, _, window, cx| this.pick_suggestion(i, window, cx)))
                .child(icon)
                .child(div().min_w_0().truncate().text_size(u(14.)).child(item.label.clone()))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_size(u(13.))
                        .text_color(if selected { gpui_kit::white().opacity(0.8) } else { muted })
                        .child(item.detail.clone()),
                )
        });
        Some(
            v_flex()
                .absolute()
                .left(u(8.))
                // Above the box, however many lines it has grown to.
                .bottom(u(58. + 8. + 6. + 22. * self.input.read(cx).value().matches('\n').count().min(7) as f32))
                .w(u(300.))
                .p(u(5.))
                .gap(u(1.))
                .rounded(u(10.))
                .bg(Hsla::from(gpui_kit::rgb(0x262628)).opacity(0.94))
                .border_1()
                .border_color(gpui_kit::white().opacity(0.12))
                .shadow_lg()
                .child(
                    div()
                        .px(u(8.))
                        .pt(u(2.))
                        .pb(u(4.))
                        .text_size(u(11.))
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(muted)
                        .child(match (s.mention, s.query.is_empty()) {
                            (true, true) => "Members".to_string(),
                            (true, false) => format!("Members matching @{}", s.query),
                            _ => format!("Emoji matching :{}", s.query),
                        }),
                )
                .children(rows)
                .into_any_element(),
        )
    }

    /// Re-reads the text before the cursor for a `:shortcode` or `@name`
    /// and fills the popup. `search` also asks the server for matching
    /// members (only on typing, not when their answers come back).
    fn refresh_suggestion(&mut self, search: bool, cx: &mut Context<Self>) {
        let input = self.input.read(cx);
        let (text, cursor) = (input.value().to_string(), input.cursor());
        let emoji = crate::emoji_suggest::query(&text, cursor).map(|(start, q)| (start, q.to_string(), false));
        let mention = || crate::emoji_suggest::mention_query(&text, cursor).map(|(start, q)| (start, q.to_string(), true));
        let found = emoji.or_else(mention);
        if let (Some((_, query, true)), Some(guild), true) = (&found, self.guild, search) {
            if !query.is_empty() {
                self.ctx.services.core.search_members(guild, query);
            }
        }
        self.suggest = found.and_then(|(start, query, mention)| {
            let items = if mention { self.mention_items(&query, cx) } else { self.emoji_items(&query, cx) };
            if items.is_empty() {
                return None;
            }
            // Typing on keeps the highlighted row when it still matches.
            let keep = self.suggest.as_ref().filter(|s| s.start == start).and_then(|s| s.items.get(s.selected).cloned());
            let selected = keep.and_then(|old| items.iter().position(|i| *i == old)).unwrap_or(0);
            Some(Suggest { start, cursor, query, mention, items, selected })
        });
        cx.notify();
    }

    /// `:query` matches: the server emoji we may use first, then the standard ones.
    fn emoji_items(&self, query: &str, cx: &Context<Self>) -> Vec<SuggestItem> {
        const LIMIT: usize = 8;
        let wanted = query.to_lowercase();
        let store = self.ctx.services.store.read(cx);
        // The standard ones always get a share: a lot of matching server emoji mustn't
        // push `:sob:` off the list. Names starting with what's typed come first.
        let standard = crate::emoji_suggest::suggestions(query, LIMIT);
        let custom_room = LIMIT - standard.len().min(LIMIT / 2);
        let mut custom: Vec<_> = self
            .channel
            .map(|channel| store.cache().usable_emojis(channel))
            .unwrap_or_default()
            .into_iter()
            .filter(|e| !wanted.is_empty() && e.name.to_lowercase().contains(&wanted))
            .collect();
        custom.sort_by_key(|e| !e.name.to_lowercase().starts_with(&wanted));
        let mut items: Vec<SuggestItem> = custom
            .into_iter()
            .take(custom_room)
            .map(|e| SuggestItem {
                emoji: None,
                avatar: None,
                label: format!(":{}:", e.name),
                detail: String::new(),
                mention: None,
                custom: Some((e.name.clone(), e.id, e.animated)),
            })
            .collect();
        let room = LIMIT - items.len();
        items.extend(crate::emoji_suggest::suggestions(query, room).into_iter().map(|(emoji, code)| SuggestItem {
            emoji: Some(emoji),
            avatar: None,
            label: format!(":{code}:"),
            detail: String::new(),
            mention: None,
            custom: None,
        }));
        items
    }

    fn update_suggestion(&mut self, cx: &mut Context<Self>) {
        self.refresh_suggestion(true, cx);
    }

    fn mention_items(&self, query: &str, cx: &App) -> Vec<SuggestItem> {
        let Some(channel) = self.channel else { return Vec::new() };
        let cache = self.ctx.services.store.read(cx).cache();
        let me = cache.me().map(|u| u.id);
        let lower = query.to_lowercase();
        let may_ping_all = cache.can_mention_everyone(channel);
        let mut others: Vec<SuggestItem> = Vec::new();
        // Groups have @everyone and @here too, just no roles.
        {
            if may_ping_all {
                for (word, note) in [("everyone", "Notify everyone who can see this channel"), ("here", "Notify everyone online here")] {
                    if word.starts_with(&lower) {
                        others.push(SuggestItem {
                            emoji: None,
                            avatar: None,
                            label: format!("@{word}"),
                            detail: note.to_string(),
                            mention: Some(PingTarget::Everyone(if word == "here" { "@here" } else { "@everyone" })),
                            custom: None,
                        });
                    }
                }
            }
        }
        if let Some(guild) = self.guild.filter(|_| may_ping_all || !query.is_empty()) {
            let mut roles: Vec<_> = cache.guild(guild).map(|g| g.roles.iter().collect()).unwrap_or_default();
            roles.retain(|r: &&cord_core::model::Role| r.id != guild && (r.mentionable || may_ping_all) && r.name.to_lowercase().contains(&lower));
            roles.sort_by_key(|r| std::cmp::Reverse(r.position));
            others.extend(roles.into_iter().take(4).map(|r| SuggestItem {
                emoji: None,
                avatar: None,
                label: format!("@{}", r.name),
                detail: "Role".to_string(),
                mention: Some(PingTarget::Role(r.id)),
                custom: None,
            }));
        }
        let people = 8usize.saturating_sub(others.len().min(3));
        let mut items: Vec<SuggestItem> = cache
            .mention_candidates(self.guild, channel, query, people + 1)
            .into_iter()
            .filter(|u| Some(u.id) != me || !query.is_empty())
            .take(people)
            .map(|user| {
                let name = cache.display_name(self.guild, user).to_string();
                SuggestItem {
                    emoji: None,
                    avatar: Some(cache.avatar_url(self.guild, user, 44)),
                    detail: user.handle(),
                    label: name,
                    mention: Some(PingTarget::User(user.id)),
                    custom: None,
                }
            })
            .collect();
        items.extend(others);
        items.truncate(8);
        items
    }

    fn move_suggestion(&mut self, by: i32, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.suggest {
            let n = s.items.len() as i32;
            s.selected = ((s.selected as i32 + by).rem_euclid(n)) as usize;
            cx.notify();
        }
    }

    fn accept_suggestion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(s) = self.suggest.take() else { return };
        let Some(item) = s.items.get(s.selected).cloned() else { return };
        if s.cursor > self.input.read(cx).value().len() {
            return;
        }
        let custom = item.custom.clone().map(|(name, id, animated)| {
            ping_token(self.ctx.services.store.read(cx).cache(), self.guild, ComposerPing::CustomEmoji { name, id, animated })
        });
        let token = match (item.mention, item.emoji) {
            _ if custom.is_some() => custom.expect("just checked"),
            (Some(PingTarget::User(id)), _) => user_token(id, &item.label),
            (Some(PingTarget::Role(id)), _) => InlineToken::new(format!("role:{id}"), format!("<@&{id}>")).with_label(item.label.clone()),
            (Some(PingTarget::Everyone(word)), _) => InlineToken::new(format!("ping:{word}"), word),
            (None, Some(emoji)) => emoji_token(emoji),
            (None, None) => return,
        };
        self.input.update(cx, |input, cx| {
            let _ = input.replace_range_with_token(s.start..s.cursor, token, window, cx);
            input.insert(" ", window, cx);
        });
        cx.notify();
    }

    fn pick_suggestion(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = &mut self.suggest {
            s.selected = index;
        }
        self.accept_suggestion(window, cx);
        self.input.update(cx, |input, cx| input.focus(window, cx));
    }

    /// Looks on the clipboard for what the input can't paste: a picture, or copied files.
    fn paste_files(&mut self, cx: &mut Context<Self>) {
        let job = cx.background_executor().spawn(async { crate::uploads::from_clipboard() });
        cx.spawn(async move |this, cx| {
            let found = job.await;
            let _ = this.update(cx, |this, cx| this.attach(found, cx));
        })
        .detach();
    }

    /// The + button: the file picker.
    fn pick_files(&mut self, cx: &mut Context<Self>) {
        let picked = cx.prompt_for_paths(gpui_kit::PathPromptOptions { files: true, directories: false, multiple: true, prompt: None });
        cx.spawn(async move |this, cx| {
            let Ok(Ok(Some(paths))) = picked.await else { return };
            let found = paths.iter().map(|p| crate::uploads::Pending::from_path(p)).collect();
            let _ = this.update(cx, |this, cx| this.attach(found, cx));
        })
        .detach();
    }

    fn attach(&mut self, found: Vec<crate::uploads::Pending>, cx: &mut Context<Self>) {
        let limit = self.channel.map(|c| self.ctx.services.store.read(cx).cache().upload_limit(c));
        for file in found {
            let size = std::fs::metadata(&file.path).map_or(0, |m| m.len());
            if limit.is_some_and(|limit| size > limit) {
                let says = crate::uploads::too_large(&file.name, size, limit.unwrap_or(0));
                file.discard();
                self.ctx.services.toast(Toast::warning("File too large", says), cx);
                continue;
            }
            if self.pending.len() >= crate::uploads::MAX_FILES {
                file.discard();
                self.ctx.services.toast(Toast::warning("Too many files", format!("A message can carry {} at most.", crate::uploads::MAX_FILES)), cx);
                break;
            }
            self.pending.push(file);
        }
        self.keep_files(cx);
        cx.notify();
    }

    /// The files waiting to go, as tiles above the message box.
    fn pending_row(&self, cx: &Context<Self>) -> Option<AnyElement> {
        if self.pending.is_empty() {
            return None;
        }
        let theme = cx.theme();
        let (tile, border, muted, danger) = (theme.foreground.opacity(0.05), theme.border, theme.muted_foreground, theme.danger);
        let tiles = self.pending.iter().enumerate().map(|(i, file)| {
            v_flex()
                .relative()
                .w(u(200.))
                .h(u(200.))
                .flex_shrink_0()
                .p(u(8.))
                .gap(u(8.))
                .rounded(u(8.))
                .border_1()
                .border_color(border)
                .bg(tile)
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .map(|d| {
                            if file.is_image() {
                                d.child(img(file.path.clone()).size_full().rounded(u(4.)).object_fit(gpui_kit::ObjectFit::Contain))
                            } else {
                                d.text_color(muted).child(Icon::new(IconName::Paperclip).size(u(48.)))
                            }
                        }),
                )
                .child(div().flex_shrink_0().truncate().text_size(u(14.)).child(file.name.clone()))
                .child(
                    div()
                        .id(("remove-upload", i))
                        .absolute()
                        .top(u(4.))
                        .right(u(4.))
                        .size(u(28.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(u(6.))
                        .bg(gpui_kit::black().opacity(0.7))
                        .text_color(danger)
                        .cursor_pointer()
                        .hover(|d| d.opacity(0.8))
                        .on_click(cx.listener(move |this, _, _, cx| {
                            if i < this.pending.len() {
                                this.pending.remove(i).discard();
                                this.keep_files(cx);
                                cx.notify();
                            }
                        }))
                        .child(Icon::new(IconName::Trash).size(u(16.))),
                )
        });
        Some(
            h_flex()
                .id("pending-files")
                .p(u(12.))
                .gap(u(12.))
                .overflow_x_scroll()
                .when(self.replying.is_none(), |d| d.rounded_t(u(8.)))
                .border_1()
                .border_b_0()
                .border_color(border)
                .bg(theme.popover)
                .children(tiles)
                .into_any_element(),
        )
    }

    fn send(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel else { return };
        let text = self.input.read(cx).value().to_string();
        // Files can go with no text at all.
        let prepared = match nav::prepare_message(&text) {
            Err("") if !self.pending.is_empty() => Ok(String::new()),
            other => other,
        };
        match prepared {
            Ok(content) => {
                self.input.update(cx, |input, cx| input.set_value("", window, cx));
                if let Some(message) = self.editing {
                    self.ctx.services.store.update(cx, |store, cx| store.edit_message(channel, message, content, cx));
                    self.leave_edit(window, cx);
                    return;
                }
                crate::drafts::set_text(channel, String::new(), cx);
                let ping = self.reply_ping;
                let reply = self.replying.take().map(|(id, _, _)| (id, ping));
                crate::drafts::set_files(channel, Vec::new(), cx);
                let files = std::mem::take(&mut self.pending).into_iter().filter_map(|f| f.into_upload()).collect();
                self.ctx.services.store.update(cx, |store, cx| store.send(channel, content, reply, files, cx));
            }
            Err("") => {}
            // Too long: keep the text so nothing is lost.
            Err(message) => self.ctx.services.toast(Toast::warning("Message too long", message), cx),
        }
    }

    /// Shift+click on a name or avatar: put a mention in the message box.
    fn mention(&mut self, id: Id, name: String, window: &mut Window, cx: &mut Context<Self>) {
        self.input.update(cx, |input, cx| {
            let _ = input.replace_with_token(user_token(id, &name), window, cx);
            input.insert(" ", window, cx);
            input.focus(window, cx);
        });
    }

    /// Live markdown: what the marks wrap looks the part as it's typed.
    fn style_composer(&mut self, cx: &mut Context<Self>) {
        let theme = cx.theme();
        let (dim, code_bg, cover) = (theme.muted_foreground, theme.foreground.opacity(0.08), theme.muted_foreground.opacity(0.35));
        let text = self.input.read(cx).value().to_string();
        let styles = crate::composer_md::styles(&text)
            .into_iter()
            .map(|(range, look)| {
                let under = gpui_kit::UnderlineStyle { thickness: gpui_kit::px(1.), color: None, wavy: false };
                let strike = gpui_kit::StrikethroughStyle { thickness: gpui_kit::px(1.), color: None };
                let style = gpui_kit::HighlightStyle {
                    color: look.mark.then_some(dim),
                    font_weight: look.bold.then_some(FontWeight::BOLD),
                    font_style: look.italic.then_some(gpui_kit::FontStyle::Italic),
                    underline: look.underline.then_some(under),
                    strikethrough: look.strike.then_some(strike),
                    background_color: if look.code { Some(code_bg) } else if look.spoiler { Some(cover) } else { None },
                    ..Default::default()
                };
                (range, style)
            })
            .collect();
        self.input.update(cx, |input, cx| input.set_text_styles(styles, cx));
    }

    /// Turns a finished `:joy:`, or an emoji typed or pasted as a character,
    /// into an emoji token. One per call; whether it changed anything.
    fn convert_typed(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let input = self.input.read(cx);
        let (text, cursor) = (input.value().to_string(), input.cursor());
        let tokens: Vec<std::ops::Range<usize>> = input.tokens().iter().map(|t| t.range()).collect();
        let free = |range: &std::ops::Range<usize>| !tokens.iter().any(|t| t.start < range.end && range.start < t.end);
        // After a `\\`, a shortcode becomes the plain character: not Apple's picture, as sent raw.
        let finished = crate::emoji_suggest::finished_shortcode(&text, cursor)
            .map(|(start, emoji)| {
                let token = if text[..start].ends_with('\\') { InlineToken::new(format!("plain-emoji:{emoji}"), emoji.to_string()) } else { emoji_token(emoji) };
                (start..cursor, token)
            })
            .filter(|(range, _)| free(range));
        // Written out in full: `<@id>`, `<#id>`, `<@&id>`, `<:name:id>`. Not @everyone,
        // which only the suggestions offer, as they know if it's allowed.
        let usable: Vec<(String, Id, bool)> = {
            let store = self.ctx.services.store.read(cx);
            self.channel.map(|c| store.cache().usable_emojis(c)).unwrap_or_default().into_iter().map(|e| (e.name.clone(), e.id, e.animated)).collect()
        };
        // A server emoji by name. After a `\\` it's left as its raw form.
        let by_name = || {
            let before = &text[..cursor];
            let start = before.strip_suffix(':')?.rfind(':')?;
            let name = &before[start + 1..before.len() - 1];
            let (name, id, animated) = usable.iter().find(|(n, _, _)| n == name)?.clone();
            let range = start..cursor;
            free(&range).then_some(())?;
            let raw = format!("<{}:{name}:{id}>", if animated { "a" } else { "" });
            Some(if text[..start].ends_with('\\') {
                (range, InlineToken::new(format!("plain-emoji:{raw}"), raw))
            } else {
                let store = self.ctx.services.store.read(cx);
                (range, ping_token(store.cache(), self.guild, ComposerPing::CustomEmoji { name, id, animated }))
            })
        };
        // A pasted emoji only turns into its picture if it's ours to use; otherwise
        // it stays the text it is, as the official client wouldn't send it either.
        let typed = || {
            let store = self.ctx.services.store.read(cx);
            composer_pings(&text)
                .into_iter()
                .filter(|(range, ping)| {
                    free(range)
                        && match ping {
                            ComposerPing::Everyone(_) => false,
                            ComposerPing::CustomEmoji { id, .. } => usable.iter().any(|(_, known, _)| known == id),
                            _ => true,
                        }
                })
                .next()
                .map(|(range, ping)| (range, ping_token(store.cache(), self.guild, ping)))
        };
        let found = finished.or_else(by_name).or_else(typed).or_else(|| {
            // An emoji after a `\\` is left as it is.
            let mut skip = tokens.clone();
            skip.extend(text.match_indices('\\').map(|(at, _)| {
                let rest = &text[at + 1..];
                at..at + 1 + rest.find(char::is_whitespace).unwrap_or(rest.len())
            }));
            crate::emoji_suggest::raw_emoji(&text, &skip).map(|range| (range.clone(), emoji_token(&text[range])))
        });
        let Some((range, token)) = found else { return false };
        self.input.update(cx, |input, cx| {
            let _ = input.replace_range_with_token(range, token, window, cx);
        });
        true
    }

    fn notice(&self, text: impl Into<String>, cx: &Context<Self>) -> AnyElement {
        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .p_8()
            .text_center()
            .text_color(cx.theme().muted_foreground)
            .child(text.into())
            .into_any_element()
    }
}

/// A voice channel's chat panel: its starting width, narrowest width, and the
/// width last dragged to, kept for the next voice channel opened.
const VC_CHAT_DEFAULT: f32 = 460.;
const VC_CHAT_MIN: f32 = 320.;
/// How many rows short of a far-off message a jump lands before gliding in.
const JUMP_RUN_UP: usize = 8;
/// The pill appears this many rows from the bottom: 30 messages above what a
/// screenful holds.
const JUMP_PILL_AFTER: usize = 40;
/// A participant tile, with the stage's padding around it.
const VC_TILE_ROOM: f32 = 280. + 32.;

/// The widest the chat panel may go: one column of tiles keeps its room.
fn vc_chat_max(window: &Window, column: f32) -> f32 {
    let total = f32::from(window.viewport_size().width) / (f32::from(window.rem_size()) / 16.);
    (total - 72. - column - 5. - VC_TILE_ROOM).max(VC_CHAT_MIN)
}

struct VoiceChatWidth(f32);

impl gpui_kit::Global for VoiceChatWidth {}

/// A mention in the message box: one unit to edit, drawn `@Name`, sent as `<@id>`.
/// A ping in a message's raw text.
#[derive(Clone, Debug, PartialEq, Eq)]
enum ComposerPing {
    User(Id),
    Role(Id),
    Channel(Id),
    Everyone(&'static str),
    /// A server's emoji: `<:name:id>`, or `<a:name:id>` if animated.
    CustomEmoji { name: String, id: Id, animated: bool },
}

/// The token a ping shows as in the message box. Its text stays the raw form.
fn ping_token(cache: &Cache, guild: Option<Id>, ping: ComposerPing) -> InlineToken {
    match ping {
        ComposerPing::User(id) => {
            let name = cache.user(id).map_or_else(|| "unknown-user".to_string(), |u| cache.display_name(guild, u).to_string());
            user_token(id, &name)
        }
        ComposerPing::Role(id) => {
            let name = guild.and_then(|g| cache.guild(g)).and_then(|g| g.roles.iter().find(|r| r.id == id)).map_or("deleted-role".to_string(), |r| r.name.clone());
            InlineToken::new(format!("role:{id}"), format!("<@&{id}>")).with_label(format!("@{name}"))
        }
        ComposerPing::Channel(id) => {
            let name = cache.channel(id).map_or_else(|| "unknown".to_string(), |c| nav::channel_label(cache, c));
            InlineToken::new(format!("channel:{id}"), format!("<#{id}>")).with_label(format!("#{name}"))
        }
        ComposerPing::Everyone(word) => InlineToken::new(format!("ping:{word}"), word),
        ComposerPing::CustomEmoji { name, id, animated } => {
            let raw = format!("<{}:{name}:{id}>", if animated { "a" } else { "" });
            InlineToken::new(format!("custom-emoji:{}", crate::message_text::custom_emoji_url(id, animated)), raw).with_label(format!(":{name}:"))
        }
    }
}

/// The pings in `text` and where they are, in order. One after a `\\` stays raw.
fn composer_pings(text: &str) -> Vec<(std::ops::Range<usize>, ComposerPing)> {
    let mut found = Vec::new();
    let mut at = 0;
    while at < text.len() {
        let rest = &text[at..];
        let ping = if let Some(word) = ["@everyone", "@here"].into_iter().find(|w| rest.starts_with(*w)) {
            Some((word.len(), ComposerPing::Everyone(word)))
        } else if rest.starts_with('<') {
            rest.find('>').and_then(|end| {
                let inner = &rest[1..end];
                let (make, digits): (fn(Id) -> ComposerPing, &str) = if let Some(d) = inner.strip_prefix("@&") {
                    (ComposerPing::Role, d)
                } else if let Some(d) = inner.strip_prefix("@!").or_else(|| inner.strip_prefix('@')) {
                    (ComposerPing::User, d)
                } else if let Some(d) = inner.strip_prefix('#') {
                    (ComposerPing::Channel, d)
                } else {
                    let (animated, rest) = match inner.strip_prefix("a:") {
                        Some(rest) => (true, rest),
                        None => (false, inner.strip_prefix(':')?),
                    };
                    let (name, digits) = rest.split_once(':')?;
                    let valid = !name.is_empty() && name.chars().all(|c| c.is_alphanumeric() || c == '_');
                    return valid.then_some(())
                        .and_then(|()| digits.parse().ok())
                        .map(|id| (end + 1, ComposerPing::CustomEmoji { name: name.to_string(), id: Id(id), animated }));
                };
                Some((end + 1, make(Id(digits.parse().ok()?))))
            })
        } else {
            None
        };
        match ping {
            Some((len, ping)) => {
                if !text[..at].ends_with('\\') {
                    found.push((at..at + len, ping));
                }
                at += len;
            }
            None => at += rest.chars().next().map_or(1, char::len_utf8),
        }
    }
    found
}

/// How far to scroll so a row `distance` below the view's top and `height` tall
/// shows in full, moving no further than that. `None`: it already does. A row
/// not measured yet (it's above the view) only knows it has to come down.
fn reveal_amount(distance: f32, height: Option<f32>, view: f32) -> Option<f32> {
    const MARGIN: f32 = 16.;
    let Some(height) = height else {
        return (distance < 0.).then_some(distance - MARGIN);
    };
    if height >= view - MARGIN * 2. {
        // Taller than the view: its top is what matters.
        return ((distance - MARGIN).abs() > 1.).then_some(distance - MARGIN);
    }
    if distance < 0. {
        Some(distance - MARGIN)
    } else if distance + height > view {
        Some(distance + height - view + MARGIN)
    } else {
        None
    }
}

/// How many voice tiles to a row and their size, in a stage `room` wide.
fn voice_tile_size(room: f32, people: usize) -> (usize, f32, f32) {
    const MIN_W: f32 = 280.;
    const MAX_W: f32 = 640.;
    const GAP: f32 = 8.;
    let fit = (((room + GAP) / (MIN_W + GAP)).floor() as usize).clamp(1, people.max(1));
    // Whole pixels, so a resize moves them in steps instead of shimmering.
    let width = ((room - GAP * (fit as f32 - 1.)) / fit as f32).clamp(MIN_W.min(room), MAX_W).floor();
    (fit, width, (width * 9. / 16.).round())
}

fn user_token(id: Id, name: &str) -> InlineToken {
    InlineToken::new(format!("user:{id}"), format!("<@{id}>")).with_label(format!("@{name}"))
}

/// An emoji in the message box: drawn as its picture, sent as the character.
fn emoji_token(emoji: &str) -> InlineToken {
    InlineToken::new(format!("emoji:{emoji}"), emoji.to_string())
}

/// How the message box draws its tokens: Apple's emoji, and Discord's pill.
fn composer_token(token: &InlineTokenContext, _: &mut Window, cx: &mut App) -> AnyElement {
    let line = token.line_height();
    let selected = cx.theme().selection;
    let frame = div().h(line).flex().items_center().rounded(u(3.)).when(token.is_selected(), |d| d.bg(selected));
    if let Some(url) = token.token().id().strip_prefix("custom-emoji:") {
        return frame.child(img(url.to_string()).size(u(22.)).flex_shrink_0()).into_any_element();
    }
    if token.token().id().starts_with("plain-emoji:") {
        return frame.child(token.token().text().to_string()).into_any_element();
    }
    if token.token().id().starts_with("emoji:") {
        return frame.child(crate::badges::emoji_image(token.token().text(), 22.)).into_any_element();
    }
    frame
        .px(u(2.))
        .when(!token.is_selected(), |d| d.bg(Hsla::from(gpui_kit::rgb(0x5865f2)).opacity(0.24)))
        .text_color(Hsla::from(gpui_kit::rgb(0xa9b4ff)))
        .child(token.token().label().clone())
        .into_any_element()
}

/// Whether a channel kind has messages to show: voice channels do.
fn has_chat(kind: ChannelKind) -> bool {
    kind.is_text_like() || matches!(kind, ChannelKind::Voice | ChannelKind::Stage)
}

/// Grey stand-ins for message groups while a channel's history loads, as
/// Discord draws them: an avatar circle, a name bar, then lines of "words".
fn skeleton_messages(cx: &App) -> AnyElement {
    // Word widths per line, in px; repeating with variation looks like text.
    const GROUPS: [&[&[f32]]; 7] = [
        &[&[60., 90., 40., 120., 70.]],
        &[&[110., 50., 80.], &[40., 140., 60., 90., 30., 70.]],
        &[&[70., 40.]],
        &[&[90., 130., 50., 60., 110.], &[60., 80.], &[150., 40., 90.]],
        &[&[50., 70., 100., 40.]],
        &[&[120., 60., 90., 50., 80., 40.], &[70., 110.]],
        &[&[80., 50., 130.]],
    ];
    let theme = cx.theme();
    let (name, word) = (theme.foreground.opacity(0.16), theme.foreground.opacity(0.08));
    let bar = |w: f32, h: f32, color: Hsla| div().w(u(w)).h(u(h)).flex_shrink_0().rounded_full().bg(color);
    let groups = GROUPS.iter().enumerate().map(|(g, lines)| {
        h_flex()
            .w_full()
            .px_4()
            .pt(u(17.))
            .gap_4()
            .items_start()
            .child(div().size(u(AVATAR)).flex_shrink_0().rounded_full().bg(name))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .gap(u(8.))
                    .pt(u(3.))
                    .child(h_flex().gap(u(8.)).child(bar(60. + (g % 3) as f32 * 30., 14., name)).child(bar(40., 10., word)))
                    .children(lines.iter().map(|words| {
                        h_flex().gap(u(6.)).overflow_hidden().children(words.iter().map(|w| bar(*w, 14., word)))
                    })),
            )
    });
    v_flex()
        .size_full()
        .justify_end()
        .overflow_hidden()
        .pb(u(24.))
        .children(groups)
        .with_animation(
            "skeleton",
            gpui_kit::Animation::new(std::time::Duration::from_millis(1600)).repeat().with_easing(gpui_kit::bounce(gpui_kit::ease_in_out)),
            |this, t| this.opacity(1. - t * 0.4),
        )
        .into_any_element()
}

/// "Message #general" / "Message @ada", once the cache knows the channel.
fn placeholder_for(cache: &Cache, channel: Id) -> Option<String> {
    let c = cache.channel(channel)?;
    let prefix = match c.kind {
        ChannelKind::Voice | ChannelKind::Stage => "",
        _ if c.guild_id.is_some() => "#",
        _ => "@",
    };
    Some(format!("Message {prefix}{}", nav::channel_label(cache, c)))
}

const AVATAR: f32 = 40.;
/// List id of the row above the first message.
const TOP_ROW: Id = Id(0);

/// Where an element was last painted, so a card can open beside it.
type Anchor = Rc<std::cell::Cell<Option<gpui_kit::Bounds<Pixels>>>>;

/// `element`, with its bounds noted in `anchor` as it's painted.
fn anchored_by(anchor: &Anchor, element: impl IntoElement) -> gpui_kit::Div {
    let anchor = anchor.clone();
    div().flex_shrink_0().on_children_prepainted(move |bounds, _, _| anchor.set(bounds.first().copied())).child(element)
}

/// Shift+click mentions the author; a plain click opens their profile card,
/// just past the end of what was clicked.
fn on_author_click(
    page: &WeakEntity<ChannelPage>,
    author: &cord_core::model::User,
    name: &str,
    anchor: &Anchor,
) -> impl Fn(&ClickEvent, &mut Window, &mut App) + 'static {
    let (page, id, name, anchor) = (page.clone(), author.id, name.to_string(), anchor.clone());
    move |event, window, cx| {
        let position = anchor.get().map_or(event.position(), |b| gpui_kit::point(b.right() + gpui_kit::px(8.), b.top()));
        let shift = event.modifiers().shift;
        let _ = page.update(cx, |page, cx| {
            if shift {
                page.mention(id, name.clone(), window, cx)
            } else {
                page.open_profile(id, position, window, cx)
            }
        });
    }
}

/// Hovering a name or avatar fetches their profile ahead of a click.
fn on_author_hover(page: &WeakEntity<ChannelPage>, user: Id) -> impl Fn(&bool, &mut Window, &mut App) + 'static {
    let page = page.clone();
    move |over, _, cx| {
        let over = *over;
        let _ = page.update(cx, |page, cx| {
            let core = page.ctx.services.core.clone();
            crate::profile_preload::hover(user, page.guild, over, &core, cx);
        });
    }
}

/// Right click on a name or avatar: that person's menu, not the message's.
fn on_author_menu(
    page: &WeakEntity<ChannelPage>,
    author: &cord_core::model::User,
    name: &str,
) -> impl Fn(&gpui_kit::MouseDownEvent, &mut Window, &mut App) + 'static {
    let (page, id, name) = (page.clone(), author.id, name.to_string());
    move |event, _, cx| {
        cx.stop_propagation();
        let (position, name) = (event.position, name.clone());
        let _ = page.update(cx, |page, cx| page.user_menu(id, name, position, cx));
    }
}

/// Right click anywhere else on a row: the message's menu.
fn on_row_menu(page: &WeakEntity<ChannelPage>, id: Id) -> impl Fn(&gpui_kit::MouseDownEvent, &mut Window, &mut App) + 'static {
    let page = page.clone();
    move |event, _, cx| {
        let position = event.position;
        let _ = page.update(cx, |page, cx| page.message_menu(id, position, cx));
    }
}

/// Discord's gold for messages that ping the viewer.
pub(crate) const GOLD: Hsla = Hsla { h: 0.114, s: 0.87, l: 0.56, a: 1.0 };

fn mention_bar() -> impl IntoElement {
    div().absolute().left_0().top_0().bottom_0().w(u(2.)).bg(GOLD)
}

/// The row of the message being replied to: blurple, a little stronger under the pointer.
fn reply_tint(lit: bool) -> Hsla {
    Hsla::from(gpui_kit::rgb(0x5865f2)).opacity(if lit { 0.16 } else { 0.11 })
}

fn reply_bar() -> impl IntoElement {
    div().absolute().left_0().top_0().bottom_0().w(u(2.)).bg(gpui_kit::rgb(0x5865f2))
}

/// Whether `message` mentions the signed-in user, by name, role or @everyone.
fn pings_me(cache: &Cache, guild: Option<Id>, message: &Message) -> bool {
    let Some(me) = cache.me() else { return false };
    if message.author.id == me.id {
        return false;
    }
    let roles = guild.and_then(|g| cache.member(g, me.id)).map(|m| m.roles.as_slice()).unwrap_or(&[]);
    chat_format::mentions_me(message, me.id, roles)
}

/// Who a message is from: their name in this server and its colour.
struct Byline {
    name: String,
    color: Option<Hsla>,
    bot: bool,
    /// Server tag and its badge image.
    tag: Option<(String, Option<String>)>,
    role_icon: Option<cord_core::cache::RoleIcon>,
    decoration: Option<String>,
    /// A display name style's effect and font.
    effect: u8,
    effect_color: Option<Hsla>,
    font: Option<&'static str>,
    /// Joined the server within the week: the "new here" mark.
    new_member: bool,
    /// Gradient stops for the name (two or more), from its style or role.
    stops: Vec<Hsla>,
}

fn byline(cache: &Cache, guild: Option<Id>, author: &cord_core::model::User) -> Byline {
    // The cache has the freshest copy of the user (styles come with it).
    let styled = cache.styled_user(author);
    let role_colors = guild.map(|g| cache.member_colors(g, author.id)).unwrap_or_default();
    let look = crate::badges::name_look(styled, &role_colors);
    Byline {
        effect: look.effect,
        effect_color: look.effect_color,
        font: look.font,
        stops: look.stops,
        new_member: guild.is_some_and(|g| cache.is_new_member(g, author.id)),
        name: cache.display_name(guild, author).to_string(),
        color: look.color,
        bot: author.bot,
        tag: author.server_tag().map(|(t, b)| (t.to_string(), b)),
        role_icon: guild.and_then(|g| cache.role_icon(g, author.id)),
        decoration: author.decoration_url(),
    }
}

/// One message. `stacked` rows sit under the previous message from the same
/// person: no avatar or name, the time shows in the gutter on hover.
/// Compact mode: time, name and text on one line for every message.
fn compact_row(ix: usize, m: &ChatMessage, guild: Option<Id>, cache: &Cache, shared: &Shared, page: &WeakEntity<ChannelPage>, replying_to: Option<Id>, cx: &App) -> AnyElement {
    let author = &m.message.author;
    let by = byline(cache, guild, author);
    let replying = replying_to == Some(m.message.id);
    let mine = !replying && pings_me(cache, guild, &m.message);
    let theme = cx.theme();
    let name_anchor = Anchor::default();
    div()
        .id(("message", ix))
        .on_mouse_down(gpui_kit::MouseButton::Right, on_row_menu(page, m.message.id))
        .w_full()
        .px_4()
        .py(u(1.))
        .relative()
        .when(mine, |this| this.bg(GOLD.opacity(0.08)))
        .hover(|style| style.bg(if mine { GOLD.opacity(0.12) } else { theme.secondary }))
        .when(mine, |this| this.child(mention_bar()))
        .when(replying, |this| this.bg(reply_tint(false)).hover(|style| style.bg(reply_tint(true))).child(reply_bar()))
        .when(m.state == SendState::Pending, |this| this.opacity(0.5))
        .children(rich_view::reply_line(&Env { shared, cache, message: &m.message, guild }, 8., cx))
        .child(
            h_flex()
                .gap_2()
                .items_start()
                .child(
                    div()
                        .w(u(AVATAR))
                        .flex_shrink_0()
                        .pt(u(2.))
                        .text_xs()
                        .text_color(theme.muted_foreground)
                        .child(nav::short_time(m.message.id.timestamp_ms(), &chrono::Local)),
                )
                .child(anchored_by(&name_anchor,
                    div()
                        .id(("author", ix))
                        .flex_shrink_0()
                        .font_weight(FontWeight::SEMIBOLD)
                        .cursor_pointer()
                        .hover(|style| style.underline())
                        .when_some(by.color, |this, color| this.text_color(color))
                        .when_some(by.font, |this, font| this.font_family(font))
                        .on_click(on_author_click(page, author, &by.name, &name_anchor))
                        .child(crate::badges::gradient_text(&by.name, &by.stops, 0.)),
                ))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(message_content(&m.message, guild, cache, shared, cx))
                        .when(m.state == SendState::Failed, |this| {
                            this.child(div().text_xs().text_color(theme.danger).child("Failed to send"))
                        }),
                ),
        )
        .into_any_element()
}

/// A topic's first line without markdown marks: `**bold**` → `bold`.
fn plain_topic(topic: &str) -> String {
    let line = topic.lines().find(|l| !l.trim().is_empty()).unwrap_or("").trim();
    let line = line.trim_start_matches('#').trim_start_matches("-#").trim_start_matches('>').trim();
    ["***", "**", "__", "~~", "||", "`"].iter().fold(line.to_string(), |text, mark| text.replace(mark, ""))
}

/// "Niz is typing…" in the gap above the composer, as Discord draws it.
fn typing_line(store: &Entity<Store>, channel: Id, guild: Option<Id>, cx: &App) -> Option<AnyElement> {
    let store = store.read(cx);
    let cache = store.cache();
    let typing: Vec<String> = store
        .typing(channel)
        .into_iter()
        .map(|id| cache.user(id).map_or_else(|| "Someone".to_string(), |u| cache.display_name(guild, u).to_string()))
        .collect();
    // On the way out it still says who it was.
    thread_local! {
        static LAST_TYPING: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }
    let on = !typing.is_empty();
    if on {
        LAST_TYPING.with_borrow_mut(|last| last.clone_from(&typing));
    }
    let names = LAST_TYPING.with_borrow(Clone::clone);
    let fade_id = gpui_kit::SharedString::from(format!("typing-{channel}"));
    let (shown, moving) = crate::transition::progress(&fade_id, on);
    if shown <= 0. && !moving {
        return None;
    }
    let eased = crate::transition::ease(shown);
    // Somebody new: the names slide up into place.
    let who = {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        names.hash(&mut hasher);
        hasher.finish() as usize
    };
    let theme = cx.theme();
    let bold = |name: &str| div().font_weight(FontWeight::BOLD).text_color(theme.foreground).child(name.to_string());
    let mut line = h_flex().gap(u(4.)).relative();
    line = match names.as_slice() {
        [a] => line.child(bold(a)).child("is typing…"),
        [a, b] => line.child(bold(a)).child("and").child(bold(b)).child("are typing…"),
        [a, b, c] => line.child(bold(&format!("{a},"))).child(bold(&format!("{b},"))).child("and").child(bold(c)).child("are typing…"),
        _ => line.child("Several people are typing…"),
    };
    Some(
        h_flex()
            .absolute()
            .bottom(u((eased - 1.) * 6.))
            .opacity(eased)
            .left_0()
            .right_0()
            .h(u(24.))
            .pl(u(16.))
            .gap(u(16.))
            .items_center()
            .text_size(u(14.))
            .line_height(u(18.))
            .text_color(theme.muted_foreground)
            .bg(theme.background.opacity(0.9))
            .child(h_flex().w(u(40.)).justify_center().gap(u(3.)).children((0..3).map(|i| {
                // Three dots pulsing one after another, as Discord's do.
                let dot = theme.foreground;
                div().size(u(6.)).rounded_full().bg(dot).with_animation(
                    ("typing-dot", i as usize),
                    gpui_kit::Animation::new(std::time::Duration::from_millis(1200)).repeat(),
                    move |d, t| {
                        let wave = ((t - i as f32 / 3.).rem_euclid(1.) * std::f32::consts::TAU).cos() * 0.5 + 0.5;
                        d.opacity(0.3 + 0.7 * wave)
                    },
                )
            })))
            .child(line.with_animation(
                ("typing-names", who),
                gpui_kit::Animation::new(std::time::Duration::from_millis(220)).with_easing(gpui_kit::ease_out_quint()),
                |d, t| d.top(u((1. - t) * 8.)).opacity(t),
            ))
            .when(moving, |d| d.child(crate::transition::driver()))
            .into_any_element(),
    )
}

/// Discord's quick reactions at the front of the hover toolbar.
const QUICK_REACTIONS: [&str; 3] = ["😭", "❤️", "💀"];

/// The raised toolbar at a hovered message's top right, as Discord draws it:
/// three quick reactions, a divider, then the actions (only reacting works yet).
fn hover_actions(
    ix: usize,
    own: bool,
    cache: &Cache,
    message: &Message,
    author_name: String,
    page: &WeakEntity<ChannelPage>,
    hovered_tools: &Rc<std::cell::Cell<Option<Id>>>,
    cx: &App,
) -> impl IntoElement {
    let theme = cx.theme();
    let (muted, bright, hover) = (theme.muted_foreground, theme.foreground, theme.foreground.opacity(0.08));
    let cell = move |id: (&'static str, usize)| {
        div().id(id).size(u(32.)).flex().items_center().justify_center().rounded(u(6.)).cursor_pointer().hover(move |d| d.bg(hover))
    };
    let button = |name: &'static str, icon: IconName, tip: &'static str| {
        cell((name, ix))
            .text_color(muted)
            .hover(move |d| d.text_color(bright))
            .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
            .child(Icon::new(icon).size(u(20.)))
    };
    let (channel, id) = (message.channel_id, message.id);
    use cord_core::permissions as perms;
    let can_react = cache.has_permission(channel, perms::ADD_REACTIONS);
    // Nobody moderates a DM or a group, whatever permissions it nominally gives.
    let in_server = cache.channel(channel).is_some_and(|c| c.guild_id.is_some());
    let can_manage = in_server && cache.has_permission(channel, perms::MANAGE_MESSAGES);
    let can_pin = !in_server || can_manage || cache.has_permission(channel, perms::PIN_MESSAGES);
    let can_send = cache.can_send(channel);
    let shift = crate::ui::shift_held();
    let pinned = message.pinned;
    let quick = QUICK_REACTIONS.iter().enumerate().map(|(i, emoji)| {
        let page = page.clone();
        let name = emoji.to_string();
        cell((["act-q0", "act-q1", "act-q2"][i], ix))
            .text_size(u(20.))
            .on_click(move |_, _, cx| {
                let emoji = ReactionEmoji { name: Some(name.clone()), ..Default::default() };
                let _ = page.update(cx, |page, cx| {
                    page.ctx.services.store.update(cx, |store, cx| store.set_reaction(channel, id, emoji, true, cx))
                });
            })
            .child(crate::badges::emoji_image(emoji, 20.))
    });
    h_flex()
        .id(("toolbar", ix))
        .absolute()
        .top(u(-25.))
        .right(u(16.))
        .p(u(2.))
        .items_center()
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_md()
        // Where it overlaps the message above, the pointer is over the toolbar,
        // not this message: that message stays lit, the one above doesn't light.
        .occlude()
        .on_hover({
            let (page, cell, id) = (page.clone(), hovered_tools.clone(), message.id);
            move |over, _, cx| {
                let now = if *over { Some(id) } else if cell.get() == Some(id) { None } else { return };
                if cell.get() != now {
                    cell.set(now);
                    // Moving under the pointer: the render when it stops shows this.
                    if !crate::smooth_scroll::list_scrolling() {
                        let _ = page.update(cx, |_, cx| cx.notify());
                    }
                }
            }
        })
        // Holding Shift, the message's ID at one end and deleting it at the other.
        .when(shift, |this| {
            this.child(
                cell(("act-id", ix))
                    .text_color(muted)
                    .hover(move |d| d.text_color(bright))
                    .text_size(u(12.))
                    .font_weight(FontWeight::BOLD)
                    .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Copy Message ID").build(window, cx))
                    .on_click(move |_, _, cx| cx.write_to_clipboard(gpui_kit::ClipboardItem::new_string(id.to_string())))
                    .child("ID"),
            )
        })
        .when(can_react, |this| {
            this.children(quick)
                .child(div().mx(u(4.)).w(u(1.)).h(u(20.)).bg(theme.border))
                .child(button("act-react", IconName::FaceSlightlySmilingPlus, "Add Reaction"))
        })
        .when(own, |this| {
            this.child(button("act-edit", IconName::Pencil, "Edit").on_click({
                let (page, content) = (page.clone(), message.content.clone());
                move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.start_edit(id, content.clone(), window, cx));
                }
            }))
        })
        .when(can_send, |this| {
            this.child(button("act-reply", IconName::Reply, "Reply").on_click({
                let (page, author) = (page.clone(), author_name);
                move |_, window, cx| {
                    let _ = page.update(cx, |page, cx| page.reply_to(id, author.clone(), window, cx));
                }
            }))
        })
        .when(can_pin, |this| {
            let (icon, tip) = if pinned { ("icons/pin-off.svg", "Unpin Message") } else { ("icons/toolbar-pin.svg", "Pin Message") };
            let pin_button = cell(("act-pin", ix))
                .text_color(muted)
                .hover(move |d| d.text_color(bright))
                .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
                .child(Icon::default().path(icon).size(u(20.)));
            this.child(pin_button.on_click({
                let page = page.clone();
                move |_, _, cx| {
                    let _ = page.update(cx, |page, cx| {
                        page.ctx.services.store.update(cx, |store, cx| {
                            if pinned {
                                store.unpin_message(channel, id, cx)
                            } else {
                                store.pin_message(channel, id, cx)
                            }
                        });
                    });
                }
            }))
        })
        .when(shift && (own || can_manage), |this| {
            this.child(button("act-delete", IconName::Trash, "Delete Message").on_click({
                let page = page.clone();
                move |_, _, cx| {
                    let _ = page.update(cx, |page, cx| {
                        // Someone else's is a moderator's action: that always asks first.
                        if own {
                            page.ctx.services.store.update(cx, |store, cx| store.delete_message(channel, id, cx));
                        } else {
                            page.confirm_delete = Some(id);
                            cx.notify();
                        }
                    });
                }
            }))
        })
}

fn message_row(
    ix: usize,
    m: &ChatMessage,
    stacked: bool,
    margin: bool,
    guild: Option<Id>,
    cache: &Cache,
    shared: &Shared,
    page: &WeakEntity<ChannelPage>,
    hovered: &Rc<std::cell::Cell<Option<Id>>>,
    hovered_tools: &Rc<std::cell::Cell<Option<Id>>>,
    replying_to: Option<Id>,
    cx: &App,
) -> AnyElement {
    let author = &m.message.author;
    let by = byline(cache, guild, author);
    // The message being replied to shows as that, even if it also mentions us.
    let replying = replying_to == Some(m.message.id);
    let mine = !replying && pings_me(cache, guild, &m.message);
    let theme = cx.theme();
    let ms = m.message.id.timestamp_ms();
    let tz = chrono::Local;
    let (name_anchor, avatar_anchor) = (Anchor::default(), Anchor::default());

    let gutter = if stacked {
        div()
            .w(u(AVATAR))
            .flex_shrink_0()
            .pt(u(3.))
            .text_xs()
            .text_right()
            .text_color(gpui_kit::transparent_black())
            .group_hover("message", |style| style.text_color(theme.muted_foreground))
            .child(nav::short_time(ms, &tz))
            .into_any_element()
    } else {
        anchored_by(&avatar_anchor, div()
            .id(("avatar", ix))
            .flex_shrink_0()
            .cursor_pointer()
            .on_hover(on_author_hover(page, author.id))
            .on_click(on_author_click(page, author, &by.name, &avatar_anchor))
            .on_mouse_down(gpui_kit::MouseButton::Right, on_author_menu(page, author, &by.name))
            .child(
                Avatar::new().border_0()
                    .src(cache.avatar_url(guild, author, 80))
                    .name(by.name.clone())
                    .size(u(AVATAR)),
            )
            .relative()
            // Avatar decorations are 1.2× the avatar, centred on it.
            .when_some(by.decoration.clone(), |this, url| {
                this.child(img(url).absolute().top(u(-AVATAR * 0.1)).left(u(-AVATAR * 0.1)).size(u(AVATAR * 1.2)))
            }))
            .into_any_element()
    };

    let header = (!stacked).then(|| {
        h_flex()
            .gap(u(4.))
            .items_center()
            .child(anchored_by(&name_anchor,
                div()
                    .id(("author", ix))
                    .text_size(u(16.))
                    .font_weight(FontWeight::MEDIUM)
                    .cursor_pointer()
                    .hover(|style| style.underline())
                    .when_some(by.color, |this, color| this.text_color(color))
                    .when_some(by.font, |this, font| this.font_family(font))
                    .on_hover(on_author_hover(page, author.id))
                    .on_click(on_author_click(page, author, &by.name, &name_anchor))
                    .on_mouse_down(gpui_kit::MouseButton::Right, on_author_menu(page, author, &by.name))
                    .child(crate::badges::name_effect(crate::badges::name_text(("author-name", ix), &by.name, &by.stops, hovered.get() == Some(m.message.id)), &by.name, &crate::badges::NameLook { effect: by.effect, effect_color: by.effect_color, font: by.font, color: by.color, stops: Vec::new() }, hovered.get() == Some(m.message.id) || hovered_tools.get() == Some(m.message.id)))
            ))
            .when_some(by.tag.clone(), |this, (tag, badge)| {
                this.child(crate::badges::tag_pill(SharedString::from(format!("tag-{ix}")), &tag, badge.as_deref(), cx))
            })
            .when_some(by.role_icon.clone(), |this, icon| {
                this.child(crate::badges::role_icon(SharedString::from(format!("roleicon-{ix}")), &icon, 18.))
            })
            .when(by.new_member, |this| {
                this.child(
                    div()
                        .id(("new-member", ix))
                        .flex_shrink_0()
                        .text_color(Hsla::from(gpui_kit::rgb(0x45a366)))
                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("I'm new here, say hi!").build(window, cx))
                        .child(Icon::default().path("icons/new-member.svg").size(u(20.))),
                )
            })
            .map(|this| {
                if author.is_discord() {
                    this.child(crate::badges::official_badge())
                } else {
                    this.when(by.bot, |this| this.child(crate::badges::app_badge()))
                }
            })
            .child(
                div()
                    .ml(u(4.))
                    .text_size(u(12.))
                    .text_color(theme.muted_foreground)
                    .child(nav::format_message_time(ms, &chrono::Local::now())),
            )
    });

    let lit = !crate::smooth_scroll::list_scrolling() && (hovered.get() == Some(m.message.id) || hovered_tools.get() == Some(m.message.id));
    let ephemeral = m.message.is_ephemeral();
    let row = div()
        .id(("message", ix))
        .on_mouse_down(gpui_kit::MouseButton::Right, on_row_menu(page, m.message.id))
        .on_hover({
            let (page, cell, id) = (page.clone(), hovered.clone(), m.message.id);
            move |over, _, cx| {
                let now = if *over { Some(id) } else if cell.get() == Some(id) { None } else { return };
                if cell.get() != now {
                    cell.set(now);
                    // Moving under the pointer: the render when it stops shows this.
                    if !crate::smooth_scroll::list_scrolling() {
                        let _ = page.update(cx, |_, cx| cx.notify());
                    }
                }
            }
        })
        .group("message")
        .w_full()
        .px_4()
        .py(u(2.))
        .relative()
        .when(mine, |this| this.bg(GOLD.opacity(0.08)))
        // Set by hand, not `.hover`: the quick actions hang outside the row.
        .when(lit, |this| this.bg(if mine { GOLD.opacity(0.12) } else { theme.secondary }))
        .when(mine, |this| this.child(mention_bar()))
        .when(replying, |this| this.bg(reply_tint(lit)).child(reply_bar()))
        .when(ephemeral, |this| {
            let blurple = Hsla::from(gpui_kit::rgb(0x5865f2));
            this.bg(blurple.opacity(0.05)).child(div().absolute().top_0().bottom_0().left_0().w(u(2.)).bg(blurple))
        })
        .when(m.state == SendState::Pending, |this| this.opacity(0.5))
        .children(rich_view::reply_line(&Env { shared, cache, message: &m.message, guild }, AVATAR / 2., cx))
        .child(
            h_flex()
                .gap_4()
                .items_start()
                .child(gutter)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .line_height(gpui_kit::relative(1.375))
                        .children(header)
                        .child(message_content(&m.message, guild, cache, shared, cx))
                        .when(ephemeral, |this| {
                            let (channel, id, page) = (m.message.channel_id, m.message.id, page.clone());
                            this.child(
                                h_flex()
                                    .mt(u(4.))
                                    .gap(u(4.))
                                    .items_center()
                                    .text_size(u(12.))
                                    .text_color(theme.muted_foreground)
                                    .child(Icon::new(IconName::Eye).size(u(16.)))
                                    .child("Only you can see this \u{2022}")
                                    .child(
                                        div()
                                            .id(("dismiss", ix))
                                            .text_color(theme.link)
                                            .cursor_pointer()
                                            .hover(|d| d.underline())
                                            .on_click(move |_, _, cx| {
                                                let _ = page.update(cx, |page, cx| {
                                                    page.ctx.services.store.update(cx, |store, cx| store.dismiss_message(channel, id, cx));
                                                });
                                            })
                                            .child("Dismiss message"),
                                    ),
                            )
                        })
                        .when(m.state == SendState::Failed, |this| {
                            this.child(div().text_xs().text_color(theme.danger).child("Failed to send"))
                        }),
                ),
        )
        // Last, so it paints over the reply line and the name.
        .when(lit && !ephemeral, |this| {
            this.child(hover_actions(ix, cache.me().is_some_and(|me| me.id == author.id), cache, &m.message, by.name.clone(), page, hovered_tools, cx))
        });
    // The 17px between groups sits outside the row (so hovering doesn't
    // light it up), as padding on a wrapper: list items drop their own margins.
    if !stacked && margin {
        div().w_full().pt(u(17.)).child(row).into_any_element()
    } else {
        row.into_any_element()
    }
}

/// "… just showed up!" and the like: an icon, a sentence with the name in it, the time.
#[allow(clippy::too_many_arguments)]
fn system_row(
    ix: usize,
    m: &ChatMessage,
    line: &rich_view::SystemLine,
    margin: bool,
    guild: Option<Id>,
    cache: &Cache,
    shared: &Shared,
    page: &WeakEntity<ChannelPage>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    // Ours to them reads the other way round: the name's theirs, either way.
    let me = cache.me().map(|u| u.id);
    let other = (line.kind == rich_view::SystemKind::FriendAccepted && me == Some(m.message.author.id))
        .then(|| cache.channel(m.message.channel_id)?.recipient_ids.first().and_then(|id| cache.user(*id)))
        .flatten();
    let named = other.unwrap_or(&m.message.author);
    let (before, after) = if other.is_some() { ("You accepted", "'s friend request.".to_string()) } else { (line.before, line.after.clone()) };
    // Someone else's call we weren't in.
    let missed = line.kind == rich_view::SystemKind::Call
        && me != Some(m.message.author.id)
        && m.message.call.as_ref().is_some_and(|call| !me.is_some_and(|me| call.participants.contains(&me)));
    let (before, after) = if missed {
        ("You missed a call from", rich_view::call_lasted(&m.message).map_or_else(|| ".".to_string(), |lasted| format!(" that lasted {lasted}.")))
    } else {
        (before, after)
    };
    // The message it points at, to jump to.
    let pinned = (line.kind == rich_view::SystemKind::Pin).then(|| m.message.message_reference.as_ref().and_then(|r| r.message_id)).flatten();
    let by = byline(cache, guild, named);
    let time = nav::format_message_time(m.message.id.timestamp_ms(), &chrono::Local::now());
    let muted = theme.muted_foreground;
    let name_spot = crate::markdown_view::NameSpot::default();
    let (icon, tint) = match line.kind {
        rich_view::SystemKind::Added => (Icon::new(IconName::ArrowRight), Hsla::from(gpui_kit::rgb(0x23a55a))),
        rich_view::SystemKind::Left => (Icon::new(IconName::ArrowLeft), Hsla::from(gpui_kit::rgb(0xf23f43))),
        rich_view::SystemKind::Renamed => (Icon::new(IconName::Pencil), muted),
        rich_view::SystemKind::Join => (Icon::new(IconName::ArrowRight), Hsla::from(gpui_kit::rgb(0x23a55a))),
        rich_view::SystemKind::Boost => (Icon::default().path("icons/boost.svg"), Hsla::from(gpui_kit::rgb(0xff73fa))),
        rich_view::SystemKind::Pin => (Icon::default().path("icons/toolbar-pin.svg"), muted),
        rich_view::SystemKind::FriendAccepted => (Icon::default().path("icons/user-check.svg"), Hsla::from(gpui_kit::rgb(0x23a55a))),
        rich_view::SystemKind::Thread => (Icon::new(IconName::MessageSquareText), muted),
        rich_view::SystemKind::Call => (Icon::default().path("icons/phone.svg"), Hsla::from(gpui_kit::rgb(if missed { 0xf23f43 } else { 0x23a55a }))),
    };
    let row = h_flex()
        .id(("message", ix))
        .on_mouse_down(gpui_kit::MouseButton::Right, on_row_menu(page, m.message.id))
        .w_full()
        .px_4()
        .py(u(2.))
        .gap_4()
        .items_center()
        .child(div().w(u(AVATAR)).flex_shrink_0().flex().justify_center().text_color(tint).child(icon.size(u(20.))))
        .child(
            h_flex()
                .flex_1()
                .min_w_0()
                .flex_wrap()
                .items_center()
                .text_color(muted)
                .when(!before.is_empty(), |d| d.child(format!("{}\u{a0}", before.trim_end())))
                .child(crate::markdown_view::spotted(&name_spot,
                    div()
                        .id(("system-name", ix))
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .on_click({
                            let (open, who, target) = (shared.open_user.clone(), named.id, name_spot.clone());
                            move |event, window, cx| open(who, crate::markdown_view::card_at(&target, event.position()), window, cx)
                        })
                        .font_weight(FontWeight::MEDIUM)
                        // In a DM there's no role to color it, and these lines don't carry name styles.
                        .text_color(if guild.is_some() { by.color.unwrap_or(theme.foreground) } else { theme.foreground })
                        .when(guild.is_some(), |d| d.when_some(by.font, |d, font| d.font_family(font)))
                        .child(by.name.clone()),
                ))
                .map(|d| match pinned {
                    Some(target) => {
                        let jump = shared.jump.clone();
                        d.child("\u{a0}pinned\u{a0}")
                            .child(
                                div()
                                    .id(("pinned-link", ix))
                                    .text_color(theme.foreground)
                                    .cursor_pointer()
                                    .hover(|d| d.underline())
                                    .on_click(move |_, _, cx| jump(target, cx))
                                    .child("a message"),
                            )
                            .child("\u{a0}to this channel.\u{a0}")
                    }
                    None => d.child(format!("{}\u{a0}", after.replacen(' ', "\u{a0}", 1))),
                })
                .child(div().text_size(u(12.)).child(time)),
        );
    if margin {
        div().w_full().pt(u(17.)).child(row).into_any_element()
    } else {
        row.into_any_element()
    }
}

/// "AutoMod has blocked a message in #chat", with what was said and why.
#[allow(clippy::too_many_arguments)]
fn automod_row(
    ix: usize,
    m: &ChatMessage,
    alert: &rich_view::AutomodAlert,
    margin: bool,
    guild: Option<Id>,
    cache: &Cache,
    page: &WeakEntity<ChannelPage>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let author = &m.message.author;
    let by = byline(cache, guild, author);
    let time = nav::format_message_time(m.message.id.timestamp_ms(), &chrono::Local::now());
    let muted = theme.muted_foreground;
    let blurple = Hsla::from(gpui_kit::rgb(0x5865f2));
    let (where_, known) = match alert.channel.and_then(|c| cache.channel(c)) {
        Some(channel) => (format!("#{}", nav::channel_label(cache, channel)), true),
        None => ("unknown-channel".to_string(), false),
    };
    let amber = Hsla::from(gpui_kit::rgb(0xf0b232));
    let lit: Vec<_> = alert
        .matched
        .as_deref()
        .map(|words| rich_view::find_ignoring_case(&alert.text, words))
        .unwrap_or_default()
        .into_iter()
        .map(|range| (range, gpui_kit::HighlightStyle { color: Some(amber), background_color: Some(amber.opacity(0.25)), ..Default::default() }))
        .collect();
    let why = [alert.keyword.as_ref().map(|k| format!("Keyword: {k}")), (!alert.rule.is_empty()).then(|| format!("Rule: {}", alert.rule))]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("  \u{2022}  ");
    let row = h_flex()
        .id(("message", ix))
        .on_mouse_down(gpui_kit::MouseButton::Right, on_row_menu(page, m.message.id))
        .w_full()
        .px_4()
        .py(u(2.))
        .gap_4()
        .items_start()
        .child(
            div()
                .size(u(AVATAR))
                .flex_shrink_0()
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(blurple)
                .text_color(gpui_kit::white())
                .child(Icon::default().path("icons/automod.svg").size(u(24.))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .line_height(gpui_kit::relative(1.375))
                .child(
                    h_flex()
                        .flex_wrap()
                        .gap(u(5.))
                        .items_center()
                        .text_color(muted)
                        .child(div().font_weight(FontWeight::MEDIUM).text_color(blurple).child("AutoMod"))
                        .child(
                            div()
                                .h(u(16.))
                                .px(u(5.))
                                .flex()
                                .items_center()
                                .rounded(u(4.))
                                .bg(blurple)
                                .text_color(gpui_kit::white())
                                .text_size(u(10.))
                                .font_weight(FontWeight::BOLD)
                                .child("OFFICIAL"),
                        )
                        .child(if alert.flagged { "has flagged a message in" } else { "has blocked a message in" })
                        .child(div().font_weight(FontWeight::MEDIUM).when(known, |d| d.text_color(theme.foreground)).child(where_))
                        .child(div().text_size(u(12.)).child(time)),
                )
                .child(
                    h_flex()
                        .mt(u(6.))
                        .p(u(12.))
                        .gap(u(12.))
                        .items_start()
                        .w_full()
                        .max_w(u(720.))
                        .rounded(u(8.))
                        .bg(theme.secondary)
                        .child(Avatar::new().border_0().src(cache.avatar_url(guild, author, 64)).name(by.name.clone()).size(u(40.)))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(
                                    div()
                                        .font_weight(FontWeight::MEDIUM)
                                        .text_color(by.color.unwrap_or(theme.foreground))
                                        .when_some(by.font, |d, font| d.font_family(font))
                                        .child(by.name.clone()),
                                )
                                .child(div().child(gpui_kit::StyledText::new(alert.text.clone()).with_highlights(lit)))
                                .when(!why.is_empty(), |d| d.child(div().mt(u(2.)).text_size(u(12.)).text_color(muted).child(why))),
                        ),
                ),
        );
    if margin {
        div().w_full().pt(u(17.)).child(row).into_any_element()
    } else {
        row.into_any_element()
    }
}

/// "…'s poll … has closed.", with who won.
#[allow(clippy::too_many_arguments)]
fn poll_result_row(
    ix: usize,
    m: &ChatMessage,
    outcome: &rich_view::PollOutcome,
    margin: bool,
    guild: Option<Id>,
    cache: &Cache,
    shared: &Shared,
    page: &WeakEntity<ChannelPage>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let by = byline(cache, guild, &m.message.author);
    let time = nav::format_message_time(m.message.id.timestamp_ms(), &chrono::Local::now());
    let muted = theme.muted_foreground;
    let row = h_flex()
        .id(("message", ix))
        .on_mouse_down(gpui_kit::MouseButton::Right, on_row_menu(page, m.message.id))
        .w_full()
        .px_4()
        .py(u(2.))
        .gap_4()
        .items_start()
        .child(
            div()
                .w(u(AVATAR))
                .flex_shrink_0()
                .flex()
                .justify_center()
                .pt(u(2.))
                .text_color(muted)
                .child(Icon::default().path("icons/poll.svg").size(u(20.))),
        )
        .child(
            v_flex()
                .flex_1()
                .min_w_0()
                .line_height(gpui_kit::relative(1.375))
                .child(
                    h_flex()
                        .flex_wrap()
                        .items_center()
                        .text_color(muted)
                        .child(
                            div()
                                .font_weight(FontWeight::MEDIUM)
                                .text_color(by.color.unwrap_or(theme.foreground))
                                .when_some(by.font, |d, font| d.font_family(font))
                                .child(by.name.clone()),
                        )
                        .child("\u{2019}s poll\u{a0}")
                        .child(div().font_weight(FontWeight::SEMIBOLD).text_color(theme.foreground).child(outcome.question.clone()))
                        .child("\u{a0}has closed.\u{a0}")
                        .child(div().text_size(u(12.)).child(time)),
                )
                .child(rich_view::poll_outcome_card(&Env { shared, cache, message: &m.message, guild }, outcome, cx)),
        );
    if margin {
        div().w_full().pt(u(17.)).child(row).into_any_element()
    } else {
        row.into_any_element()
    }
}

/// Markdown text, embeds, attachments and the rest of a message's body.
fn message_content(message: &Message, guild: Option<Id>, cache: &Cache, shared: &Shared, cx: &App) -> AnyElement {
    rich_view::body(&Env { shared, cache, message, guild }, cx)
}

/// A thin rule with the date centred, between messages of different days.
/// The height of the bar across the top of a channel.
const TOP_BAR: f32 = 49.;

/// The red line above the first message that arrived unread, "NEW" on its right.
fn new_divider(cx: &App) -> AnyElement {
    let red = cx.theme().danger;
    h_flex()
        .w_full()
        .pl_4()
        .my(u(4.))
        .items_center()
        .child(div().h(u(1.)).flex_1().bg(red))
        .child(
            div()
                .h(u(13.))
                .px(u(4.))
                .flex()
                .items_center()
                .rounded_l(u(4.))
                .bg(red)
                .text_color(gpui_kit::white())
                .text_size(u(10.))
                .font_weight(FontWeight::BOLD)
                .child("NEW"),
        )
        .into_any_element()
}

fn day_divider(ms: u64, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let rule = || div().h(u(1.)).flex_1().bg(theme.border);
    h_flex()
        .w_full()
        .px_4()
        .mt(u(16.))
        .mb(u(8.))
        .gap_2()
        .items_center()
        .child(rule())
        .child(
            div()
                .text_size(u(12.))
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme.muted_foreground)
                .child(chat_format::day_label(ms, &chrono::Local)),
        )
        .child(rule())
        .into_any_element()
}

/// Discord's toolbar icons: 24px glyphs in 32px hit boxes, muted until hovered.
fn tool_button(id: &'static str, icon: impl Into<IconOrPath>, tip: &'static str, cx: &App) -> gpui_kit::Stateful<gpui_kit::Div> {
    let theme = cx.theme();
    let (muted, bright) = (theme.muted_foreground, theme.foreground);
    div()
        .id(id)
        .size(u(32.))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .text_color(muted)
        .cursor_pointer()
        .hover(move |d| d.text_color(bright))
        .tooltip(move |window, cx| gpui_kit::component::tooltip::Tooltip::new(tip).build(window, cx))
        // The size the member list's button draws its icon at, so the row matches.
        .child(icon.into().icon().size(u(18.)))
}

/// A toolbar icon: one of the kit's, or a file of Discord's own.
enum IconOrPath {
    Named(IconName),
    Path(&'static str),
}

impl IconOrPath {
    fn icon(self) -> Icon {
        match self {
            IconOrPath::Named(name) => Icon::new(name),
            IconOrPath::Path(path) => Icon::default().path(path),
        }
    }
}

impl From<IconName> for IconOrPath {
    fn from(name: IconName) -> Self {
        IconOrPath::Named(name)
    }
}

impl From<&'static str> for IconOrPath {
    fn from(path: &'static str) -> Self {
        IconOrPath::Path(path)
    }
}

/// Stands in for the message box when the user can't write here.
fn locked_composer(reason: String, cx: &App) -> impl IntoElement {
    let theme = cx.theme();
    h_flex()
        .min_h(u(44.))
        .px_4()
        .gap_2()
        .items_center()
        .rounded(u(8.))
        .bg(theme.popover)
        .text_color(theme.muted_foreground)
        .cursor_not_allowed()
        .child(Icon::default().path("icons/lock.svg").size(u(16.)))
        .child(div().flex_1().min_w_0().truncate().child(reason))
}

impl Render for ChannelPage {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        crate::ui::note_chat_view(cx.entity_id());
        crate::perf::count("page");
        crate::perf::tick();
        if let Some((user, name)) = crate::links::take_mention() {
            self.mention(user, name, window, cx);
        }
        let screens = self.ctx.services.settings.read(cx).get().image_screens;
        crate::anim::note_view(self.list.logical_scroll_top().item_ix, screens, window, cx);
        if let Some((user, position, always)) = crate::links::take_profile(cx) {
            if always {
                self.last_closed = None;
                self.show_profile(user, position, false, window, cx);
            } else if !self.toggled_shut(user, window, cx) {
                self.show_profile(user, position, false, window, cx);
            }
        }
        if let Some(target) = self.channel.and_then(|c| crate::links::take_jump(c, cx)) {
            self.jump_target = Some((target, 0));
            self.seek_jump_target(cx);
        }
        if self.unseen > 0 && self.list.is_following_tail() {
            self.unseen = 0;
        }
        self.close_banner_once_read(cx);
        self.note_unasked_move();
        if let Some((ix, to_bottom)) = self.glide_to {
            let view = self.list.viewport_bounds();
            let row = self.row_bounds(ix);
            if to_bottom {
                // Lands with a little room under it.
                let room = f32::from(window.rem_size()) * 5.;
                match row {
                    Some(row) => {
                        self.glide_to = None;
                        self.scroller.glide_by(f32::from(row.bottom() - view.bottom()) + room);
                    }
                    None => window.request_animation_frame(),
                }
            } else {
                match self.row_distance(ix) {
                    Some(distance) => {
                        self.glide_to = None;
                        let height = row.map(|r| f32::from(r.size.height));
                        if let Some(by) = reveal_amount(distance, height, f32::from(view.size.height)) {
                            // Measured from where it is now: any glide under way is replaced,
                            // or a second click would add its whole distance again.
                            self.scroller.stop();
                            self.scroller.glide_by(by);
                            self.jump_settle = Some((ix, 3));
                        }
                    }
                    // Not laid out yet: ask again on the next frame.
                    None => window.request_animation_frame(),
                }
            }
        }
        // Rows above can change height as they're measured on the way: check it
        // really landed in full, and nudge it if not.
        if let Some((ix, tries)) = self.jump_settle {
            if self.scroller.is_gliding() {
                window.request_animation_frame();
            } else {
                self.jump_settle = None;
                let view = self.list.viewport_bounds();
                let height = self.row_bounds(ix).map(|r| f32::from(r.size.height));
                if tries > 0
                    && let Some(by) = self.row_distance(ix).and_then(|d| reveal_amount(d, height, f32::from(view.size.height)))
                {
                    self.scroller.stop();
                    self.scroller.glide_by(by);
                    self.jump_settle = Some((ix, tries - 1));
                    window.request_animation_frame();
                }
            }
        }
        if let Some(bench) = &mut self.bench {
            bench.tick(&self.list, &self.scroller, window);
            cx.notify();
        }
        // Like Discord, the member list steps aside when the chat would get
        // narrower than this (in Discord's 100%-zoom pixels).
        const CHAT_MIN: f32 = 480.;
        let width = f32::from(window.viewport_size().width) / f32::from(window.rem_size()) * 16.;
        let layout = self.ctx.services.settings.read(cx).get();
        let (column_width, members_width) = (layout.column_width, layout.members_width);
        let members_fit = width - 72. - column_width - members_width >= CHAT_MIN
            || std::env::var_os("RUSTCORD_FORCE_MEMBERS").is_some();
        let store = self.ctx.services.store.read(cx);
        let cache = store.cache();

        let Some(channel_id) = self.channel else {
            let text = if cache.me().is_none() { "Connecting to Discord…" } else { "Select a channel" };
            return self.notice(text, cx);
        };
        let Some(channel) = cache.channel(channel_id) else {
            let text = if cache.me().is_none() { "Connecting to Discord…" } else { "This channel doesn't exist, or you can't see it." };
            return self.notice(text, cx);
        };
        if channel.kind == ChannelKind::Forum {
            return self.notice("Forum channels aren't supported yet.", cx);
        }
        if !has_chat(channel.kind) {
            return self.notice("This channel has no chat.", cx);
        }
        let label = nav::channel_label(cache, channel);
        let history = store.history(channel_id).cloned();
        let empty = cache.messages(channel_id).is_none_or(|m| m.is_empty());

        // Either the server said so, or our roles don't let us see the channel.
        let forbidden = history == Some(History::Forbidden) || !cache.can_view(channel_id);
        let messages: AnyElement = match history {
            _ if forbidden => self.notice("You can't view this channel.", cx),
            Some(History::Failed(error)) => {
                let store = self.ctx.services.store.clone();
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(div().text_color(cx.theme().muted_foreground).child(format!("Couldn't load messages: {error}")))
                    .child(
                        Button::new("reload-history")
                            .small()
                            .label("Retry")
                            .on_click(move |_, _, cx| store.update(cx, |store, cx| store.reload_channel(channel_id, cx))),
                    )
                    .into_any_element()
            }
            Some(History::Loading) if empty => skeleton_messages(cx),
            _ => {
                let store = self.ctx.services.store.clone();
                let page = cx.entity().downgrade();
                let guild = self.guild;
                let prefs = self.ctx.services.settings.read(cx).get();
                let (compact, chat_text, smooth) = (prefs.compact, prefs.chat_text, prefs.smooth_scrolling);
                let shared = Shared {
                    poll_ui: self.poll_ui.clone(),
                    in_card: false,
                    inert: false,
                    emoji_cap: None,
                    remeasure: {
                        let page = page.clone();
                        Rc::new(move |message, cx| {
                            let _ = page.update(cx, |page, cx| {
                                if let Some(ix) = page.rendered.iter().position(|r| *r == message) {
                                    page.list.remeasure_items(ix..ix + 1);
                                    cx.notify();
                                }
                            });
                        })
                    },
                    vote: {
                        let store = store.clone();
                        Rc::new(move |message, answers, cx| {
                            store.update(cx, |store, cx| store.vote_poll(channel_id, message, answers, cx));
                        })
                    },
                    revealed: self.revealed.clone(),
                    reveal: {
                        let page = page.clone();
                        Rc::new(move |message, group, cx| {
                            let _ = page.update(cx, |page, cx| {
                                page.revealed.borrow_mut().insert((message, group));
                                cx.notify();
                            });
                        })
                    },
                    jump: {
                        let page = page.clone();
                        Rc::new(move |id, cx| {
                            let _ = page.update(cx, |page, cx| page.jump_to(id, cx));
                        })
                    },
                    react: {
                        let store = store.clone();
                        Rc::new(move |message, emoji, on, cx| {
                            store.update(cx, |store, cx| store.set_reaction(channel_id, message, emoji, on, cx));
                        })
                    },
                    open_user: {
                        let page = page.clone();
                        Rc::new(move |user, position, window, cx| {
                            let _ = page.update(cx, |page, cx| page.open_profile(user, position, window, cx));
                        })
                    },
                    open_user_menu: {
                        let page = page.clone();
                        Rc::new(move |user, position, cx| {
                            let _ = page.update(cx, |page, cx| {
                                let name = {
                                    let cache = page.ctx.services.store.read(cx).cache();
                                    cache.user(user).map_or_else(|| "this person".to_string(), |u| cache.display_name(page.guild, u).to_string())
                                };
                                page.user_menu(user, name, position, cx);
                            });
                        })
                    },
                    who_reacted: {
                        let (core, store) = (self.ctx.services.core.clone(), self.ctx.services.store.clone());
                        Rc::new(move |pill, cx| match pill {
                            Some((message, emoji, count)) => {
                                crate::reaction_names::hover(channel_id, message, emoji, count, guild, core.clone(), store.clone(), cx)
                            }
                            None => crate::reaction_names::unhover(cx),
                        })
                    },
                    text_size: chat_text as f32,
                    parsed: self.parsed.clone(),
                };
                // A thread says who started it, once we know them.
                let starter = (channel.kind == cord_core::model::ChannelKind::Thread)
                    .then(|| {
                        let cache = self.ctx.services.store.read(cx).cache();
                        let user = cache.user(channel.owner_id?)?;
                        Some(cache.display_name(channel.guild_id, user).to_string())
                    })
                    .flatten();
                let beginning = match starter {
                    Some(name) => format!("Started by {name}"),
                    None => format!("This is the beginning of {}{label}.", if channel.guild_id.is_some() { "#" } else { "@" }),
                };
                let highlight = self.highlight.clone();
                let hovered_row = self.hovered_row.clone();
                let replying_to = self.replying.as_ref().map(|(id, _, _)| *id);
                let hovered_tools = self.hovered_tools.clone();
                let row_store = self.ctx.services.store.clone();
                let build_row = move |ix: usize, _window: &mut Window, cx: &mut App| -> AnyElement {
                    crate::perf::count("row");
                    // Start this row's GIFs before drawing it, so they play on its first frame.
                    let gif_message = ix.checked_sub(1).and_then(|i| {
                        let messages = store.read(cx).cache().messages(channel_id)?;
                        let m = messages.get(i)?;
                        crate::gifs::gif_videos(&m.message).next().is_some().then(|| m.message.clone())
                    });
                    if let Some(message) = gif_message {
                        crate::gifs::warm(&message, cx);
                    }
                    let sticker_message = ix.checked_sub(1).and_then(|i| {
                        let store = store.read(cx);
                        let m = store.cache().messages(channel_id)?.get(i)?;
                        m.message.sticker_items.iter().any(|s| crate::stickers::is_rendered(s.format_type)).then(|| (m.message.clone(), store.core().clone()))
                    });
                    if let Some((message, core)) = sticker_message {
                        crate::stickers::warm(&message, &core, cx);
                    }
                    let invite_message = ix.checked_sub(1).and_then(|i| {
                        let store = store.read(cx);
                        let m = store.cache().messages(channel_id)?.get(i)?;
                        let c = &m.message.content;
                        (c.contains("discord.gg/") || c.contains("/invite/")).then_some(())?;
                        Some((m.message.clone(), store.core().clone()))
                    });
                    if let Some((message, core)) = invite_message {
                        crate::invites::warm(&message, &core, cx);
                    }
                    // A forward's GIFs, stickers and invites are started the same way.
                    let forwarded = ix.checked_sub(1).and_then(|i| {
                        let store = store.read(cx);
                        let m = store.cache().messages(channel_id)?.get(i)?;
                        Some((m.message.forwarded()?, store.core().clone()))
                    });
                    if let Some((inner, core)) = forwarded {
                        crate::gifs::warm(&inner, cx);
                        crate::stickers::warm(&inner, &core, cx);
                        crate::invites::warm(&inner, &core, cx);
                    }
                    let store = store.read(cx);
                    let cache = store.cache();
                    let Some(messages) = cache.messages(channel_id) else { return div().into_any_element() };
                    if ix == 0 {
                        let muted = cx.theme().muted_foreground;
                        let note = |text: String| div().w_full().py_4().text_center().text_sm().text_color(muted).child(text);
                        return if store.is_loading_older(channel_id) {
                            note("Loading older messages…".into()).into_any_element()
                        } else if !messages.has_older {
                            note(beginning.clone()).into_any_element()
                        } else if cache.scrollback_full(channel_id) {
                            note("That's as far back as rustcord keeps. Scroll down to return.".into()).into_any_element()
                        } else {
                            // Older messages exist: fetch them now this row is about to show.
                            let page = page.clone();
                            cx.defer(move |cx| {
                                let _ = page.update(cx, |page, cx| page.reached_top(cx));
                            });
                            div().h(u(24.)).into_any_element()
                        };
                    }
                    let i = ix - 1;
                    let Some(m) = messages.get(i) else { return div().into_any_element() };
                    let prev = i.checked_sub(1).and_then(|p| messages.get(p)).map(|p| &p.message);
                    let stacked = nav::continues_group(prev, &m.message, &chrono::Local)
                        && !rich_view::is_reply(&m.message)
                        && rich_view::system_line(&m.message).is_none()
                        && prev.is_none_or(|p| p.kind != rich_view::KIND_POLL_RESULT && rich_view::automod_alert(p).is_none() && rich_view::system_line(p).is_none());
                    let new_day = chat_format::starts_new_day(prev, &m.message, &chrono::Local);
                    let system = rich_view::system_line(&m.message);
                    let notice = system.is_some() || rich_view::poll_outcome(&m.message).is_some() || rich_view::automod_alert(&m.message).is_some();
                    // Rows keep their gap to the group above as padding: the jump glow leaves it out.
                    let gap_above = !new_day && (notice || (!compact && !stacked));
                    let row = if let Some(line) = &system {
                        system_row(i, m, line, !new_day, guild, cache, &shared, &page, cx)
                    } else if let Some(outcome) = rich_view::poll_outcome(&m.message) {
                        poll_result_row(i, m, &outcome, !new_day, guild, cache, &shared, &page, cx)
                    } else if let Some(alert) = rich_view::automod_alert(&m.message) {
                        automod_row(i, m, &alert, !new_day, guild, cache, &page, cx)
                    } else if compact {
                        compact_row(i, m, guild, cache, &shared, &page, replying_to, cx)
                    } else {
                        message_row(i, m, stacked, !new_day, guild, cache, &shared, &page, &hovered_row, &hovered_tools, replying_to, cx)
                    };
                    // Only the message lights up, not the date bar above it.
                    let row = if let Some((_, jump)) = highlight.get().filter(|(id, _)| *id == m.message.id) {
                        // The same look as a message being replied to, behind the row, fading.
                        div()
                            .relative()
                            .w_full()
                            .child(
                                div()
                                    .absolute()
                                    .top(u(if gap_above { 17. } else { 0. }))
                                    .bottom_0()
                                    .left_0()
                                    .right_0()
                                    .bg(reply_tint(false))
                                    .child(reply_bar())
                                    .with_animation(
                                        ("jump-glow", jump),
                                        gpui_kit::Animation::new(std::time::Duration::from_millis(1900)),
                                        // Holds a moment so it can be found, then fades.
                                        |d, t| d.opacity(if t < 0.5 { 1. } else { 1. - crate::transition::ease((t - 0.5) * 2.) }),
                                    ),
                            )
                            .child(row)
                            .into_any_element()
                    } else {
                        row
                    };
                    let first_new = store.new_after().is_some_and(|read| {
                        m.message.id > read && prev.is_none_or(|p| p.id <= read) && Some(m.message.author.id) != cache.me().map(|u| u.id)
                    });
                    let row = if new_day || first_new {
                        v_flex()
                            .w_full()
                            .when(new_day, |d| d.child(day_divider(m.message.id.timestamp_ms(), cx)))
                            .when(first_new, |d| d.child(new_divider(cx)))
                            .child(row)
                            .into_any_element()
                    } else {
                        row
                    };
                    // Discord keeps a 24px gap between the newest message and the composer.
                    if i + 1 == messages.len() {
                        v_flex().w_full().pb(u(24.)).child(row).into_any_element()
                    } else {
                        row
                    }
                };
                let list = list(self.list.clone(), move |ix, window, cx| {
                    let element = build_row(ix, window, cx);
                    let id = ix.checked_sub(1).and_then(|i| row_store.read(cx).cache().messages(channel_id)?.get(i).map(|m| m.message.id));
                    match id {
                        Some(id) => crate::anim::row(id, element).into_any_element(),
                        None => element,
                    }
                });
                div()
                    .relative()
                    .size_full()
                    .text_size(u(chat_text as f32))
                    .line_height(u(chat_text as f32 * 22. / 16.))
                    .when(smooth, |this| this.child(self.scroller.layer()))
                    .overflow_hidden()
                    .child(list.size_full())
                    .when_some(typing_line(&self.ctx.services.store, channel_id, self.guild, cx), |this, line| this.child(line))
                    .when_some(self.new_messages_bar(channel_id, cx), |this, bar| this.child(bar))
                    .map(|this| {
                        let unread = self.unread_below(channel_id, cx);
                        // Read from the list each frame: the smooth scroller moves it
                        // without firing the list's scroll events.
                        let older = !self.list.is_following_tail() && self.list.logical_scroll_top().item_ix + JUMP_PILL_AFTER < self.rendered.len();
                        // The jump bar follows the scroll from outside the list, so the list
                        // moves as a layer only while the bar's place in that stays the same.
                        {
                            let (list, rows) = (self.list.clone(), self.rendered.len());
                            let extra = unread.is_some() || self.unseen > 0;
                            let wants_bar = move |list: &gpui_kit::ListState, top: f32| {
                                const NEARLY: f32 = 200.;
                                let following = list.is_following_tail();
                                let near = following || f32::from(list.max_offset_for_scrollbar().y) - top <= NEARLY;
                                let older = !following && list.logical_scroll_top().item_ix + JUMP_PILL_AFTER < rows;
                                !near && (extra || older)
                            };
                            let now = wants_bar(&list, f32::from((-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y)));
                            self.scroller.outside_stays_right(move |top| wants_bar(&list, top) == now);
                        }
                        if !self.near_newest() && (unread.is_some() || older || self.unseen > 0) {
                            this.child(self.jump_bar(unread, cx))
                        } else {
                            this
                        }
                    })
                    .into_any_element()
            }
        };

        let members_on = self.ctx.services.settings.read(cx).get().show_members;
        let group_list = (members_on && members_fit && channel.kind == ChannelKind::GroupDm).then(|| self.group_members(channel, members_width, cx));
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let icon = match channel.kind {
            ChannelKind::Voice | ChannelKind::Stage => IconName::Volume2,
            _ if channel.guild_id.is_none() => IconName::AtSign,
            _ => IconName::Hash,
        };
        // The header shows the topic's first line as plain text, its markdown
        // marks dropped (the full styling would need a whole message to render in).
        let topic = channel.topic.as_deref().map(plain_topic);
        let has_topic = topic.is_some();
        let search = match channel.guild_id.and_then(|g| cache.guild(g)) {
            Some(g) => format!("Search {}", g.name),
            None => "Search".to_string(),
        };
        // Measured in Discord: 49px with 8px padding (16px on the left).
        let header = h_flex()
            .h(u(49.))
            .pl(u(16.))
            .pr(u(8.))
            .gap(u(8.))
            .flex_shrink_0()
            .items_center()
            .border_b_1()
            .border_color(border)
            .child(Icon::new(icon).size(u(24.)).text_color(muted))
            .child(div().flex_shrink_0().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(label.clone()))
            .when_some(topic, |this, topic| {
                this.child(div().flex_shrink_0().mx(u(4.)).size(u(4.)).rounded_full().bg(muted.opacity(0.6)))
                    .child(div().flex_1().min_w_0().truncate().text_size(u(14.)).text_color(muted).child(topic))
            })
            // With a topic, it's what fills the row: a spacer would take half its room.
            .when(!has_topic, |this| this.child(div().flex_1()))
            .child(tool_button("threads", "icons/toolbar-threads.svg", "Threads", cx))
            .child(tool_button("bell", "icons/toolbar-bell.svg", "Notification Settings", cx))
            .child(tool_button("pin", "icons/toolbar-pin.svg", "Pinned Messages", cx))
            .when(self.guild.is_some() || channel.kind == ChannelKind::GroupDm, |this| {
                this.child(
                    Button::new("toggle-members")
                        .ghost()
                        .small()
                        .icon(Icon::default().path("icons/toolbar-members.svg").size(u(24.)))
                        .selected(members_on)
                        .tooltip(if members_on { "Hide Member List" } else { "Show Member List" })
                        .on_click(cx.listener(|this, _, _, cx| this.toggle_members(cx))),
                )
            })
            .child(
                h_flex()
                    .id("search-box")
                    .cursor_default()
                    .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Search — arrives later").build(window, cx))
                    .w(u(240.))
                    .h(u(32.))
                    .px_2()
                    .ml_2()
                    .items_center()
                    .justify_between()
                    .rounded(u(4.))
                    .bg(theme.popover)
                    .text_size(u(14.))
                    .text_color(muted)
                    .child(div().min_w_0().truncate().child(search))
                    .child(Icon::new(IconName::Search).size(u(16.))),
            );
        // A voice channel: who's in it on the left, its chat in a panel beside
        // them that opens from the channel list and drags wider or narrower.
        let voice = matches!(channel.kind, ChannelKind::Voice | ChannelKind::Stage);
        let open = self.vc_chat_open;
        // Too narrow for every icon: the less-used ones step aside, as Discord's do.
        let chat_width = width - 72. - column_width - if members_fit && self.members.is_some() { members_width } else { 0. };
        let compact_tools = voice || chat_width < 640.;
        // However it was dragged, never so wide the tiles lose their room.
        let width = self.vc_chat_width.min(vc_chat_max(window, column_width));
        let stage = voice.then(|| self.voice_stage(channel_id, &label, window, cx));
        let edge = (voice && open).then(|| {
            let theme = cx.theme();
            let (line, grab) = (theme.border, theme.foreground.opacity(0.25));
            div()
                .id("vc-chat-edge")
                .w(u(5.))
                .h_full()
                .flex_shrink_0()
                .flex()
                .justify_center()
                .cursor_col_resize()
                .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                    this.resizing = true;
                    cx.stop_propagation();
                    cx.notify();
                }))
                .child(div().w(u(1.)).h_full().bg(if self.resizing { grab } else { line }))
        });
        let chat_header = voice.then(|| {
            h_flex()
                .h(u(49.))
                .pl(u(16.))
                .pr(u(8.))
                .gap(u(8.))
                .flex_shrink_0()
                .items_center()
                .border_b_1()
                .border_color(border)
                .child(Icon::new(IconName::MessageCircle).size(u(20.)).text_color(muted))
                .child(div().flex_1().min_w_0().truncate().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(label.clone()))
                .child(tool_button("close-vc-chat", IconName::Close, "Close Chat", cx).on_click(cx.listener(|this, _, _, cx| {
                    this.vc_chat_open = false;
                    cx.notify();
                })))
        });
        // In a voice channel the chat has its own header; elsewhere the bar spans the page.
        let top_header = chat_header.is_none().then_some(header);
        h_flex()
            .relative()
            .size_full()
            .when(self.resizing || self.resizing_members, |d| d.cursor_col_resize())
            .on_mouse_move(cx.listener(|this, event: &gpui_kit::MouseMoveEvent, window, cx| {
                if !this.resizing && !this.resizing_members {
                    return;
                }
                // From the pointer to the window's right edge, in the UI's own pixels.
                let scale = f32::from(window.rem_size()) / 16.;
                let wanted = (f32::from(window.viewport_size().width) - f32::from(event.position.x)) / scale;
                if this.resizing_members {
                    this.ctx.services.settings.update(cx, |settings, cx| settings.update_unsaved(cx, |s| s.members_width = wanted));
                    return;
                }
                let column = this.ctx.services.settings.read(cx).get().column_width;
                let width = wanted.clamp(VC_CHAT_MIN, vc_chat_max(window, column));
                this.vc_chat_width = width;
                cx.set_global(VoiceChatWidth(width));
                cx.notify();
            }))
            .on_mouse_up(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                if this.resizing_members {
                    this.resizing_members = false;
                    this.ctx.services.settings.read(cx).save();
                }
                if this.resizing {
                    this.resizing = false;
                    cx.notify();
                }
            }))
            .when(top_header.is_some(), |d| d.relative().pt(u(TOP_BAR)))
            .children(stage)
            .children(edge)
            .child(
                v_flex()
                    .map(|d| match (voice, open) {
                        (false, _) => d.flex_1().min_w_0(),
                        (true, true) => d.w(u(width)).flex_shrink_0(),
                        (true, false) => d.w_0().overflow_hidden(),
                    })
                    .h_full()
                    .children(chat_header)
                    .child(div().flex_1().min_h_0().child(messages))
                    .when(!forbidden, |this| {
                        let composer = match nav::composer_lock(cache, channel) {
                            Some(reason) => locked_composer(reason, cx).into_any_element(),
                            None => {
                                let theme = cx.theme();
                                let (muted, popover, border) = (theme.muted_foreground, theme.popover, theme.border);
                                let filled = !self.input.read(cx).value().trim().is_empty() || !self.pending.is_empty();
                                let attached = self.replying.is_some() || self.editing.is_some() || !self.pending.is_empty();
                                // Measured in Discord: 58px tall with a hairline border, inset 8px.
                                h_flex()
                                    .min_h(u(58.))
                                    .overflow_hidden()
                                    .px_2()
                                    .gap_1()
                                    // As in Discord: the icons stay on the first line as text grows.
                                    .items_start()
                                    // Joined to what's above it, as one outlined box.
                                    .map(|d| if attached { d.rounded_b(u(8.)) } else { d.rounded(u(8.)) })
                                    .border_1()
                                    .border_color(border)
                                    .bg(popover)
                                    .child(h_flex().h(u(56.)).flex_shrink_0().items_center().pl(u(8.)).child(tool_button("upload", IconName::Plus, "Upload a File", cx).on_click(cx.listener(|this, _, _, cx| this.pick_files(cx)))))
                                    // However it's asked for (Ctrl+V, the right click menu), a picture or files come here.
                                    .child(div().capture_action(cx.listener(|this, _: &gpui_kit::component::input::Paste, _, cx| this.paste_files(cx))).flex_1().min_w(u(160.)).min_h(u(56.)).flex().items_center().text_size(u(16.)).child(Textarea::new(&self.input).appearance(false).large().token(composer_token)))
                                    .child(h_flex().h(u(56.)).flex_shrink_0().items_center().gap_1().when(!compact_tools, |d| d.child(tool_button("translate", IconName::Languages, "Translate", cx))
                                    .child(tool_button("present", IconName::Gift, "Gifts — arrives with the pickers", cx))
                                    .child(
                                        div()
                                            .id("animated")
                                            .tooltip(|window, cx| {
                                                gpui_kit::component::tooltip::Tooltip::new("GIFs — arrives with the pickers").build(window, cx)
                                            })
                                            .mx(u(4.))
                                            .w(u(24.))
                                            .h(u(20.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(u(4.))
                                            .border_2()
                                            .border_color(muted)
                                            .text_color(muted)
                                            .text_size(u(9.))
                                            .font_weight(FontWeight::BOLD)
                                            .child("GIF"),
                                    )
                                    .child(tool_button("sticker", IconName::Sticker, "Stickers — arrives with the pickers", cx)))
                                    .child(tool_button("emoji", IconName::FaceSlightlySmiling, "Emoji — arrives with the pickers", cx))
                                    .child(tool_button("apps", IconName::Blocks, "Apps", cx))
                                    .child(div().mx(u(6.)).w(u(1.)).h(u(24.)).bg(border))
                                    .child(
                                        Button::new("send")
                                            .ghost()
                                            .icon(Icon::new(IconName::SendHorizontal).size(u(24.)))
                                            .opacity(if filled { 1. } else { 0.4 })
                                            .tooltip("Send")
                                            .on_click(cx.listener(|this, _, window, cx| this.send(window, cx))),
                                    ))
                                    .into_any_element()
                            }
                        };
                        this.child(
                            div()
                                .relative()
                                .px(u(8.))
                                .pb(u(8.))
                                .flex_shrink_0()
                                .when_some(self.replying.clone(), |d, (_, author, author_id)| {
                                    let theme = cx.theme();
                                    // Their name as the chat draws it: font, role colour, gradient.
                                    let look = {
                                        let cache = self.ctx.services.store.read(cx).cache();
                                        let user = author_id.and_then(|id| cache.user(id)).cloned();
                                        let colors = author_id.zip(self.guild).map(|(id, g)| cache.member_colors(g, id)).unwrap_or_default();
                                        user.map(|u| crate::badges::name_look(&u, &colors))
                                    };
                                    let (name_font, name_color, name_stops) = look.map_or((None, theme.foreground, Vec::new()), |l| (l.font, l.color.unwrap_or(theme.foreground), l.stops));
                                    let pending = !self.pending.is_empty();
                                    // Discord offers the ping in servers, for someone else's message.
                                    let offer_ping = self.guild.is_some() && !self.replying_to_self;
                                    let ping = self.reply_ping;
                                    let blurple = Hsla::from(gpui_kit::rgb(0x7983f5));
                                    d.child(
                                        h_flex()
                                            .h(u(32.))
                                            .px(u(16.))
                                            .gap(u(4.))
                                            .items_center()
                                            .when(!pending, |d| d.rounded_t(u(8.)))
                                            .border_1()
                                            .border_b_0()
                                            .border_color(theme.border)
                                            .bg(theme.popover)
                                            .text_size(u(14.))
                                            .text_color(theme.muted_foreground)
                                            .child("Replying to")
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .truncate()
                                                    .font_weight(FontWeight::SEMIBOLD)
                                                    .text_color(name_color)
                                                    .when_some(name_font, |d, f| d.font_family(f))
                                                    .child(crate::badges::gradient_text(&author, &name_stops, 0.)),
                                            )
                                            .when(offer_ping, |d| {
                                                d.child(
                                                    h_flex()
                                                        .id("reply-ping")
                                                        .flex_shrink_0()
                                                        .mr(u(10.))
                                                        .gap(u(2.))
                                                        .items_center()
                                                        .font_weight(FontWeight::BOLD)
                                                        .text_size(u(13.))
                                                        .text_color(if ping { blurple } else { theme.muted_foreground })
                                                        .cursor_pointer()
                                                        .tooltip(|window, cx| gpui_kit::component::tooltip::Tooltip::new("Ping in reply").build(window, cx))
                                                        .on_click(cx.listener(|this, _, _, cx| {
                                                            this.reply_ping = !this.reply_ping;
                                                            cx.notify();
                                                        }))
                                                        .child(Icon::new(IconName::AtSign).size(u(16.)))
                                                        .child(if ping { "ON" } else { "OFF" }),
                                                )
                                            })
                                            .child(div().mr(u(10.)).w(u(1.)).h(u(16.)).flex_shrink_0().bg(theme.border))
                                            .child(
                                                div()
                                                    .id("cancel-reply")
                                                    .size(u(18.))
                                                    .flex_shrink_0()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .bg(theme.muted_foreground)
                                                    .text_color(theme.popover)
                                                    .cursor_pointer()
                                                    .hover(|d| d.bg(theme.foreground))
                                                    .on_click(cx.listener(|this, _, _, cx| {
                                                        this.replying = None;
                                                        cx.notify();
                                                    }))
                                                    .child(Icon::new(IconName::Close).size(u(12.))),
                                            ),
                                    )
                                })
                                .when(self.editing.is_some(), |d| {
                                    let theme = cx.theme();
                                    d.child(
                                        h_flex()
                                            .h(u(32.))
                                            .px(u(16.))
                                            .items_center()
                                            .rounded_t(u(8.))
                                            .border_1()
                                            .border_b_0()
                                            .border_color(theme.border)
                                            .bg(theme.popover)
                                            .text_size(u(14.))
                                            .text_color(theme.muted_foreground)
                                            .child(div().flex_1().min_w_0().truncate().child("Editing message: Enter to save, Esc to cancel"))
                                            .child(
                                                div()
                                                    .id("cancel-edit")
                                                    .size(u(18.))
                                                    .flex_shrink_0()
                                                    .flex()
                                                    .items_center()
                                                    .justify_center()
                                                    .rounded_full()
                                                    .bg(theme.muted_foreground)
                                                    .text_color(theme.popover)
                                                    .cursor_pointer()
                                                    .hover(|d| d.bg(theme.foreground))
                                                    .on_click(cx.listener(|page, _, window, cx| {
                                                        page.leave_edit(window, cx);
                                                        cx.notify();
                                                    }))
                                                    .child(Icon::new(IconName::Close).size(u(12.))),
                                            ),
                                    )
                                })
                                .children(self.pending_row(cx))
                                .child(composer)
                                .when_some(self.suggestion_popup(cx), |d, popup| d.child(popup)),
                        )
                    }),
            )
            // Cached: while the chat scrolls the member list reuses its last frame.
            .when(self.members.is_some() && members_fit, |d| {
                d.child(
                    div().relative().w_0().h_full().flex_shrink_0().child(
                        div()
                            .id("members-edge")
                            .absolute()
                            .left(u(-2.))
                            .top_0()
                            .w(u(5.))
                            .h_full()
                            .cursor_col_resize()
                            .on_mouse_down(gpui_kit::MouseButton::Left, cx.listener(|this, _, _, cx| {
                                this.resizing_members = true;
                                cx.stop_propagation();
                            })),
                    ),
                )
            })
            .children(self.members.as_ref().filter(|_| members_fit).map(|(panel, _)| {
                AnyView::from(panel.clone()).cached(
                    gpui_kit::StyleRefinement::default().h_full().w(u(members_width)).flex_shrink_0(),
                )
            }))
            .when_some(group_list, |this, list| this.child(list))
            // The bar runs over the member list too, with the search above it, as Discord's.
            .when_some(top_header, |d, header| d.child(div().absolute().top_0().left_0().right_0().h(u(TOP_BAR)).occlude().child(header)))
            .children(self.profile.as_ref().map(|(card, _)| card.clone()))
            .when_some(self.confirm_delete, |d, message| d.child(self.delete_dialog(message, window, cx)))
            .into_any_element()
    }
}

pub fn page(ctx: RouteContext, window: &mut Window, cx: &mut App) -> AnyView {
    cx.new(|cx| ChannelPage::new(ctx, window, cx)).into()
}

#[test]
fn jumps_move_only_as_far_as_they_must() {
        // In full already: no movement at all.
        assert_eq!(reveal_amount(100., Some(50.), 800.), None);
        assert_eq!(reveal_amount(0., Some(50.), 800.), None);
        // Cut off at the top or the bottom: just enough, with a little margin.
        assert_eq!(reveal_amount(-20., Some(50.), 800.), Some(-36.));
        assert_eq!(reveal_amount(780., Some(50.), 800.), Some(46.));
        // Above the view, not measured yet.
        assert_eq!(reveal_amount(-500., None, 800.), Some(-516.));
        assert_eq!(reveal_amount(10., None, 800.), None);
        // Taller than the view: top aligned, and then settled.
        assert_eq!(reveal_amount(300., Some(2000.), 800.), Some(284.));
        assert_eq!(reveal_amount(16., Some(2000.), 800.), None);
    }

    #[test]
    fn voice_tiles_wrap_instead_of_squeezing() {
        // Too narrow for two: one a row, at full width.
        assert_eq!(voice_tile_size(400., 8), (1, 400., 225.));
        // Room for two, with the gap.
        assert_eq!(voice_tile_size(600., 8), (2, 296., 167.));
        // One person doesn't blow up past the limit.
        assert_eq!(voice_tile_size(2000., 1).1, 640.);
        // Tiles never add up to more than the room.
        for room in 300..1600 {
            let (columns, width, _) = voice_tile_size(room as f32, 8);
            assert!(columns as f32 * width + (columns - 1) as f32 * 8. <= room as f32, "{room}");
        }
    }

    #[test]
    fn finds_pings_in_raw_text() {
        let found = composer_pings("hi <@1> <@!2>, <@&3> é <#4> @everyone <a:x:5> <@nope> @here");
        let pings: Vec<_> = found.iter().map(|(_, p)| p.clone()).collect();
        assert_eq!(pings, [
            ComposerPing::User(Id(1)),
            ComposerPing::User(Id(2)),
            ComposerPing::Role(Id(3)),
            ComposerPing::Channel(Id(4)),
            ComposerPing::Everyone("@everyone"),
            ComposerPing::CustomEmoji { name: "x".into(), id: Id(5), animated: true },
            ComposerPing::Everyone("@here"),
        ]);
        assert_eq!(&"hi <@1>"[found[0].0.clone()], "<@1>");
    }

    #[test]
    fn typed_emoji_and_escapes() {
        let pings = |t: &str| composer_pings(t).into_iter().map(|(_, p)| p).collect::<Vec<_>>();
        assert_eq!(pings("<:cat:12>"), [ComposerPing::CustomEmoji { name: "cat".into(), id: Id(12), animated: false }]);
        assert_eq!(pings("<a:cat_2:12>"), [ComposerPing::CustomEmoji { name: "cat_2".into(), id: Id(12), animated: true }]);
        // A backslash keeps it raw.
        assert!(pings("\\<@1> \\<:cat:12> \\<#3>").is_empty());
        assert!(pings("<:cat:nope> <:: 1>").is_empty());
    }

    #[test]
    fn topics_lose_markdown_marks() {
        assert_eq!(plain_topic("Go to your **User Settings** ⚙ →\nmore"), "Go to your User Settings ⚙ →");
        assert_eq!(plain_topic("## hi __there__"), "hi there");
    }


