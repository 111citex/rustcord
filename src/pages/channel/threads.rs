//! Threads in the chat page: the chip under a message that started one, a
//! thread's own first rows, joining and leaving, and starting a thread.

use gpui_kit::component::input::{Input, InputEvent, InputState};

use super::*;
use crate::threads as wording;

/// The dialog that names a thread before it's made.
pub(super) struct NewThread {
    message: Id,
    name: Entity<InputState>,
    _enter: Subscription,
}

const BLURPLE: u32 = 0x5865f2;

impl ChannelPage {
    pub(super) fn open_thread(&self, thread: Id, cx: &mut Context<Self>) {
        let Some(path) = self.ctx.services.store.read(cx).cache().channel(thread).map(nav::channel_path) else { return };
        navigate(&self.ctx.router, path, cx);
    }

    /// Opens the box that names a new thread, started from `message`.
    pub(super) fn start_new_thread(&mut self, message: Id, window: &mut Window, cx: &mut Context<Self>) {
        let Some(channel) = self.channel else { return };
        let cache = self.ctx.services.store.read(cx).cache();
        let Some(m) = cache.messages(channel).and_then(|m| m.iter().find(|m| m.message.id == message)) else { return };
        let name = cord_core::forum::default_thread_name(&cache.readable_text(&m.message));
        let input = cx.new(|cx| InputState::new(window, cx).placeholder("Thread name").default_value(name));
        let enter = cx.subscribe_in(&input, window, |this, _, event: &InputEvent, _, cx| {
            if matches!(event, InputEvent::PressEnter { .. }) {
                this.make_thread(cx);
            }
        });
        input.update(cx, |input, cx| {
            input.focus(window, cx);
            input.select_all(window, cx);
        });
        self.new_thread = Some(NewThread { message, name: input, _enter: enter });
        cx.notify();
    }

    /// The dialog's Create Thread: makes it, then goes there.
    fn make_thread(&mut self, cx: &mut Context<Self>) {
        let (Some(channel), Some(new)) = (self.channel, self.new_thread.take()) else { return };
        let name = new.name.read(cx).value().trim().chars().take(cord_core::forum::NAME_LIMIT).collect::<String>();
        cx.notify();
        if name.is_empty() {
            self.ctx.services.toast(Toast::warning("Name the thread", "A thread needs a name."), cx);
            self.new_thread = Some(new);
            return;
        }
        let task = self.ctx.services.store.update(cx, |store, cx| store.start_thread(channel, new.message, name, cx));
        cx.spawn(async move |this, cx| {
            if let Some(id) = task.await {
                let _ = this.update(cx, |page, cx| page.open_thread(id, cx));
            }
        })
        .detach();
    }

    /// Closes what the thread tools put on top of the page; whether anything was.
    pub(super) fn close_thread_overlays(&mut self, cx: &mut Context<Self>) -> bool {
        let open = self.new_thread.is_some() || self.threads_panel.is_some();
        self.new_thread = None;
        self.threads_panel = None;
        if open {
            cx.notify();
        }
        open
    }

    /// The Threads button: the list of this channel's threads, or put away again.
    pub(super) fn toggle_threads(&mut self, cx: &mut Context<Self>) {
        if self.threads_panel.take().is_some() {
            cx.notify();
            return;
        }
        let Some(channel) = self.channel else { return };
        let cache = self.ctx.services.store.read(cx).cache();
        // From inside a thread it lists the threads of the channel it's in.
        let parent = cache.thread(channel).and_then(|t| t.parent_id).unwrap_or(channel);
        let panel = crate::threads_panel::new(self.ctx.clone(), parent, cx);
        let close = cx.subscribe(&panel, |this, _, _: &crate::threads_panel::Closed, cx| {
            this.threads_panel = None;
            cx.notify();
        });
        self.threads_panel = Some((panel, close));
        cx.notify();
    }

