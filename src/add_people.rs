//! "Add to DM": pick friends to add to a group, with a search.

use std::rc::Rc;

use cord_core::Id;
use cord_core::member_list::Status;
use gpui_kit::assets::IconName;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{AnyElement, App, AppContext as _, Context, Entity, FontWeight, Global, InteractiveElement as _, IntoElement, ListAlignment, ListState, ParentElement as _, Render, SharedString, StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, anchored, deferred, div, list, point, px};

use crate::smooth_scroll::{ScrollTarget, SmoothScroller};
use crate::store::Store;
use crate::ui::u;

#[derive(Clone)]
struct Person {
    id: Id,
    name: String,
    username: String,
    avatar: String,
    status: Option<Status>,
    mobile: bool,
}

pub struct AddPeople {
    store: Entity<Store>,
    channel: Id,
    /// How many more the group can take.
    room: usize,
    people: Vec<Person>,
    search: Entity<InputState>,
    chosen: Vec<Id>,
    rows: ListState,
    scroller: SmoothScroller,
    /// What the list was last sized for.
    shown_for: String,
    _watching: Vec<Subscription>,
}

struct Open(Entity<AddPeople>);
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

/// Opens it for `channel`. `inside` are already in; `room` is how many more fit.
pub fn open(store: Entity<Store>, channel: Id, inside: Vec<Id>, room: usize, window: &mut Window, cx: &mut App) {
    let cache = store.read(cx).cache();
    let me = cache.me().map(|u| u.id);
    let mut people: Vec<Person> = cache
        .friend_ids()
        .into_iter()
        .filter(|id| !inside.contains(id) && Some(*id) != me)
        .filter_map(|id| {
            let user = cache.user(id)?;
            let presence = cache.presence(id);
            Some(Person {
                id,
                name: user.display_name().to_string(),
                username: user.username.clone(),
                avatar: cache.avatar_url(None, user, 64),
                status: Some(presence.map_or(Status::Offline, |p| p.status)),
                mobile: presence.is_some_and(|p| p.mobile),
            })
        })
        .collect();
    people.sort_by_cached_key(|p| p.name.to_lowercase());
    let panel = cx.new(|cx| {
        let search = cx.new(|cx| InputState::new(window, cx).placeholder("Search"));
        search.update(cx, |input, cx| input.focus(window, cx));
        let typed = cx.subscribe(&search, |_, _, _: &InputEvent, cx| cx.notify());
        let escape = cx.intercept_keystrokes(|event, _, cx| {
            if event.keystroke.key == "escape" {
                close(cx);
                cx.stop_propagation();
            }
        });
        let rows = ListState::new(people.len(), ListAlignment::Top, px(300.));
        let scroller = SmoothScroller::new(ScrollTarget::List(rows.clone())).owned_by("add-people");
        AddPeople { store, channel, room, people, search, chosen: Vec::new(), rows, scroller, shown_for: String::new(), _watching: vec![typed, escape] }
    });
    cx.set_global(Open(panel));
    cx.refresh_windows();
}

pub fn overlay(window: &Window, cx: &App) -> Option<AnyElement> {
    let panel = cx.try_global::<Open>()?.0.clone();
    let viewport = window.viewport_size();
    let backdrop = div()
        .id("add-people-backdrop")
        .occlude()
        .w(viewport.width)
        .h(viewport.height)
        .flex()
        .items_center()
        .justify_center()
        .bg(gpui_kit::black().opacity(0.6))
        .on_click(|_, _, cx| close(cx))
        .child(panel);
    Some(deferred(anchored().position(point(px(0.), px(0.))).child(backdrop)).with_priority(4).into_any_element())
}

/// Whether someone's name or username has all of `query`, ignoring case.
fn matches(name: &str, username: &str, query: &str) -> bool {
    let query = query.trim().to_lowercase();
    query.is_empty() || name.to_lowercase().contains(&query) || username.to_lowercase().contains(&query)
}

/// "You can add …".
fn room_line(left: usize) -> String {
    match left {
        0 => "This group is full.".to_string(),
        1 => "You can add 1 more person.".to_string(),
        n => format!("You can add {n} more people."),
    }
}

impl AddPeople {
    fn toggle(&mut self, id: Id, cx: &mut Context<Self>) {
        if let Some(at) = self.chosen.iter().position(|c| *c == id) {
            self.chosen.remove(at);
        } else if self.chosen.len() < self.room {
            self.chosen.push(id);
        }
        cx.notify();
    }

    fn add(&mut self, cx: &mut Context<Self>) {
        if self.chosen.is_empty() {
            return;
        }
        let (channel, users) = (self.channel, std::mem::take(&mut self.chosen));
        self.store.update(cx, |store, cx| store.add_to_group(channel, users, cx));
        close(cx);
    }
}

