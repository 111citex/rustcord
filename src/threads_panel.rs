//! The list the Threads button opens: a channel's active threads and its
//! archived ones, newest activity first.

use cord_core::model::Channel;
use cord_core::{Id, ThreadSearch};
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Context, Entity, EventEmitter, FontWeight, InteractiveElement as _, IntoElement, ParentElement as _, Render, ScrollHandle,
    SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, Window, div,
};

use crate::nav;
use crate::router::{RouteContext, navigate};
use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::threads as wording;
use crate::ui::u;

/// The panel asks to be put away (a thread was picked, or its X was).
pub struct Closed;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Tab {
    Active,
    Archived,
}

/// How far asking Discord for one tab's threads has got.
#[derive(Clone, Debug, PartialEq)]
enum Fetch {
    Idle,
    Loading,
    /// All asked for; more can be fetched when true.
    Done(bool),
    Failed(String),
}

pub struct ThreadsPanel {
    ctx: RouteContext,
    parent: Id,
    tab: Tab,
    fetch: [Fetch; 2],
    asked: [u32; 2],
    scroll: ScrollHandle,
    scroller: SmoothScroller,
    _store: Subscription,
}

impl EventEmitter<Closed> for ThreadsPanel {}

pub fn new(ctx: RouteContext, parent: Id, cx: &mut App) -> Entity<ThreadsPanel> {
    use gpui_kit::AppContext as _;
    cx.new(|cx| {
        let scroll = ScrollHandle::new();
        let store = ctx.services.store.clone();
        let mut panel = ThreadsPanel {
            ctx,
            parent,
            tab: Tab::Active,
            fetch: [Fetch::Idle, Fetch::Idle],
            asked: [0, 0],
            scroller: SmoothScroller::new(ScrollTarget::Handle(scroll.clone())).calm_hover().layered(),
            scroll,
            _store: cx.observe(&store, |_, _, cx| cx.notify()),
        };
        panel.fetch_more(cx);
        panel
    })
}

/// Which of `threads` a tab lists, most recently active first.
fn tab_rows(threads: Vec<&Channel>, archived: bool) -> Vec<&Channel> {
    let mut rows: Vec<&Channel> = threads.into_iter().filter(|t| t.thread.as_ref().is_some_and(|i| i.archived == archived)).collect();
    rows.sort_by_key(|t| std::cmp::Reverse(t.last_message_id.unwrap_or(t.id).max(t.id)));
    rows
}

impl ThreadsPanel {
    fn index(tab: Tab) -> usize {
        tab as usize
    }

