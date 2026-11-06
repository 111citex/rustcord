//! The panels that moderation, nicknames and notes go through: Timeout, Kick, Ban,
//! Change Nickname and Add Note. Each is a modal over the whole window that takes every
//! click, wheel turn and key, so nothing reaches the chat behind. Its button is the
//! confirmation: nothing is sent before it is pressed.

use cord_core::Id;
use gpui_kit::assets::IconName;
use gpui_kit::component::input::{InputEvent, Textarea, TextareaState};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{HighlightStyle, InteractiveText, StyledText, AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global, InteractiveElement as _, IntoElement, ParentElement as _, Render, Styled as _, StatefulInteractiveElement as _, Subscription, WeakEntity, Window, anchored, deferred, div, point, px};

use crate::store::Store;
use crate::ui::u;

/// Discord's timeout lengths, in seconds.
pub const DURATIONS: [(&str, u64); 6] = [("60 secs", 60), ("5 mins", 300), ("10 mins", 600), ("1 hour", 3600), ("1 day", 86_400), ("1 week", 604_800)];

/// Why someone is banned. The last one is explained in words.
pub const BAN_REASONS: [&str; 4] = ["Suspicious or spam account", "Compromised or hacked account", "Breaking server rules", "Other"];
const OTHER: usize = 3;

/// How far back a ban deletes what they posted, in seconds.
pub const HISTORY: [(&str, u32); 7] = [
    ("Don't Delete Any", 0),
    ("Previous Hour", 3600),
    ("Previous 6 Hours", 21_600),
    ("Previous 12 Hours", 43_200),
    ("Previous 24 Hours", 86_400),
    ("Previous 3 Days", 259_200),
    ("Previous 7 Days", 604_800),
];
/// Discord's own starting point.
const DEFAULT_HISTORY: usize = 1;

const TIMEOUT_TEXT: &str = "Members who are in timeout are temporarily not allowed to chat or react in text channels. They are also not allowed to connect to voice or Stage channels. ";
const TIMEOUT_HELP: &str = "https://support.discord.com/hc/en-us/articles/4413305239191-Time-Out-FAQ";

/// The reason a ban is sent with, or `None` while one is still missing: a choice, and
/// for "Other" some words of one's own.
pub fn ban_reason(choice: Option<usize>, other: &str) -> Option<String> {
    match choice? {
        OTHER => Some(other.trim().to_string()).filter(|text| !text.is_empty()),
        n => BAN_REASONS.get(n).map(|s| s.to_string()),
    }
}

/// When a timeout of `seconds` from `now` ends, as Discord wants the time.
pub fn timeout_until(now: chrono::DateTime<chrono::Utc>, seconds: u64) -> String {
    (now + chrono::Duration::seconds(seconds as i64)).format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()
}

#[derive(Clone)]
pub enum What {
    Timeout { guild: Id, user: Id, name: String },
    Kick { guild: Id, user: Id, name: String, handle: String },
    Ban { guild: Id, user: Id, handle: String },
    Nickname { guild: Id, user: Id, me: bool, name: String, current: String },
    FriendNickname { user: Id, name: String, current: String },
    Note { user: Id, name: String },
    /// Send a copy of a message to chats picked from `to`.
    Forward { from: Id, message: Id, guild: Option<Id>, to: Vec<Target> },
    Thread { channel: Id, message: Id },
    GroupName { channel: Id, current: String },
}

/// The most chats a message is forwarded to at once, as Discord's.
const FORWARD_MOST: usize = 5;

/// Somewhere to forward to: a chat, or a channel of a server (`place`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Target {
    pub id: Id,
    pub label: String,
    pub place: Option<String>,
}

impl Target {
    /// Whether the label or the server has `query`, ignoring case.
    fn matches(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        query.is_empty() || self.label.to_lowercase().contains(&query) || self.place.as_deref().is_some_and(|p| p.to_lowercase().contains(&query))
    }
}

/// Where a message may be forwarded from `guild`: the open chats first, then the channels
/// of the server it's in, then every other server's. `chats` and `servers` are in the order to show.
pub fn forward_targets(chats: Vec<(Id, String)>, servers: Vec<cord_core::cache::Destination>, guild: Option<Id>) -> Vec<Target> {
    let (here, elsewhere): (Vec<_>, Vec<_>) = servers.into_iter().partition(|d| Some(d.guild) == guild);
    let chats = chats.into_iter().map(|(id, label)| Target { id, label, place: None });
    let channels = here.into_iter().chain(elsewhere).map(|d| Target { id: d.id, label: format!("#{}", d.name), place: Some(d.server) });
    chats.chain(channels).collect()
}

