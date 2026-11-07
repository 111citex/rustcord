//! Layout sizes. Every size in the UI is written as Discord's pixel size at
//! 100% zoom and goes through [`u`], which turns it into rems: the window's
//! rem follows Settings › Zoom, so the whole layout (not just text) scales.

use gpui_kit::{Rems, rems};

/// GPUI's rem at 100% zoom.
const BASE_REM: f32 = 16.;

/// A Discord 100%-zoom pixel size, scaled by the user's zoom.
pub fn u(px: f32) -> Rems {
    rems(px / BASE_REM)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_sizes_become_rems() {
        assert_eq!(u(16.), rems(1.));
        assert_eq!(u(48.), rems(3.));
        assert_eq!(u(0.), rems(0.));
    }
}

thread_local! {
    static CHAT_VIEW: std::cell::Cell<Option<gpui_kit::EntityId>> = const { std::cell::Cell::new(None) };
}

/// The chat page says it's the one on screen.
pub fn note_chat_view(view: gpui_kit::EntityId) {
    CHAT_VIEW.set(Some(view));
}

/// Redraws the chat alone. A window refresh redraws everything, the channel and
/// member lists included: too much to do while scrolling past things that load.
pub fn redraw_chat(cx: &mut gpui_kit::App) {
    match CHAT_VIEW.get() {
        Some(view) => cx.notify(view),
        None => cx.refresh_windows(),
    }
}

thread_local! {
    static SHIFT: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether Shift is held right now.
pub fn shift_held() -> bool {
    SHIFT.get()
}

/// Notes a change of the modifier keys: messages show more with Shift held.
pub fn note_shift(held: bool, cx: &mut gpui_kit::App) {
    if SHIFT.replace(held) != held {
        redraw_chat(cx);
    }
}

thread_local! {
    static SHELL_VIEW: std::cell::Cell<Option<gpui_kit::EntityId>> = const { std::cell::Cell::new(None) };
}

/// Notes the view that draws the window's overlays.
pub fn note_shell_view(view: gpui_kit::EntityId) {
    SHELL_VIEW.set(Some(view));
}

thread_local! {
    static OVERLAY_VIEW: std::cell::Cell<Option<gpui_kit::EntityId>> = const { std::cell::Cell::new(None) };
}

/// Notes the view the floating panels are drawn in.
pub fn note_overlay_view(view: gpui_kit::EntityId) {
    OVERLAY_VIEW.set(Some(view));
}

/// Redraws the floating panels alone: the ringing panel, the video window, a live card.
pub fn redraw_overlays(cx: &mut gpui_kit::App) {
    match OVERLAY_VIEW.get() {
        Some(view) => cx.notify(view),
        None => redraw_shell(cx),
    }
}

/// Redraws the overlays alone: not everything under them, as a window refresh would.
pub fn redraw_shell(cx: &mut gpui_kit::App) {
    match SHELL_VIEW.get() {
        Some(view) => cx.notify(view),
        None => cx.refresh_windows(),
    }
}
