//! What search looks like: the box with its filter menu, and the results column.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use cord_core::Id;
use cord_core::search::{HAS, PAGE, Sort};
use gpui_kit::assets::IconName;
use gpui_kit::component::avatar::Avatar;
use gpui_kit::component::input::Input;
use gpui_kit::component::{ActiveTheme, Icon, Sizable as _, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    AnyElement, App, Entity, FontWeight, InteractiveElement as _, IntoElement as _, ParentElement as _, SharedString, StatefulInteractiveElement as _,
    Styled as _, Window, div,
};

use crate::markdown_view::ParseCache;
use crate::search::{Filter, Results, Scope, Search, pages, typing_filter};
use crate::store::Store;
use crate::ui::u;

/// The search results column is at least this wide, whatever the member list's width.
pub const RESULTS_WIDTH: f32 = 464.;

/// One choice in the menu under the box: what it says, and the box's text after picking it.
struct Choice {
    label: String,
    detail: String,
    avatar: Option<String>,
    text: String,
}

/// The box's text with its last word swapped for `filter:value` and a space.
fn with_last_word(text: &str, filter: Filter, value: &str) -> String {
    let keep = text.rfind(char::is_whitespace).map_or("", |at| &text[..=at]);
    let value = if value.contains(' ') { format!("\"{value}\"") } else { value.to_string() };
    format!("{keep}{}:{value} ", filter.key())
}

fn choices(text: &str, scope: Scope, store: &Store) -> (String, Vec<Choice>) {
    let cache = store.cache();
    let Some((filter, typed)) = typing_filter(text) else {
        let options = Filter::ALL
            .into_iter()
            .filter(|f| f.applies(scope.guild.is_some()))
            .map(|f| {
                let spacer = if text.is_empty() || text.ends_with(' ') { "" } else { " " };
                Choice { label: format!("{}:", f.key()), detail: f.hint().to_string(), avatar: None, text: format!("{text}{spacer}{}:", f.key()) }
            })
            .collect();
        return ("Search Options".into(), options);
    };
    let lower = typed.to_lowercase();
    let simple = |values: &[&str]| -> Vec<Choice> {
        values
            .iter()
            .filter(|v| v.starts_with(&lower))
            .map(|v| Choice { label: v.to_string(), detail: String::new(), avatar: None, text: with_last_word(text, filter, v) })
            .collect()
    };
    let list = match filter {
        Filter::From | Filter::Mentions => {
            let mut people: Vec<&cord_core::model::User> = cache.mention_candidates(scope.guild, scope.channel, typed, 8);
            if scope.guild.is_none() {
                let others = cache.channel(scope.channel).into_iter().flat_map(|c| c.recipient_ids.iter()).filter_map(|id| cache.user(*id));
                people.extend(others.chain(cache.me()).filter(|u| u.username.to_lowercase().contains(&lower) || u.display_name().to_lowercase().contains(&lower)));
                let mut seen = HashSet::new();
                people.retain(|u| seen.insert(u.id));
            }
            people
                .into_iter()
                .take(8)
                .map(|u| Choice {
                    label: cache.display_name(scope.guild, u).to_string(),
                    detail: u.username.clone(),
                    avatar: Some(cache.avatar_url(scope.guild, u, 64)),
                    text: with_last_word(text, filter, &u.username),
                })
                .collect()
        }
        Filter::Has => simple(&HAS),
        Filter::Pinned => simple(&["true", "false"]),
        Filter::AuthorType => simple(&["user", "bot", "webhook"]),
        Filter::In => {
            let mut channels: Vec<&cord_core::model::Channel> = scope
                .guild
                .map(|g| cache.guild_channels(g))
                .unwrap_or_default()
                .into_iter()
                .filter(|c| c.kind.is_text_like() && c.kind != cord_core::model::ChannelKind::Thread && cache.can_view(c.id))
                .filter(|c| c.name.as_deref().is_some_and(|n| n.to_lowercase().contains(lower.trim_start_matches('#'))))
                .collect();
            channels.sort_by_key(|c| (c.position, c.id));
            channels
                .into_iter()
                .take(8)
                .map(|c| {
                    let name = c.name.clone().unwrap_or_default();
                    Choice { label: format!("#{name}"), detail: String::new(), avatar: None, text: with_last_word(text, filter, &name) }
                })
                .collect()
        }
        Filter::Before | Filter::During | Filter::After => {
            let today = chrono::Local::now().date_naive();
            [("Today", today), ("Yesterday", today - chrono::Days::new(1)), ("A week ago", today - chrono::Days::new(7))]
                .into_iter()
                .map(|(label, day)| {
                    let day = day.format("%Y-%m-%d").to_string();
                    Choice { label: label.into(), detail: day.clone(), avatar: None, text: with_last_word(text, filter, &day) }
                })
                .collect()
        }
    };
    (format!("{}: {}", filter.key(), filter.hint()), list)
}