pub struct Panel {
    store: Entity<Store>,
    what: What,
    /// The reason, nickname or note.
    text: Entity<TextareaState>,
    duration: usize,
    ban_choice: Option<usize>,
    /// Chats ticked to forward to.
    chosen: Vec<Id>,
    history: usize,
    dropdown_open: bool,
    /// A note is being fetched: it can't be saved over what's already there.
    loading: bool,
    focus: gpui_kit::FocusHandle,
    _watching: Vec<Subscription>,
}

struct Open(Entity<Panel>);
impl Global for Open {}

pub fn is_open(cx: &App) -> bool {
    cx.has_global::<Open>()
}

pub fn close(cx: &mut App) {
    if is_open(cx) {
        cx.remove_global::<Open>();
        cx.refresh_windows();
    }
}

pub fn open(store: Entity<Store>, what: What, window: &mut Window, cx: &mut App) {
    let panel = cx.new(|cx| {
        let (placeholder, rows, start) = match &what {
            What::Timeout { .. } => ("Enter a reason. This will only be visible in the Audit Log and will not be shown to the member.", 4, String::new()),
            What::Kick { .. } => ("", 3, String::new()),
            What::Ban { .. } => ("Why are they being banned?", 3, String::new()),
            What::Nickname { current, .. } | What::FriendNickname { current, .. } => ("", 1, current.clone()),
            What::Note { .. } => ("Click to add a note", 4, String::new()),
            What::Forward { .. } => ("Search", 1, String::new()),
            What::Thread { .. } => ("New Thread", 1, String::new()),
            What::GroupName { current, .. } => ("Group name", 1, current.clone()),
        };
        let text = cx.new(|cx| {
            let mut state = TextareaState::new(window, cx).auto_grow(rows, rows).placeholder(placeholder);
            if rows == 1 {
                state = state.submit_on_enter(true);
            }
            state
        });
        if !start.is_empty() {
            text.update(cx, |state, cx| state.set_value(start, window, cx));
        }
        let typed = cx.subscribe(&text, |this: &mut Panel, _, event: &InputEvent, cx| {
            if matches!(event, InputEvent::PressEnter { shift: false, .. }) && matches!(this.what, What::Nickname { .. } | What::FriendNickname { .. } | What::Thread { .. } | What::GroupName { .. }) {
                this.submit(cx);
            }
            cx.notify();
        });
        let weak = cx.entity().downgrade();
        let keys = cx.intercept_keystrokes(move |event, window, cx| {
            let _ = weak.update(cx, |panel, cx| {
                let writing = gpui_kit::Focusable::focus_handle(panel.text.read(cx), cx).is_focused(window);
                if event.keystroke.key == "escape" {
                    if panel.dropdown_open {
                        panel.dropdown_open = false;
                        cx.notify();
                    } else {
                        close(cx);
                    }
                    cx.stop_propagation();
                } else if !writing {
                    // Nothing typed here may reach the page behind.
                    cx.stop_propagation();
                }
            });
        });
        let note_for = if let What::Note { user, .. } = &what { Some(*user) } else { None };
        let panel = Panel { store: store.clone(), what: what.clone(), text, duration: 0, ban_choice: None, chosen: Vec::new(), history: DEFAULT_HISTORY, dropdown_open: false, loading: note_for.is_some(), focus: cx.focus_handle(), _watching: vec![typed, keys] };
        if let Some(user) = note_for {
            let core = store.read(cx).core().clone();
            cx.spawn_in(window, async move |this, cx| {
                let note = core.note(user).await.unwrap_or_default();
                let _ = this.update_in(cx, |this, window, cx| {
                    this.loading = false;
                    this.text.update(cx, |state, cx| state.set_value(note, window, cx));
                    cx.notify();
                });
            })
            .detach();
        }
        panel
    });
    // Typing goes to the reason; a panel with nothing to type still takes the keys.
    let writes = !matches!(what, What::Ban { .. });
    if writes {
        panel.read(cx).text.clone().update(cx, |state, cx| state.focus(window, cx));
    } else {
        let focus = panel.read(cx).focus.clone();
        window.focus(&focus, cx);
    }
    cx.set_global(Open(panel));
    cx.refresh_windows();
}

