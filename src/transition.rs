//! `transition: 256ms ease`, which GPUI doesn't have, for the few things that
//! ease (the rail's pill): how far along is worked out from the time since it
//! changed, starting from wherever it was, so leaving halfway turns straight back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::{IntoElement, SharedString, Styled, canvas};

const SECS: f32 = 0.256;

#[derive(Clone, Copy)]
struct Fade {
    on: bool,
    from: f32,
    changed: Instant,
}

thread_local! {
    static FADES: RefCell<HashMap<SharedString, Fade>> = RefCell::default();
}

/// CSS's `ease`, near enough.
pub fn ease(t: f32) -> f32 {
    let t = t.clamp(0., 1.);
    1. - (1. - t) * (1. - t) * (1. - t)
}

/// Where a fade is, `elapsed` seconds after turning `on` from `from`. 0 is off, 1 is on.
fn amount(from: f32, on: bool, elapsed: f32) -> f32 {
    let to = if on { 1. } else { 0. };
    from + (to - from) * ease(elapsed / SECS)
}

/// How far `id` is towards `on` right now, and whether it's still moving.
pub fn progress(id: &SharedString, on: bool) -> (f32, bool) {
    FADES.with_borrow_mut(|fades| {
        let now = Instant::now();
        let Some(fade) = fades.get_mut(id) else {
            // The first look isn't a change: nothing animates as a thing appears.
            let settled = now.checked_sub(std::time::Duration::from_secs_f32(SECS)).unwrap_or(now);
            fades.insert(id.clone(), Fade { on, from: if on { 1. } else { 0. }, changed: settled });
            return (if on { 1. } else { 0. }, false);
        };
        if fade.on != on {
            let at = amount(fade.from, fade.on, now.duration_since(fade.changed).as_secs_f32());
            *fade = Fade { on, from: at, changed: now };
        }
        let elapsed = now.duration_since(fade.changed).as_secs_f32();
        let done = elapsed >= SECS;
        (amount(fade.from, fade.on, elapsed), !done)
    })
}

/// Keeps frames coming while a fade runs. Takes up no room.
pub fn driver() -> impl IntoElement {
    canvas(|_, _, _| {}, |_, _, window, _| window.request_animation_frame()).absolute().size_0()
}

thread_local! {
    static POINTER: RefCell<Option<SharedString>> = const { RefCell::new(None) };
}

/// Whether the pointer's on the element given this id with [`track_pointer`].
pub fn pointer_on(id: &SharedString) -> bool {
    POINTER.with_borrow(|p| p.as_ref() == Some(id))
}

/// Notes the pointer arriving on or leaving `id`, redrawing nothing.
pub fn set_pointer(id: &SharedString, over: bool) {
    POINTER.with_borrow_mut(|p| {
        if over {
            *p = Some(id.clone());
        } else if p.as_ref() == Some(id) {
            *p = None;
        }
    });
}

/// An `on_hover` listener keeping [`pointer_on`] up to date.
pub fn track_pointer(id: SharedString) -> impl Fn(&bool, &mut gpui_kit::Window, &mut gpui_kit::App) + 'static {
    move |over, window, _| {
        POINTER.with_borrow_mut(|p| {
            if *over {
                *p = Some(id.clone());
            } else if p.as_ref() == Some(&id) {
                *p = None;
            }
        });
        window.refresh();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fade_runs_its_course_and_turns_back_from_where_it_is() {
        assert_eq!(amount(0., true, 0.), 0.);
        assert_eq!(amount(0., true, SECS), 1.);
        assert_eq!(amount(1., false, SECS), 0.);
        let halfway = amount(0., true, SECS / 2.);
        assert!(halfway > 0.5 && halfway < 1.);
        // Leaving partway starts from there, with no jump.
        assert_eq!(amount(halfway, false, 0.), halfway);
    }

    #[test]
    fn appearing_does_not_animate_but_every_change_after_does() {
        let id = SharedString::from("t-appear");
        assert_eq!(progress(&id, false), (0., false));
        assert_eq!(progress(&id, false), (0., false));
        // Hovering after it had fully settled eases in; it doesn't jump to the end.
        let (value, moving) = progress(&id, true);
        assert!(value < 0.1 && moving, "{value}");
    }

    #[test]
    fn turning_back_midway_carries_on_from_there() {
        let id = SharedString::from("t-turn");
        progress(&id, false);
        progress(&id, true);
        std::thread::sleep(std::time::Duration::from_millis(60));
        let (out, _) = progress(&id, true);
        let (back, moving) = progress(&id, false);
        assert!(moving);
        assert!((back - out).abs() < 0.1, "{back} vs {out}");
    }
}