/// The search box, with the filter menu under it while it has focus.
pub fn search_box(search: &Entity<Search>, store: &Entity<Store>, scope: Scope, placeholder: String, window: &mut Window, cx: &mut App) -> AnyElement {
    let state = search.read(cx);
    let text = state.input.read(cx).value().to_string();
    let open = state.scope.is_some_and(|s| s.covers(scope.guild, scope.channel));
    // Only while the box really has focus: a change from outside it doesn't count.
    let input = state.input.clone();
    let focused = state.focused && gpui_kit::Focusable::focus_handle(input.read(cx), cx).is_focused(window);
    // Each place says what it searches.
    if state.placeholder != placeholder {
        input.update(cx, |input, cx| input.set_placeholder(placeholder.clone(), window, cx));
        search.update(cx, |search, _| search.placeholder = placeholder);
    }
    let theme = cx.theme();
    let clear = (open || !text.is_empty()).then(|| {
        let search = search.clone();
        div()
            .id("search-clear")
            .cursor_pointer()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                search.update(cx, |search, cx| search.close(window, cx));
            })
            .child(Icon::new(IconName::Close).size(u(16.)))
    });
    let menu = focused.then(|| {
        let (title, list) = choices(&text, scope, store.read(cx));
        let rows = list.into_iter().enumerate().map(|(i, choice)| {
            let (input, chosen) = (input.clone(), choice.text.clone());
            h_flex()
                .id(("search-choice", i))
                .h(u(32.))
                .px(u(8.))
                .gap(u(8.))
                .items_center()
                .rounded(u(4.))
                .cursor_pointer()
                .hover(|d| d.bg(theme.foreground.opacity(0.08)))
                // On press, before the box would lose focus.
                .on_mouse_down(gpui_kit::MouseButton::Left, move |_, window, cx| {
                    cx.stop_propagation();
                    input.update(cx, |input, cx| {
                        input.set_value(chosen.clone(), window, cx);
                        input.focus(window, cx);
                    });
                })
                .when_some(choice.avatar, |d, url| d.child(Avatar::new().border_0().src(url).size(u(20.))))
                .child(div().text_size(u(14.)).font_weight(FontWeight::MEDIUM).child(SharedString::from(choice.label)))
                .child(div().min_w_0().truncate().text_size(u(13.)).text_color(theme.muted_foreground).child(SharedString::from(choice.detail)))
        });
        v_flex()
            .id("search-menu")
            .occlude()
            .absolute()
            .top(u(36.))
            .right_0()
            .w(u(300.))
            .p(u(8.))
            .rounded(u(8.))
            .border_1()
            .border_color(theme.border)
            .bg(theme.popover)
            .shadow_lg()
            .child(div().px(u(8.)).pb(u(6.)).text_size(u(12.)).font_weight(FontWeight::BOLD).text_color(theme.muted_foreground).child(SharedString::from(title.to_uppercase())))
            .children(rows)
    });
    div()
        .relative()
        .w_full()
        .child(
            h_flex()
                .h(u(32.))
                .px(u(8.))
                .gap(u(4.))
                .items_center()
                .rounded(u(6.))
                .bg(theme.popover)
                .border_1()
                .border_color(if focused { theme.ring } else { theme.border })
                .text_size(u(14.))
                .text_color(theme.muted_foreground)
                .child(div().flex_1().min_w_0().child(Input::new(&input).appearance(false).small()))
                .child(match clear {
                    Some(clear) => clear.into_any_element(),
                    None => Icon::new(IconName::Search).size(u(16.)).into_any_element(),
                }),
        )
        .children(menu.map(|m| gpui_kit::deferred(m).with_priority(2)))
        .into_any_element()
}