pub fn overlay(window: &Window, cx: &App) -> Option<AnyElement> {
    let panel = cx.try_global::<Open>()?.0.clone();
    let viewport = window.viewport_size();
    let backdrop = div()
        .id("panel-backdrop")
        .occlude()
        .w(viewport.width)
        .h(viewport.height)
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::black().opacity(0.6))
        .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
        .on_click(|_, _, cx| close(cx))
        .child(panel);
    Some(deferred(anchored().position(point(px(0.), px(0.))).child(backdrop)).with_priority(5).into_any_element())
}

impl Panel {
    fn typed(&self, cx: &App) -> String {
        self.text.read(cx).value().to_string()
    }

    /// Whether the button may be pressed yet.
    fn ready(&self, cx: &App) -> bool {
        match &self.what {
            What::Ban { .. } => ban_reason(self.ban_choice, &self.typed(cx)).is_some(),
            What::Note { .. } => !self.loading,
            What::Forward { .. } => !self.chosen.is_empty(),
            What::Thread { .. } => !self.typed(cx).trim().is_empty(),
            _ => true,
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        if !self.ready(cx) {
            return;
        }
        let text = self.typed(cx);
        let reason = text.trim().to_string();
        let what = self.what.clone();
        let (duration, history, choice, chosen) = (self.duration, self.history, self.ban_choice, self.chosen.clone());
        self.store.update(cx, |store, cx| match what {
            What::Timeout { guild, user, .. } => store.timeout_member(guild, user, Some(timeout_until(chrono::Utc::now(), DURATIONS[duration].1)), reason, cx),
            What::Kick { guild, user, .. } => store.kick_member(guild, user, reason, cx),
            What::Ban { guild, user, .. } => {
                if let Some(reason) = ban_reason(choice, &text) {
                    store.ban_member(guild, user, HISTORY[history].1, reason, cx);
                }
            }
            What::Nickname { guild, user, me, .. } => store.set_nickname(guild, user, me, reason, cx),
            What::FriendNickname { user, .. } => store.set_friend_nickname(user, reason, cx),
            What::Note { user, .. } => store.set_note(user, text, cx),
            What::Forward { from, message, guild, .. } => {
                for to in &chosen {
                    store.forward_message(*to, from, message, guild, cx);
                }
            }
            What::Thread { channel, message } => store.create_thread(channel, message, reason, cx),
            What::GroupName { channel, .. } => store.rename_group(channel, reason, cx),
        });
        close(cx);
    }
}

impl Render for Panel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (fg, muted, border) = (theme.foreground, theme.muted_foreground, theme.border);
        let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
        let danger = theme.danger;
        let ready = self.ready(cx);
        let this: WeakEntity<Self> = cx.entity().downgrade();
        let label = |text: &'static str| div().text_size(u(14.)).font_weight(FontWeight::MEDIUM).child(text);
        let box_for = |content: AnyElement| div().w_full().px(u(12.)).py(u(10.)).rounded(u(8.)).border_1().border_color(border).bg(theme.background.opacity(0.5)).text_size(u(15.)).text_color(fg).child(content);
        let text_box = || box_for(Textarea::new(&self.text).into_any_element());