    /// "Join Thread" or "Leave Thread", in the header of a thread we can see.
    pub(super) fn join_button(&self, thread: Id, cx: &Context<Self>) -> Option<AnyElement> {
        let cache = self.ctx.services.store.read(cx).cache();
        let info = cache.thread(thread)?.thread.as_ref()?;
        let joined = info.joined;
        // A private thread can't be joined from outside, and a locked one not by us.
        let theme = cx.theme();
        let (bg, fg) = if joined { (theme.foreground.opacity(0.08), theme.foreground) } else { (Hsla::from(gpui_kit::rgb(BLURPLE)), gpui_kit::white()) };
        Some(
            div()
                .id("join-thread")
                .h(u(28.))
                .px(u(12.))
                .mr(u(4.))
                .flex()
                .items_center()
                .flex_shrink_0()
                .rounded(u(6.))
                .bg(bg)
                .text_color(fg)
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .hover(|d| d.opacity(0.85))
                .child(if joined { "Leave Thread" } else { "Join Thread" })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.ctx.services.store.update(cx, |store, cx| {
                        if joined {
                            store.leave_thread(thread, cx)
                        } else {
                            store.join_thread(thread, cx)
                        }
                    });
                }))
                .into_any_element(),
        )
    }

    pub(super) fn new_thread_dialog(&self, window: &Window, cx: &Context<Self>) -> Option<AnyElement> {
        let new = self.new_thread.as_ref()?;
        let theme = cx.theme();
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
        let backdrop = div()
            .id("new-thread-backdrop")
            .occlude()
            .w(viewport.width)
            .h(viewport.height)
            .flex()
            .items_center()
            .justify_center()
            .bg(gpui_kit::black().opacity(0.6))
            .on_click(cx.listener(|this, _, _, cx| {
                this.new_thread = None;
                cx.notify();
            }))
            .child(
                v_flex()
                    .id("new-thread-dialog")
                    .occlude()
                    .w(u(440.))
                    .max_w_full()
                    .p(u(20.))
                    .gap(u(16.))
                    .rounded(u(12.))
                    .border_1()
                    .border_color(theme.border)
                    .bg(theme.popover)
                    .shadow_lg()
                    .on_click(|_, _, cx| cx.stop_propagation())
                    .child(div().text_size(u(20.)).font_weight(FontWeight::SEMIBOLD).child("Create Thread"))
                    .child(
                        v_flex()
                            .gap(u(8.))
                            .child(div().text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(theme.muted_foreground).child("THREAD NAME"))
                            .child(Input::new(&new.name)),
                    )
                    .child(
                        h_flex()
                            .justify_end()
                            .gap(u(8.))
                            .child(button("new-thread-cancel", "Cancel", theme.foreground.opacity(0.08), theme.foreground).on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.new_thread = None;
                                cx.notify();
                            })))
                            .child(button("new-thread-make", "Create Thread", Hsla::from(gpui_kit::rgb(BLURPLE)), gpui_kit::white()).on_click(cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.make_thread(cx);
                            }))),
                    ),
            );
        Some(gpui_kit::deferred(gpui_kit::anchored().position(gpui_kit::point(gpui_kit::px(0.), gpui_kit::px(0.))).child(backdrop)).with_priority(5).into_any_element())
    }
}

/// What a thread's top of the page says: who started it and under what.
pub(super) fn intro(cache: &Cache, thread: &cord_core::model::Channel, label: &str, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let by = wording::starter_name(cache, thread);
    let parent = thread.parent_id.and_then(|p| cache.channel(p)).map(|p| nav::channel_label(cache, p));
    let created = thread.thread.as_ref().and_then(|t| t.created_at.as_deref()).and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok()).map(|t| t.timestamp_millis().max(0) as u64);
    let when = created.unwrap_or_else(|| thread.id.timestamp_ms());
    v_flex()
        .w_full()
        .px_4()
        .pt(u(16.))
        .pb(u(8.))
        .gap(u(4.))
        .child(
            div()
                .size(u(64.))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme.foreground.opacity(0.08))
                .child(Icon::default().path("icons/toolbar-threads.svg").size(u(36.))),
        )
        .child(div().text_size(u(28.)).line_height(u(34.)).font_weight(FontWeight::BOLD).child(label.to_string()))
        .child(
            div()
                .text_size(u(14.))
                .text_color(theme.muted_foreground)
                .child(match (by, parent) {
                    (Some(name), Some(parent)) => format!("Started by {name} in #{parent}"),
                    (Some(name), None) => format!("Started by {name}"),
                    (None, Some(parent)) => format!("A thread in #{parent}"),
                    (None, None) => "Thread".to_string(),
                }),
        )
        .child(div().text_size(u(12.)).text_color(theme.muted_foreground).child(nav::format_message_time(when, &chrono::Local::now())))
        .into_any_element()
}

