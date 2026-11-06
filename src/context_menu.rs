//! Right-click menus, drawn over the window by the shell. What a menu holds
//! is up to whoever opens it; entries with no action yet are shown dimmed so
//! the menu keeps Discord's shape without pretending they work.

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, Bounds, ClipboardItem, FontWeight, Global, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, canvas, deferred, div, point, px,
};

use crate::ui::u;

pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

pub enum Entry {
    Item { label: SharedString, icon: Option<IconName>, picture: Option<&'static str>, danger: bool, action: Option<Action>, hint: Option<SharedString> },
    /// Opens more entries beside it while the pointer's on it, as Mute Channel does.
    Submenu { label: SharedString, hint: Option<SharedString>, entries: Rc<Vec<Entry>> },
    Separator,
    /// Drags from silent to double, staying open.
    Slider { label: SharedString, value: Rc<Cell<f32>>, set: Rc<dyn Fn(f32, &mut App)> },
    /// A box to tick. `closes`: what it does takes over, so the menu goes.
    Check { label: SharedString, on: Rc<Cell<bool>>, danger: bool, closes: bool, action: Action },
    /// A row of emoji buttons along the top of a message's menu.
    Reactions(Vec<(String, Action)>),
}

impl Entry {
    pub fn item(label: impl Into<SharedString>, action: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Entry::Item { label: label.into(), icon: None, picture: None, danger: false, action: Some(Rc::new(action)), hint: None }
    }

    pub fn slider(label: impl Into<SharedString>, value: f32, set: impl Fn(f32, &mut App) + 'static) -> Self {
        Entry::Slider { label: label.into(), value: Rc::new(Cell::new(value)), set: Rc::new(set) }
    }

    /// `asks`: it opens a confirmation rather than happening here and now.
    pub fn check(label: impl Into<SharedString>, on: bool, asks: bool, action: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        Entry::Check { label: label.into(), on: Rc::new(Cell::new(on)), danger: asks, closes: asks, action: Rc::new(action) }
    }

    /// Discord has it, rustcord doesn't do it yet.
    pub fn later(label: impl Into<SharedString>) -> Self {
        Entry::Item { label: label.into(), icon: None, picture: None, danger: false, action: None, hint: None }
    }

    /// A row opening `entries` beside it.
    pub fn submenu(label: impl Into<SharedString>, mut entries: Vec<Entry>) -> Self {
        tidy(&mut entries);
        Entry::Submenu { label: label.into(), hint: None, entries: Rc::new(entries) }
    }