        let (title, body, action, tone): (String, AnyElement, &'static str, gpui_kit::Hsla) = match &self.what {
            What::Timeout { name, .. } => (
                format!("Timeout {name}"),
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child({
                        let link = TIMEOUT_TEXT.len()..TIMEOUT_TEXT.len() + "Learn more.".len();
                        let styled = StyledText::new(format!("{TIMEOUT_TEXT}Learn more.")).with_highlights([(link.clone(), HighlightStyle { color: Some(gpui_kit::Hsla::from(gpui_kit::rgb(0x00a8fc))), ..Default::default() })]);
                        InteractiveText::new("timeout-learn-more", styled).on_click(vec![link], |_, _, cx| cx.open_url(TIMEOUT_HELP))
                    }))
                    .child(
                        v_flex().gap(u(8.)).child(label("Duration")).child(
                            h_flex().w_full().rounded(u(8.)).border_1().border_color(border).overflow_hidden().children(DURATIONS.iter().enumerate().map(|(i, (name, _))| {
                                let chosen = self.duration == i;
                                let this = this.clone();
                                div()
                                    .id(("timeout-length", i))
                                    .flex_1()
                                    .h(u(38.))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .cursor_pointer()
                                    .text_size(u(14.))
                                    .font_weight(FontWeight::MEDIUM)
                                    .when(i > 0, |d| d.border_l_1().border_color(border))
                                    .when(chosen, |d| d.bg(blurple).text_color(gpui_kit::white()))
                                    .when(!chosen, |d| d.hover(|d| d.bg(fg.opacity(0.06))))
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        let _ = this.update(cx, |panel, cx| {
                                            panel.duration = i;
                                            cx.notify();
                                        });
                                    })
                                    .child(*name)
                            })),
                        ),
                    )
                    .child(v_flex().gap(u(8.)).child(label("Reason")).child(text_box()))
                    .into_any_element(),
                "Timeout",
                blurple,
            ),
            What::Kick { name, handle, .. } => (
                format!("Kick {name} from Server"),
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child(format!("Are you sure you want to kick @{handle} from the server? They will be able to rejoin again with a new invite.")))
                    .child(v_flex().gap(u(8.)).child(label("Reason for Kick")).child(text_box()))
                    .into_any_element(),
                "Kick",
                danger,
            ),
            What::Ban { handle, .. } => (
                format!("Ban @{handle}?"),
                v_flex()
                    .gap(u(16.))
                    .child(
                        v_flex()
                            .gap(u(12.))
                            .child(h_flex().gap(u(4.)).child(label("Reason for Ban")).child(div().text_color(danger).child("*")))
                            .children(BAN_REASONS.iter().enumerate().map(|(i, reason)| {
                                let chosen = self.ban_choice == Some(i);
                                let this = this.clone();
                                h_flex()
                                    .id(("ban-reason", i))
                                    .gap(u(12.))
                                    .items_center()
                                    .cursor_pointer()
                                    .on_click(move |_, _, cx| {
                                        cx.stop_propagation();
                                        let _ = this.update(cx, |panel, cx| {
                                            panel.ban_choice = Some(i);
                                            cx.notify();
                                        });
                                    })
                                    .child(
                                        div()
                                            .size(u(24.))
                                            .flex_shrink_0()
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded_full()
                                            .border_2()
                                            .border_color(if chosen { blurple } else { muted })
                                            .when(chosen, |d| d.child(div().size(u(12.)).rounded_full().bg(blurple))),
                                    )
                                    .child(div().text_size(u(16.)).child(*reason))
                            })),
                    )
                    .when(self.ban_choice == Some(OTHER), |d| d.child(text_box()))
                    .child(v_flex().gap(u(8.)).child(label("Delete Message History")).child(self.history_dropdown(this.clone(), cx)))
                    .into_any_element(),
                "Ban",
                danger,
            ),
            What::Nickname { name, me, .. } => (
                if *me { "Change Nickname".to_string() } else { format!("Change Nickname of {name}") },
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child(if *me { "Pick the name you go by in this server." } else { "This changes the name everyone in this server sees them by. Leave it empty to remove it." }))
                    .child(v_flex().gap(u(8.)).child(label("Nickname")).child(text_box()))
                    .into_any_element(),
                "Save",
                blurple,
            ),
            What::FriendNickname { name, .. } => (
                format!("Change Friend Nickname of {name}"),
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child("Only you see this, outside of servers. Leave it empty to remove it."))
                    .child(v_flex().gap(u(8.)).child(label("Friend Nickname")).child(text_box()))
                    .into_any_element(),
                "Save",
                blurple,
            ),
            What::Forward { to, .. } => {
                let query = self.typed(cx);
                let shown: Vec<&Target> = to.iter().filter(|t| t.matches(&query)).collect();
                let full = self.chosen.len() >= FORWARD_MOST;
                (
                    "Forward To".to_string(),
                    v_flex()
                        .gap(u(12.))
                        .child(text_box())
                        .child(
                            v_flex()
                                .id("forward-list")
                                .h(u(280.))
                                .overflow_y_scroll()
                                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                                .children(shown.into_iter().map(|target| {
                                    let (id, ticked, this) = (target.id, self.chosen.contains(&target.id), this.clone());
                                    h_flex()
                                        .id(("forward-to", id.0 as usize))
                                        .h(u(40.))
                                        .px(u(8.))
                                        .gap(u(10.))
                                        .items_center()
                                        .rounded(u(6.))
                                        .cursor_pointer()
                                        .hover(|d| d.bg(fg.opacity(0.06)))
                                        .when(!ticked && full, |d| d.opacity(0.5))
                                        .on_click(move |_, _, cx| {
                                            cx.stop_propagation();
                                            let _ = this.update(cx, |panel, cx| {
                                                if let Some(at) = panel.chosen.iter().position(|c| *c == id) {
                                                    panel.chosen.remove(at);
                                                } else if panel.chosen.len() < FORWARD_MOST {
                                                    panel.chosen.push(id);
                                                }
                                                cx.notify();
                                            });
                                        })
                                        .child(
                                            h_flex()
                                                .flex_1()
                                                .min_w_0()
                                                .gap(u(8.))
                                                .items_baseline()
                                                .child(div().flex_shrink_0().max_w_full().truncate().text_size(u(15.)).child(target.label.clone()))
                                                .children(target.place.clone().map(|place| div().min_w_0().truncate().text_size(u(12.)).text_color(muted).child(place))),
                                        )
                                        .child(
                                            div()
                                                .size(u(22.))
                                                .flex_shrink_0()
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(u(5.))
                                                .border_1()
                                                .border_color(if ticked { blurple } else { muted.opacity(0.5) })
                                                .when(ticked, |d| d.bg(blurple).child(Icon::new(IconName::Check).size(u(16.)).text_color(gpui_kit::white()))),
                                        )
                                })),
                        )
                        .into_any_element(),
                    "Send",
                    blurple,
                )
            }
            What::Thread { .. } => (
                "Create Thread".to_string(),
                v_flex().gap(u(8.)).child(label("Thread Name")).child(text_box()).into_any_element(),
                "Create Thread",
                blurple,
            ),
            What::GroupName { .. } => (
                "Edit Group".to_string(),
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child("Leave it empty to name the group after who's in it."))
                    .child(v_flex().gap(u(8.)).child(label("Group Name")).child(text_box()))
                    .into_any_element(),
                "Save",
                blurple,
            ),
            What::Note { name, .. } => (
                format!("Note for {name}"),
                v_flex()
                    .gap(u(16.))
                    .child(div().text_size(u(14.)).text_color(muted).child("Only visible to you."))
                    .child(text_box())
                    .into_any_element(),
                "Save",
                blurple,
            ),
        };

        let button = |id: &'static str, text: &'static str, bg: gpui_kit::Hsla, on: gpui_kit::Hsla| {
            div().id(id).flex_1().h(u(44.)).flex().items_center().justify_center().rounded(u(8.)).bg(bg).text_color(on).text_size(u(15.)).font_weight(FontWeight::MEDIUM).child(text)
        };
        v_flex()
            .id("mod-panel")
            .track_focus(&self.focus)
            .occlude()
            .relative()
            .w(u(540.))
            .max_w_full()
            .p(u(30.))
            .gap(u(20.))
            .rounded(u(12.))
            .border_1()
            .border_color(border)
            .bg(theme.popover)
            .shadow_lg()
            .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
            .on_click(cx.listener(|this, _, _, cx| {
                // A click inside is not one on the backdrop; it only shuts an open dropdown.
                cx.stop_propagation();
                if this.dropdown_open {
                    this.dropdown_open = false;
                    cx.notify();
                }
            }))
            .child(
                div()
                    .id("mod-panel-close")
                    .absolute()
                    .top(u(24.))
                    .right(u(24.))
                    .cursor_pointer()
                    .text_color(muted)
                    .hover(|d| d.text_color(fg))
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                        close(cx);
                    })
                    .child(Icon::new(IconName::Close).size(u(22.))),
            )
            .child(div().pr(u(32.)).text_size(u(22.)).font_weight(FontWeight::SEMIBOLD).child(title))
            .child(body)
            .child(
                h_flex()
                    .gap(u(12.))
                    .child(button("mod-cancel", "Cancel", fg.opacity(0.08), fg).cursor_pointer().hover(|d| d.opacity(0.85)).on_click(|_, _, cx| {
                        cx.stop_propagation();
                        close(cx);
                    }))
                    .child(
                        button("mod-confirm", action, tone, gpui_kit::white())
                            .when(!ready, |d| d.opacity(0.5))
                            .when(ready, |d| d.cursor_pointer().hover(|d| d.opacity(0.85)).on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.submit(cx);
                            }))),
                    ),
            )
    }
}