    fn fetch_more(&mut self, cx: &mut Context<Self>) {
        let i = Self::index(self.tab);
        if matches!(self.fetch[i], Fetch::Loading) {
            return;
        }
        self.fetch[i] = Fetch::Loading;
        let (core, parent, tab, offset) = (self.ctx.services.core.clone(), self.parent, self.tab, self.asked[i]);
        let store = self.ctx.services.store.clone();
        cx.notify();
        cx.spawn(async move |this, cx| {
            let search = ThreadSearch { archived: tab == Tab::Archived, offset, ..Default::default() };
            let result = core.search_threads(parent, search).await;
            let _ = this.update(cx, |panel, cx| {
                let i = Self::index(tab);
                match result {
                    Ok(page) => {
                        panel.asked[i] += page.threads.len() as u32;
                        panel.fetch[i] = Fetch::Done(page.has_more && !page.threads.is_empty());
                        store.update(cx, |store, cx| store.add_threads(page.threads, &page.joined, cx));
                    }
                    Err(error) => panel.fetch[i] = Fetch::Failed(error.to_string()),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn pick_tab(&mut self, tab: Tab, cx: &mut Context<Self>) {
        self.tab = tab;
        if matches!(self.fetch[Self::index(tab)], Fetch::Idle | Fetch::Failed(_)) {
            self.fetch_more(cx);
        }
        cx.notify();
    }

    fn open(&mut self, thread: Id, cx: &mut Context<Self>) {
        let Some(path) = self.ctx.services.store.read(cx).cache().channel(thread).map(nav::channel_path) else { return };
        navigate(&self.ctx.router, path, cx);
        cx.emit(Closed);
    }
}

impl Render for ThreadsPanel {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let (muted, border) = (theme.muted_foreground, theme.border);
        let cache = self.ctx.services.store.read(cx).cache();
        let archived = self.tab == Tab::Archived;
        let now = chrono::Utc::now().timestamp_millis().max(0) as u64;
        let rows: Vec<AnyElement> = tab_rows(cache.threads_under(self.parent), archived)
            .into_iter()
            .enumerate()
            .map(|(ix, t)| {
                let info = t.thread.clone().unwrap_or_default();
                let unread = cache.is_unread(t.id);
                let by = wording::starter_name(cache, t);
                let active = t.last_message_id.unwrap_or(t.id).max(t.id).timestamp_ms();
                let id = t.id;
                h_flex()
                    .id(("thread-row", ix))
                    .w_full()
                    .px(u(12.))
                    .py(u(8.))
                    .gap(u(10.))
                    .items_center()
                    .rounded(u(6.))
                    .cursor_pointer()
                    .hover(|d| d.bg(theme.foreground.opacity(0.06)))
                    .on_click(cx.listener(move |this, _, _, cx| this.open(id, cx)))
                    .child(Icon::default().path("icons/toolbar-threads.svg").size(u(20.)).text_color(muted))
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .child(div().truncate().text_size(u(15.)).font_weight(if unread { FontWeight::BOLD } else { FontWeight::MEDIUM }).child(SharedString::from(t.name.clone().unwrap_or_default())))
                            .child(
                                div()
                                    .truncate()
                                    .text_size(u(12.))
                                    .text_color(muted)
                                    .child(format!("{} \u{2022} {}{}", wording::messages(info.message_count), wording::ago(now, active), by.map(|b| format!(" \u{2022} {b}")).unwrap_or_default())),
                            ),
                    )
                    .when(info.locked, |d| d.child(div().flex_shrink_0().text_size(u(11.)).text_color(muted).child("Locked")))
                    .when(info.joined && !archived, |d| d.child(div().flex_shrink_0().text_size(u(11.)).text_color(muted).child("Joined")))
                    .when(unread, |d| d.child(div().flex_shrink_0().size(u(8.)).rounded_full().bg(theme.foreground)))
                    .into_any_element()
            })
            .collect();
        let i = Self::index(self.tab);
        let status = match &self.fetch[i] {
            Fetch::Loading => Some("Loading threads\u{2026}".to_string()),
            Fetch::Failed(error) => Some(format!("Couldn't load threads: {error}")),
            Fetch::Done(_) | Fetch::Idle if rows.is_empty() => {
                Some(if archived { "No archived threads." } else { "There are no active threads." }.to_string())
            }
            _ => None,
        };
        let more = matches!(self.fetch[i], Fetch::Done(true));
        let smooth = self.ctx.services.settings.read(cx).get().smooth_scrolling;
        let tab = |id: &'static str, label: &'static str, this: Tab| {
            let on = self.tab == this;
            div()
                .id(id)
                .px(u(4.))
                .pb(u(6.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .cursor_pointer()
                .border_b_2()
                .border_color(if on { theme.foreground } else { gpui_kit::transparent_black() })
                .text_color(if on { theme.foreground } else { muted })
                .on_click(cx.listener(move |this_panel, _, _, cx| this_panel.pick_tab(this, cx)))
                .child(label)
        };
        v_flex()
            .id("threads-panel")
            .occlude()
            .absolute()
            .top(u(53.))
            .right(u(16.))
            .w(u(420.))
            .max_w_full()
            .max_h(u(520.))
            .rounded(u(8.))
            .border_1()
            .border_color(border)
            .bg(theme.popover)
            .shadow_lg()
            .child(
                h_flex()
                    .flex_shrink_0()
                    .px(u(16.))
                    .pt(u(12.))
                    .gap(u(16.))
                    .items_end()
                    .border_b_1()
                    .border_color(border)
                    .child(div().pb(u(6.)).text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child("Threads"))
                    .child(tab("threads-active", "Active", Tab::Active))
                    .child(tab("threads-archived", "Archived", Tab::Archived))
                    .child(div().flex_1())
                    .child(
                        div()
                            .id("threads-close")
                            .pb(u(6.))
                            .cursor_pointer()
                            .text_color(muted)
                            .hover(|d| d.text_color(theme.foreground))
                            .on_click(cx.listener(|_, _, _, cx| cx.emit(Closed)))
                            .child(Icon::new(IconName::Close).size(u(18.))),
                    ),
            )
            .child(
                div()
                    .relative()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div().id("threads-list").size_full().max_h(u(460.)).overflow_y_scroll().track_scroll(&self.scroll).child(
                            v_flex()
                                .w_full()
                                .p(u(8.))
                                .children(rows)
                                .when_some(status, |d, text| d.child(div().p(u(16.)).text_center().text_size(u(14.)).text_color(muted).child(text)))
                                .when(more, |d| {
                                    d.child(
                                        div()
                                            .id("threads-more")
                                            .mt(u(4.))
                                            .h(u(32.))
                                            .flex()
                                            .items_center()
                                            .justify_center()
                                            .rounded(u(6.))
                                            .cursor_pointer()
                                            .text_size(u(14.))
                                            .text_color(theme.link)
                                            .hover(|d| d.bg(theme.foreground.opacity(0.06)))
                                            .on_click(cx.listener(|this, _, _, cx| this.fetch_more(cx)))
                                            .child("Load more"),
                                    )
                                }),
                        ),
                    )
                    .when(smooth, |d| d.child(self.scroller.layer())),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn thread(id: u64, last: Option<u64>, archived: bool) -> Channel {
        serde_json::from_value(serde_json::json!({
            "id": id.to_string(), "type": 11, "parent_id": "9", "last_message_id": last.map(|l| l.to_string()),
            "thread_metadata": {"archived": archived},
        }))
        .unwrap()
    }

    #[test]
    fn a_tab_lists_its_own_threads_by_latest_activity() {
        let (a, b, c, d) = (thread(10, Some(50), false), thread(20, None, false), thread(30, Some(40), true), thread(5, Some(900), true));
        let all = vec![&a, &b, &c, &d];
        let ids = |archived| tab_rows(all.clone(), archived).iter().map(|t| t.id.0).collect::<Vec<_>>();
        assert_eq!(ids(false), vec![10, 20]);
        assert_eq!(ids(true), vec![5, 30]);
    }
}