    pub fn copy(label: impl Into<SharedString>, text: String) -> Self {
        Self::item(label, move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
    }

    /// An icon from our own files, as Discord draws it.
    pub fn picture(mut self, path: &'static str) -> Self {
        if let Entry::Item { picture, .. } = &mut self {
            *picture = Some(path);
        }
        self
    }

    pub fn icon(mut self, name: IconName) -> Self {
        if let Entry::Item { icon, .. } = &mut self {
            *icon = Some(name);
        }
        self
    }

    /// A line in small type under the label.
    pub fn hint(mut self, text: impl Into<SharedString>) -> Self {
        if let Entry::Item { hint, .. } | Entry::Submenu { hint, .. } = &mut self {
            *hint = Some(text.into());
        }
        self
    }

    pub fn danger(mut self) -> Self {
        if let Entry::Item { danger, .. } = &mut self {
            *danger = true;
        }
        self
    }
}

struct Open {
    position: Point<Pixels>,
    entries: Rc<Vec<Entry>>,
    /// The submenu row last pointed at, whose entries show beside it.
    sub: Rc<Cell<Option<usize>>>,
    /// Where each submenu row was painted, and where its open submenu is.
    spots: Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>,
    sub_spot: Rc<Cell<Option<Bounds<Pixels>>>>,
}

impl Global for Open {}

/// Drops separators that have nothing on one side of them, so menus can be put together
/// from optional groups without checking each.
fn tidy(entries: &mut Vec<Entry>) {
    let mut kept: Vec<Entry> = Vec::with_capacity(entries.len());
    for entry in entries.drain(..) {
        let separator = matches!(entry, Entry::Separator);
        if separator && kept.last().is_none_or(|last| matches!(last, Entry::Separator)) {
            continue;
        }
        kept.push(entry);
    }
    if matches!(kept.last(), Some(Entry::Separator)) {
        kept.pop();
    }
    *entries = kept;
}

pub fn show(position: Point<Pixels>, mut entries: Vec<Entry>, cx: &mut App) {
    tidy(&mut entries);
    cx.set_global(Open { position, entries: Rc::new(entries), sub: Rc::default(), spots: Rc::default(), sub_spot: Rc::default() });
    cx.refresh_windows();
}

/// The most the volume slider goes up to: 200%, as Discord's does.
const MAX_GAIN: f32 = 2.;

fn change_level(value: &Cell<f32>, set: &Rc<dyn Fn(f32, &mut App)>, level: f32, cx: &mut App) {
    // Every pointer move lands here: only a change is worth anything.
    if (value.get() - level).abs() < 0.001 {
        return;
    }
    value.set(level);
    set(level, cx);
    crate::ui::redraw_shell(cx);
}

pub fn close(cx: &mut App) {
    if cx.has_global::<Open>() {
        cx.remove_global::<Open>();
        cx.refresh_windows();
    }
}

pub fn is_open(cx: &App) -> bool {
    cx.has_global::<Open>()
}

/// Notes where `spot`'s element is each time it's painted. Takes up no room.
fn note_spot(spot: impl Fn(Bounds<Pixels>) + 'static) -> impl IntoElement {
    canvas(move |bounds, _, _| spot(bounds), |_, _, _, _| {}).absolute().top_0().left_0().size_full()
}

/// What sits under the pointer's arrival on a row: `Some` for a submenu row, opening it.
type OnRow = Option<Rc<dyn Fn(Option<usize>, &mut App)>>;

/// The rows of `entries`, their ids from `base`. `on_row` hears which row the pointer's on.
fn entry_rows(entries: &[Entry], base: usize, on_row: OnRow, spots: &Rc<RefCell<HashMap<usize, Bounds<Pixels>>>>, cx: &App) -> Vec<AnyElement> {
    let theme = cx.theme();
    let (fg, muted, danger, hover) = (theme.popover_foreground, theme.muted_foreground, theme.danger, theme.foreground.opacity(0.08));
    let pointed = |i: usize, opens: bool| {
        let on_row = on_row.clone();
        move |over: &bool, _: &mut Window, cx: &mut App| {
            if let (true, Some(on_row)) = (*over, on_row.as_ref()) {
                on_row(opens.then_some(i), cx);
            }
        }
    };
    entries.iter().enumerate().map(|(i, entry)| { let i = base + i; match entry {
        Entry::Separator => div().mx(u(8.)).my(u(4.)).h(u(1.)).bg(theme.border).into_any_element(),
        Entry::Slider { label, value, set } => {
            let level = value.get();
            let (value, set) = (value.clone(), set.clone());
            let change: Rc<dyn Fn(f32, &mut App)> = Rc::new(move |at, cx| change_level(&value, &set, at * MAX_GAIN, cx));
            v_flex()
                .px(u(8.))
                .py(u(6.))
                .gap(u(6.))
                .child(
                    h_flex()
                        .justify_between()
                        .text_size(u(14.))
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(fg)
                        .child(label.clone())
                        .child(div().text_color(muted).child(format!("{}%", (level * 100.).round() as u32))),
                )
                .child(div().w_full().h(u(16.)).px(u(6.)).child(crate::slider::slider(format!("menu-slider-{i}"), level / MAX_GAIN, false, gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)), muted.opacity(0.3), change)))
                .into_any_element()
        }
        Entry::Check { label, on, danger: red, closes, action } => {
            let (action, flag, closes, ticked) = (action.clone(), on.clone(), *closes, on.get());
            h_flex()
                .id(("menu-check", i))
                .h(u(32.))
                .px(u(8.))
                .gap(u(8.))
                .items_center()
                .rounded(u(4.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if *red { danger } else { fg })
                .cursor_pointer()
                .hover(move |d| d.bg(hover))
                .on_click(move |_, window, cx| {
                    if closes {
                        close(cx);
                    } else {
                        flag.set(!flag.get());
                        crate::ui::redraw_shell(cx);
                    }
                    action(window, cx);
                })
                .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                .child(
                    div()
                        .size(u(18.))
                        .flex_shrink_0()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(u(4.))
                        .border_1()
                        .border_color(if ticked { gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)) } else { muted })
                        .when(ticked, |d| d.bg(gpui_kit::rgb(0x5865f2)).child(Icon::new(IconName::Check).size(u(14.)).text_color(gpui_kit::white()))),
                )
                .into_any_element()
        }
        Entry::Reactions(list) => h_flex()
            .p(u(4.))
            .gap(u(6.))
            .children(list.iter().enumerate().map(|(n, (emoji, action))| {
                let action = action.clone();
                div()
                    .id(("menu-react", i * 8 + n))
                    .size(u(40.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(u(8.))
                    .bg(hover)
                    .cursor_pointer()
                    .hover(|d| d.opacity(0.8))
                    .on_click(move |_, window, cx| {
                        close(cx);
                        action(window, cx);
                    })
                    .child(crate::badges::emoji_image(emoji, 24.))
            }))
            .into_any_element(),
        Entry::Submenu { label, hint, entries: _ } => {
            let spots = spots.clone();
            h_flex()
                .id(("menu-sub", i))
                .relative()
                .when(hint.is_none(), |d| d.h(u(32.)))
                .when(hint.is_some(), |d| d.min_h(u(32.)).py(u(4.)))
                .px(u(8.))
                .gap(u(8.))
                .items_center()
                .rounded(u(4.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(fg)
                .cursor_pointer()
                .hover(move |d| d.bg(hover))
                .on_hover(pointed(i, true))
                .child(note_spot(move |bounds| {
                    spots.borrow_mut().insert(i, bounds);
                }))
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(label.clone()))
                        .children(hint.clone().map(|hint| div().truncate().text_size(u(12.)).font_weight(FontWeight::NORMAL).text_color(muted).child(hint))),
                )
                .child(Icon::new(IconName::ChevronRight).size(u(16.)).flex_shrink_0().text_color(muted))
                .into_any_element()
        }
        Entry::Item { label, icon, picture, danger: red, action, hint } => {
            let enabled = action.is_some();
            let action = action.clone();
            let color = if *red { danger } else { fg };
            h_flex()
                .id(("menu-item", i))
                .on_hover(pointed(i, false))
                .when(hint.is_none(), |d| d.h(u(32.)))
                .when(hint.is_some(), |d| d.min_h(u(32.)).py(u(4.)))
                .px(u(8.))
                .gap(u(8.))
                .items_center()
                .rounded(u(4.))
                .text_size(u(14.))
                .font_weight(FontWeight::MEDIUM)
                .text_color(if enabled { color } else { muted.opacity(0.55) })
                .when(enabled, |d| d.cursor_pointer().hover(move |d| d.bg(hover)))
                .when_some(action, |d, action| {
                    d.on_click(move |_, window, cx| {
                        close(cx);
                        action(window, cx);
                    })
                })
                .when_some(*picture, |d, path| d.child(Icon::default().path(path).size(u(18.)).flex_shrink_0()))
                .when_some(icon.clone(), |d, icon| d.child(Icon::new(icon).size(u(18.)).flex_shrink_0()))
                .child(match hint {
                    Some(hint) => v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().child(label.clone()))
                        .child(div().truncate().text_size(u(12.)).font_weight(FontWeight::NORMAL).text_color(muted).child(hint.clone()))
                        .into_any_element(),
                    None => div().flex_1().min_w_0().truncate().child(label.clone()).into_any_element(),
                })
                .into_any_element()
        }
    }})
    .collect()
}