/// A thread's first message: the one it was started from, in a card, with
/// no header of its own (the top of the page already says who started it).
#[allow(clippy::too_many_arguments)]
pub(super) fn starter_row(
    ix: usize,
    m: &ChatMessage,
    margin: bool,
    guild: Option<Id>,
    cache: &Cache,
    shared: &Shared,
    page: &WeakEntity<ChannelPage>,
    cx: &App,
) -> AnyElement {
    let theme = cx.theme();
    let original = m.message.referenced_message.as_deref();
    let parent = cache.thread(m.message.channel_id).and_then(|t| t.parent_id);
    let body = match original {
        Some(original) => {
            let by = byline(cache, guild, &original.author);
            v_flex()
                .gap(u(4.))
                .child(
                    h_flex()
                        .gap(u(8.))
                        .items_center()
                        .child(Avatar::new().border_0().src(cache.avatar_url(guild, &original.author, 40)).name(by.name.clone()).size(u(20.)))
                        .child(div().font_weight(FontWeight::MEDIUM).when_some(by.color, |d, c| d.text_color(c)).child(by.name.clone()))
                        .child(div().text_size(u(12.)).text_color(theme.muted_foreground).child(nav::format_message_time(original.id.timestamp_ms(), &chrono::Local::now()))),
                )
                .child(message_content(original, guild, cache, shared, cx))
                .into_any_element()
        }
        None => div().italic().text_color(theme.muted_foreground).child("Sorry, we couldn't load the first message in this thread.").into_any_element(),
    };
    let row = h_flex().id(("message", ix)).w_full().px_4().py(u(2.)).child(
        v_flex()
            .w_full()
            .min_w_0()
            .gap(u(6.))
            .p(u(12.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.foreground.opacity(0.03))
            .child(body)
            .when_some(parent, |d, parent| {
                let page = page.clone();
                d.child(
                    div()
                        .id(("starter-parent", ix))
                        .text_size(u(12.))
                        .text_color(theme.link)
                        .cursor_pointer()
                        .hover(|d| d.underline())
                        .on_click(move |_, _, cx| {
                            let _ = page.update(cx, |page, cx| page.open_thread(parent, cx));
                        })
                        .child("View in the channel"),
                )
            }),
    );
    if margin { div().w_full().pt(u(17.)).child(row).into_any_element() } else { row.into_any_element() }
}

/// The chip under a message that started a thread: its name, how busy it is,
/// and the last thing said. Clicking it goes in.
pub(super) fn chip(ix: usize, cache: &Cache, message: &Message, page: &WeakEntity<ChannelPage>, cx: &App) -> Option<AnyElement> {
    let thread = cache.thread_of(message)?;
    let info = thread.thread.as_ref()?;
    let theme = cx.theme();
    let unread = cache.is_unread(thread.id);
    let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
    let last = cache.messages(thread.id).and_then(|m| m.iter().rev().find(|m| m.message.kind != 21)).map(|m| &m.message);
    let active = thread.last_message_id.map(Id::timestamp_ms).unwrap_or_else(|| thread.id.timestamp_ms());
    let preview = last.map(|m| format!("{}: {}", cache.display_name(thread.guild_id, &m.author), wording::one_line(&cache.readable_text(m), 80)));
    let id = thread.id;
    let page = page.clone();
    Some(
        h_flex()
            .id(("thread-chip", ix))
            .mt(u(4.))
            .w(u(420.))
            .max_w_full()
            .px(u(12.))
            .py(u(8.))
            .gap(u(10.))
            .items_center()
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .cursor_pointer()
            .hover(|d| d.bg(theme.foreground.opacity(0.06)))
            .on_click(move |_, _, cx| {
                let _ = page.update(cx, |page, cx| page.open_thread(id, cx));
            })
            .child(Icon::default().path("icons/toolbar-threads.svg").size(u(20.)).text_color(theme.muted_foreground))
            .child(
                v_flex()
                    .flex_1()
                    .min_w_0()
                    .child(div().truncate().font_weight(if unread { FontWeight::BOLD } else { FontWeight::SEMIBOLD }).text_size(u(15.)).child(thread.name.clone().unwrap_or_default()))
                    .child(
                        h_flex()
                            .gap(u(6.))
                            .text_size(u(13.))
                            .text_color(theme.muted_foreground)
                            .child(div().flex_shrink_0().child(wording::messages(info.message_count)))
                            .child(div().flex_1().min_w_0().truncate().child(preview.unwrap_or_else(|| wording::ago(now, active)))),
                    ),
            )
            .when(unread, |d| d.child(div().flex_shrink_0().size(u(8.)).rounded_full().bg(theme.danger)))
            .child(Icon::new(IconName::ChevronRight).size(u(16.)).text_color(theme.muted_foreground))
            .into_any_element(),
    )
}