/// The results, in the member list's place.
pub fn results(search: &Entity<Search>, store: &Entity<Store>, parsed: Rc<RefCell<ParseCache>>, revealed: Rc<RefCell<HashSet<(Id, u16)>>>, text_size: f32, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let state = search.read(cx);
    let Some(scope) = state.scope else { return div().into_any_element() };
    let cache = store.read(cx).cache();
    let sort_label = match state.sort {
        Sort::Newest => "Newest",
        Sort::Oldest => "Oldest",
        Sort::Relevant => "Most Relevant",
    };
    let sort_menu = {
        let search = search.clone();
        move |event: &gpui_kit::ClickEvent, _: &mut Window, cx: &mut App| {
            use crate::context_menu::Entry;
            let entry = |label: &'static str, sort: Sort| {
                let search = search.clone();
                Entry::item(label, move |_, cx| search.update(cx, |search, cx| search.set_sort(sort, cx)))
            };
            crate::context_menu::show(event.position(), vec![entry("Newest", Sort::Newest), entry("Oldest", Sort::Oldest), entry("Most Relevant", Sort::Relevant)], cx);
        }
    };
    let note = |text: String| div().py(u(40.)).px(u(16.)).text_center().text_size(u(14.)).text_color(theme.muted_foreground).child(text);
    let (count, body) = match &state.results {
        Results::Loading => ("Searching…".to_string(), note("Searching…".into()).into_any_element()),
        Results::Failed(error) => ("Search failed".to_string(), note(error.clone()).into_any_element()),
        Results::Done(page) if page.messages.is_empty() => ("No Results".to_string(), note("We searched far and wide. Unfortunately, no results were found.".into()).into_any_element()),
        Results::Done(page) => {
            let channel_of: HashMap<Id, Id> = page.messages.iter().map(|m| (m.id, m.channel_id)).collect();
            let shared = crate::message_card::shared(store, Rc::new(channel_of), parsed, revealed, text_size);
            let mut last_channel = None;
            let mut list = v_flex().gap(u(8.)).p(u(12.));
            for m in &page.messages {
                // Results from a server say where each run of them is from.
                if scope.guild.is_some() && last_channel != Some(m.channel_id) {
                    last_channel = Some(m.channel_id);
                    let name = cache.channel(m.channel_id).map_or_else(|| "unknown".to_string(), |c| crate::nav::channel_label(cache, c));
                    list = list.child(
                        h_flex()
                            .pt(u(4.))
                            .gap(u(4.))
                            .items_center()
                            .text_size(u(14.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(Icon::new(IconName::Hash).size(u(16.)).text_color(theme.muted_foreground))
                            .child(SharedString::from(name)),
                    );
                }
                list = list.child(crate::message_card::card(SharedString::from(format!("result-{}", m.id)), m, scope.guild, cache, &shared, None, cx));
            }
            let (now, of) = pages(state.query.offset, page.total);
            let step = |id: &'static str, label: &'static str, offset: Option<u32>| {
                let search = search.clone();
                div()
                    .id(id)
                    .px(u(12.))
                    .h(u(32.))
                    .flex()
                    .items_center()
                    .rounded(u(4.))
                    .text_size(u(14.))
                    .map(|d| match offset {
                        Some(offset) => d
                            .bg(theme.foreground.opacity(0.08))
                            .cursor_pointer()
                            .hover(|d| d.bg(theme.foreground.opacity(0.14)))
                            .on_click(move |_, _, cx| search.update(cx, |search, cx| search.go_to(offset, cx))),
                        None => d.opacity(0.4),
                    })
                    .child(label)
            };
            let offset = state.query.offset;
            let footer = h_flex()
                .p(u(12.))
                .gap(u(12.))
                .items_center()
                .justify_center()
                .child(step("search-prev", "Back", (offset > 0).then(|| offset.saturating_sub(PAGE))))
                .child(div().text_size(u(14.)).text_color(theme.muted_foreground).child(format!("Page {now} of {of}")))
                .child(step("search-next", "Next", (now < of).then_some(offset + PAGE)));
            let total = page.total;
            let count = if total == 1 { "1 Result".to_string() } else { format!("{total} Results") };
            (count, v_flex().child(list).when(of > 1, |d| d.child(footer)).into_any_element())
        }
    };
    let unknown = (!state.unknown.is_empty()).then(|| {
        div()
            .px(u(16.))
            .py(u(6.))
            .text_size(u(12.))
            .text_color(theme.danger)
            .child(format!("Nothing matched: {}", state.unknown.join(", ")))
    });
    let close = search.clone();
    v_flex()
        .size_full()
        .bg(theme.sidebar)
        .child(
            h_flex()
                .flex_shrink_0()
                .h(u(48.))
                .px(u(16.))
                .gap(u(8.))
                .items_center()
                .border_b_1()
                .border_color(theme.border)
                .child(div().flex_1().text_size(u(16.)).font_weight(FontWeight::SEMIBOLD).child(count))
                .child(
                    h_flex()
                        .id("search-sort")
                        .gap(u(4.))
                        .items_center()
                        .text_size(u(14.))
                        .text_color(theme.muted_foreground)
                        .cursor_pointer()
                        .hover(|d| d.text_color(theme.foreground))
                        .on_click(sort_menu)
                        .child(sort_label)
                        .child(Icon::new(IconName::ChevronDown).size(u(14.))),
                )
                .child(
                    div()
                        .id("search-close")
                        .cursor_pointer()
                        .text_color(theme.muted_foreground)
                        .hover(|d| d.text_color(theme.foreground))
                        .on_click(move |_, window, cx| close.update(cx, |search, cx| search.close(window, cx)))
                        .child(Icon::new(IconName::Close).size(u(18.))),
                ),
        )
        .children(unknown)
        .child(
            div()
                .relative()
                .flex_1()
                .min_h_0()
                .flex()
                .flex_col()
                .child(state.scroller.layer())
                .child(div().id("search-results").flex_1().min_h_0().overflow_y_scroll().track_scroll(&state.scroll).child(body)),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn picking_a_value_replaces_what_was_typed_of_it() {
        assert_eq!(with_last_word("cats from:ki", Filter::From, "kit"), "cats from:kit ");
        assert_eq!(with_last_word("has:", Filter::Has, "image"), "has:image ");
        assert_eq!(with_last_word("in:gen", Filter::In, "off topic"), "in:\"off topic\" ");
    }
}