/// Where a submenu opens for a row at `row`: just right of it, its first entry level with it.
fn submenu_at(row: Bounds<Pixels>) -> Point<Pixels> {
    point(row.right() + px(12.), row.top() - px(8.))
}

/// The open menu, or nothing.
pub fn overlay(cx: &App) -> Option<AnyElement> {
    let open = cx.try_global::<Open>()?;
    let theme = cx.theme();
    let sub = open.sub.clone();
    // Pointing at a row opens its submenu, or shuts the one open.
    let on_row: OnRow = Some(Rc::new(move |row: Option<usize>, cx: &mut App| {
        if sub.get() != row {
            sub.set(row);
            crate::ui::redraw_shell(cx);
        }
    }));
    let rows = entry_rows(&open.entries, 0, on_row, &open.spots, cx);
    let panel = |id: &'static str| v_flex().id(id).occlude().w(u(220.)).p(u(8.)).rounded(u(8.)).border_1().border_color(theme.border).bg(theme.popover).shadow_lg();
    let sub_spot = open.sub_spot.clone();
    let menu = panel("context-menu")
        .on_mouse_down_out(move |event, _, cx| {
            // A click in the submenu is still in the menu.
            if !sub_spot.get().is_some_and(|b| b.contains(&event.position)) {
                close(cx);
            }
        })
        .children(rows);
    let submenu = open.sub.get().and_then(|i| {
        let Entry::Submenu { entries, .. } = open.entries.get(i)? else { return None };
        let row = *open.spots.borrow().get(&i)?;
        let spot = open.sub_spot.clone();
        let list = panel("context-submenu").relative().child(note_spot(move |bounds| spot.set(Some(bounds)))).children(entry_rows(entries, 1000, None, &open.spots, cx));
        Some(deferred(anchored().position(submenu_at(row)).snap_to_window_with_margin(px(8.)).child(list)).with_priority(3))
    });
    if submenu.is_none() {
        open.sub_spot.set(None);
    }
    Some(
        div()
            .child(deferred(anchored().position(open.position).snap_to_window_with_margin(px(8.)).child(menu)).with_priority(2))
            .children(submenu)
            .into_any_element(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_submenu_opens_beside_its_row() {
        let row = Bounds { origin: point(px(100.), px(50.)), size: gpui_kit::size(px(204.), px(32.)) };
        assert_eq!(submenu_at(row), point(px(316.), px(42.)));
        let Entry::Submenu { entries, hint, .. } = Entry::submenu("Mute", vec![Entry::Separator, Entry::later("a")]).hint("Muted") else { panic!() };
        assert_eq!(entries.len(), 1, "tidied like the menu");
        assert_eq!(hint.as_ref().map(|h| h.as_ref()), Some("Muted"));
    }

    #[test]
    fn separators_only_stand_between_entries() {
        let mut entries = vec![Entry::Separator, Entry::later("a"), Entry::Separator, Entry::Separator, Entry::later("b"), Entry::Separator];
        tidy(&mut entries);
        let shape: Vec<bool> = entries.iter().map(|e| matches!(e, Entry::Separator)).collect();
        assert_eq!(shape, [false, true, false]);
    }
}