impl Panel {
    /// "Delete Message History": what's chosen, and its choices open beneath.
    fn history_dropdown(&self, this: WeakEntity<Self>, cx: &Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (fg, border, muted) = (theme.foreground, theme.border, theme.muted_foreground);
        let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
        let open = self.dropdown_open;
        let toggle = this.clone();
        let field = h_flex()
            .id("ban-history")
            .h(u(44.))
            .px(u(16.))
            .items_center()
            .justify_between()
            .rounded(u(8.))
            .border_1()
            .border_color(if open { blurple } else { border })
            .bg(theme.background.opacity(0.5))
            .text_size(u(16.))
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let _ = toggle.update(cx, |panel, cx| {
                    panel.dropdown_open = !panel.dropdown_open;
                    cx.notify();
                });
            })
            .child(HISTORY[self.history].0)
            .child(Icon::new(if open { IconName::ChevronUp } else { IconName::ChevronDown }).size(u(20.)));
        let options = open.then(|| {
            v_flex()
                .id("ban-history-options")
                .occlude()
                .absolute()
                .top(u(48.))
                .left_0()
                .right_0()
                .max_h(u(260.))
                .overflow_y_scroll()
                .p(u(6.))
                .rounded(u(8.))
                .border_1()
                .border_color(border)
                .bg(theme.popover)
                .shadow_lg()
                .on_scroll_wheel(|_, _, cx| cx.stop_propagation())
                .children(HISTORY.iter().enumerate().map(|(i, (name, _))| {
                    let this = this.clone();
                    h_flex()
                        .id(("ban-history-option", i))
                        .h(u(40.))
                        .px(u(12.))
                        .items_center()
                        .justify_between()
                        .rounded(u(6.))
                        .text_size(u(16.))
                        .cursor_pointer()
                        .hover(|d| d.bg(fg.opacity(0.08)))
                        .on_click(move |_, _, cx| {
                            cx.stop_propagation();
                            let _ = this.update(cx, |panel, cx| {
                                panel.history = i;
                                panel.dropdown_open = false;
                                cx.notify();
                            });
                        })
                        .child(*name)
                        .when(self.history == i, |d| d.child(Icon::new(IconName::Check).size(u(18.)).text_color(muted)))
                }))
        });
        div().relative().w_full().child(field).children(options).into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ban_needs_a_reason_and_other_needs_words() {
        assert_eq!(ban_reason(None, "anything"), None);
        assert_eq!(ban_reason(Some(0), ""), Some("Suspicious or spam account".to_string()));
        assert_eq!(ban_reason(Some(2), "ignored text"), Some("Breaking server rules".to_string()));
        assert_eq!(ban_reason(Some(OTHER), "   "), None);
        assert_eq!(ban_reason(Some(OTHER), " raiding "), Some("raiding".to_string()));
    }

    #[test]
    fn a_timeout_ends_that_long_after_now() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-11-05T10:00:00Z").unwrap().with_timezone(&chrono::Utc);
        assert_eq!(timeout_until(now, 60), "2026-11-05T10:01:00.000Z");
        assert_eq!(timeout_until(now, 604_800), "2026-11-12T10:00:00.000Z");
    }

    fn target(id: u64, label: &str, place: Option<&str>) -> Target {
        Target { id: Id(id), label: label.into(), place: place.map(str::to_string) }
    }

    #[test]
    fn forwarding_searches_the_label_and_the_server_ignoring_case() {
        let channel = target(1, "#general", Some("Rust Friends"));
        assert!(channel.matches(""));
        assert!(channel.matches(" GENERAL "));
        assert!(channel.matches("rust fri"), "a channel is found by its server too");
        assert!(!channel.matches("eve"));
        assert!(!target(2, "Ada, Bob", None).matches("rust"));
    }

    #[test]
    fn forward_targets_list_chats_then_the_same_server_then_the_rest() {
        use cord_core::cache::Destination;
        let dest = |id: u64, name: &str, guild: u64, server: &str| Destination { id: Id(id), name: name.into(), guild: Id(guild), server: server.into() };
        let all = forward_targets(
            vec![(Id(1), "Ada".into())],
            vec![dest(10, "a", 7, "Other"), dest(11, "b", 8, "Here"), dest(12, "c", 7, "Other"), dest(13, "d", 8, "Here")],
            Some(Id(8)),
        );
        assert_eq!(all.iter().map(|t| t.id.0).collect::<Vec<_>>(), [1, 11, 13, 10, 12]);
        assert_eq!((all[1].label.as_str(), all[1].place.as_deref()), ("#b", Some("Here")));
        assert_eq!(all[0].place, None);
    }

    #[test]
    fn the_choices_are_discords() {
        assert_eq!(DURATIONS.map(|(_, s)| s), [60, 300, 600, 3600, 86_400, 604_800]);
        assert_eq!(HISTORY[DEFAULT_HISTORY].0, "Previous Hour");
        assert_eq!(HISTORY[0].1, 0);
    }
}
