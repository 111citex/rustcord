//! Right-click menus, drawn over the window by the shell. What a menu holds
//! is up to whoever opens it; entries with no action yet are shown dimmed so
//! the menu keeps Discord's shape without pretending they work.

use std::cell::Cell;
use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, ClipboardItem, FontWeight, Global, InteractiveElement, IntoElement, ParentElement, Pixels, Point,
    SharedString, StatefulInteractiveElement, Styled, Window, anchored, deferred, div,
};

use crate::ui::u;

pub type Action = Rc<dyn Fn(&mut Window, &mut App)>;

pub enum Entry {
    Item { label: SharedString, icon: Option<IconName>, danger: bool, action: Option<Action> },
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
        Entry::Item { label: label.into(), icon: None, danger: false, action: Some(Rc::new(action)) }
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
        Entry::Item { label: label.into(), icon: None, danger: false, action: None }
    }

    pub fn copy(label: impl Into<SharedString>, text: String) -> Self {
        Self::item(label, move |_, cx| cx.write_to_clipboard(ClipboardItem::new_string(text.clone())))
    }

    pub fn icon(mut self, name: IconName) -> Self {
        if let Entry::Item { icon, .. } = &mut self {
            *icon = Some(name);
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
}

impl Global for Open {}

pub fn show(position: Point<Pixels>, entries: Vec<Entry>, cx: &mut App) {
    cx.set_global(Open { position, entries: Rc::new(entries) });
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

/// The open menu, or nothing.
pub fn overlay(cx: &App) -> Option<AnyElement> {
    let open = cx.try_global::<Open>()?;
    let theme = cx.theme();
    let (fg, muted, danger, hover) = (theme.popover_foreground, theme.muted_foreground, theme.danger, theme.foreground.opacity(0.08));
    let rows = open.entries.iter().enumerate().map(|(i, entry)| match entry {
        Entry::Separator => div().mx(u(8.)).my(u(4.)).h(u(1.)).bg(theme.border).into_any_element(),
        Entry::Slider { label, value, set } => {
            const CELLS: usize = 40;
            let level = value.get();
            let cells = (0..CELLS).map(|n| {
                let at = (n + 1) as f32 / CELLS as f32 * MAX_GAIN;
                let (down_value, down_set, move_value, move_set) = (value.clone(), set.clone(), value.clone(), set.clone());
                div()
                    .id(("menu-cell", i * CELLS + n))
                    .flex_1()
                    .h(u(16.))
                    .flex()
                    .items_center()
                    .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
                        cx.stop_propagation();
                        change_level(&down_value, &down_set, at, cx);
                    })
                    .on_mouse_move(move |event: &gpui_kit::MouseMoveEvent, _, cx| {
                        if event.pressed_button == Some(gpui_kit::MouseButton::Left) {
                            change_level(&move_value, &move_set, at, cx);
                        }
                    })
                    .child(div().w_full().h(u(6.)).bg(if at <= level + 0.001 { gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2)) } else { muted.opacity(0.3) }))
            });
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
                .child(h_flex().w_full().items_center().children(cells))
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
        Entry::Item { label, icon, danger: red, action } => {
            let enabled = action.is_some();
            let action = action.clone();
            let color = if *red { danger } else { fg };
            h_flex()
                .id(("menu-item", i))
                .h(u(32.))
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
                .when_some(icon.clone(), |d, icon| d.child(Icon::new(icon).size(u(18.)).flex_shrink_0()))
                .child(div().flex_1().min_w_0().truncate().child(label.clone()))
                .into_any_element()
        }
    });
    let menu = v_flex()
        .id("context-menu")
        .occlude()
        .w(u(220.))
        .p(u(8.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.popover)
        .shadow_lg()
        .on_mouse_down_out(|_, _, cx| close(cx))
        .children(rows);
    Some(
        deferred(anchored().position(open.position).snap_to_window_with_margin(gpui_kit::px(8.)).child(menu))
            .with_priority(2)
            .into_any_element(),
    )
}