impl Render for AddPeople {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme();
        let query = self.search.read(cx).value().to_string();
        let shown: Rc<Vec<Person>> = Rc::new(self.people.iter().filter(|p| matches(&p.name, &p.username, &query)).cloned().collect());
        let chosen: Rc<Vec<Id>> = Rc::new(self.chosen.clone());
        // Typing changes who's listed: back to the top of the new list.
        if self.shown_for != query {
            self.shown_for = query.clone();
            self.scroller.stop();
            self.rows.reset(shown.len());
        }
        let left = self.room.saturating_sub(self.chosen.len());
        let height = (f32::from(window.viewport_size().height) - 80.).clamp(320., 620.);
        let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
        let panel: WeakEntity<Self> = cx.entity().downgrade();
        let any_chosen = !self.chosen.is_empty();
        let (rows, room_left) = (shown.clone(), left > 0);
        let list = list(self.rows.clone(), move |ix, _, cx: &mut App| -> AnyElement {
            let theme = cx.theme();
            let Some(person) = rows.get(ix) else { return div().into_any_element() };
                    let ticked = chosen.contains(&person.id);
                    let (panel, id) = (panel.clone(), person.id);
                    h_flex()
                        .id(("add-person", ix))
                        .w_full()
                        .h(u(44.))
                        .px(u(8.))
                        .gap(u(10.))
                        .items_center()
                        .rounded(u(6.))
                        .cursor_pointer()
                        .hover(|d| d.bg(theme.foreground.opacity(0.06)))
                        .when(!ticked && !room_left, |d| d.opacity(0.5))
                        .on_click(move |_, _, cx| {
                            let _ = panel.update(cx, |panel, cx| panel.toggle(id, cx));
                        })
                        .child(crate::members::avatar_with_status(
                            Avatar::new().border_0().src(person.avatar.clone()).name(person.name.clone()),
                            32.,
                            person.status,
                            person.mobile,
                            None,
                            theme.popover,
                        ))
                        .child(
                            v_flex()
                                .flex_1()
                                .min_w_0()
                                .child(div().min_w_0().truncate().text_size(u(15.)).line_height(u(20.)).font_weight(FontWeight::MEDIUM).child(SharedString::from(person.name.clone())))
                                .child(div().min_w_0().truncate().text_size(u(12.)).line_height(u(15.)).text_color(theme.muted_foreground).child(SharedString::from(person.username.clone()))),
                        )
                        .child(
                            div()
                                .flex_shrink_0()
                                .size(u(22.))
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded(u(5.))
                                .border_1()
                                .border_color(if ticked { blurple } else { theme.muted_foreground.opacity(0.5) })
                                .when(ticked, |d| d.bg(blurple).child(Icon::new(IconName::Check).size(u(16.)).text_color(gpui_kit::white()))),
                        )
                        .into_any_element()
        })
        .flex_1()
        .w_full();
        v_flex()
            .id("add-people")
            .occlude()
            .relative()
            .w(u(460.))
            .max_w_full()
            .h(u(height))
            .rounded(u(12.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .on_click(|_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .id("add-people-close")
                    .absolute()
                    .top(u(16.))
                    .right(u(16.))
                    .cursor_pointer()
                    .text_color(theme.muted_foreground)
                    .hover(|d| d.text_color(theme.foreground))
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                        close(cx);
                    })
                    .child(Icon::new(IconName::Close).size(u(20.))),
            )
            .child(
                v_flex()
                    .flex_shrink_0()
                    .px(u(20.))
                    .pt(u(20.))
                    .pb(u(10.))
                    .gap(u(4.))
                    .child(div().text_size(u(20.)).font_weight(FontWeight::SEMIBOLD).child("Add to DM"))
                    .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child(room_line(left)))
                    .child(div().pt(u(10.)).child(Input::new(&self.search)))
                    .child(div().pt(u(4.)).text_size(u(12.)).text_color(theme.muted_foreground).child("Add friends to this group.")),
            )
            .child(div().flex_1().min_h_0().w_full().px(u(12.)).flex().flex_col().when(shown.is_empty(), |d| d.items_center().justify_center().text_color(theme.muted_foreground).child("No friends found")).when(!shown.is_empty(), |d| d.relative().child(self.scroller.layer()).child(list)))
            .child(
                div().flex_shrink_0().p(u(16.)).border_t_1().border_color(theme.border).child(
                    div()
                        .id("add-people-confirm")
                        .h(u(40.))
                        .w_full()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(u(8.))
                        .bg(blurple)
                        .text_color(gpui_kit::white())
                        .text_size(u(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .when(!any_chosen, |d| d.opacity(0.5))
                        .when(any_chosen, |d| d.cursor_pointer().hover(|d| d.opacity(0.85)).on_click(cx.listener(|this, _, _, cx| this.add(cx))))
                        .child("Add to Group"),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_looks_at_both_names_ignoring_case() {
        assert!(matches("Ada", "lovelace", ""));
        assert!(matches("Ada", "lovelace", " aD "));
        assert!(matches("Ada", "lovelace", "LOVE"));
        assert!(!matches("Ada", "lovelace", "bob"));
    }

    #[test]
    fn the_line_counts_whats_left() {
        assert_eq!(room_line(0), "This group is full.");
        assert_eq!(room_line(1), "You can add 1 more person.");
        assert_eq!(room_line(8), "You can add 8 more people.");
    }
}
