//! `transition: 256ms ease`, which GPUI doesn't have, for the few things that
//! ease (the rail's pill): how far along is worked out from the time since it
//! changed, starting from wherever it was, so leaving halfway turns straight back.

use std::cell::RefCell;
use std::collections::HashMap;
use std::time::Instant;

use gpui_kit::{AnimationElement, AnimationExt as _, App, Global, IntoElement, SharedString, Styled, canvas};

const SECS: f32 = 0.256;

/// How long a panel or popout takes to come and go: quick, as Discord's are.
pub const POP: std::time::Duration = std::time::Duration::from_millis(160);
/// How far a panel or popout rises as it comes in, in pixels.
const RISE: f32 = 8.;

/// One opening of a panel or popout: its animations are keyed by `number`, so each opening
/// starts them fresh, and `closing` once it's on its way out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Showing {
    pub number: usize,
    pub closing: bool,
}

impl Showing {
    pub fn new() -> Self {
        static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        Showing { number: NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed), closing: false }
    }

    fn key(self) -> usize {
        self.number * 2 + usize::from(self.closing)
    }
}

impl Default for Showing {
    fn default() -> Self {
        Self::new()
    }
}

/// How visible a panel is `t` of the way through its coming in or going out.
pub fn shown(closing: bool, t: f32) -> f32 {
    if closing { 1. - t } else { t }
}

/// A panel or popout card coming in or going: it fades as it rises into place, and sinks a
/// little as it fades out. The element must be `relative`, for the rise.
pub fn pop<E: IntoElement + Styled + 'static>(element: E, id: &'static str, showing: Showing) -> AnimationElement<E> {
    element.with_animation((id, showing.key()), gpui_kit::Animation::new(POP).with_easing(gpui_kit::ease_out_quint()), move |d, t| {
        let shown = shown(showing.closing, t);
        d.opacity(shown).top(crate::ui::u((1. - shown) * RISE))
    })
}

/// What's behind a panel, dimmed: it only fades.
pub fn fade<E: IntoElement + Styled + 'static>(element: E, id: &'static str, showing: Showing) -> AnimationElement<E> {
    element.with_animation((id, showing.key()), gpui_kit::Animation::new(POP).with_easing(gpui_kit::ease_out_quint()), move |d, t| d.opacity(shown(showing.closing, t)))
}

/// Starts `G`'s panel going, and drops it once it has faded out. `showing` finds the
/// opening in it. A panel opened again meanwhile is a new opening, and stays.
pub fn leave<G: Global>(cx: &mut App, showing: fn(&mut G) -> &mut Showing) {
    if !cx.has_global::<G>() {
        return;
    }
    let now = showing(cx.global_mut::<G>());
    if now.closing {
        return;
    }
    now.closing = true;
    let number = now.number;
    cx.refresh_windows();
    cx.spawn(async move |cx| {
        cx.background_executor().timer(POP).await;
        cx.update(|cx| {
            if cx.has_global::<G>() && showing(cx.global_mut::<G>()).number == number {
                cx.remove_global::<G>();
                cx.refresh_windows();
            }
        });
    })
    .detach();
}

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
    fn panels_fade_in_and_out_and_each_opening_is_its_own() {
        assert_eq!((shown(false, 0.), shown(false, 1.)), (0., 1.));
        assert_eq!((shown(true, 0.), shown(true, 1.)), (1., 0.));
        let (one, two) = (Showing::new(), Showing::new());
        assert_ne!(one.key(), two.key());
        assert_ne!(one.key(), Showing { closing: true, ..one }.key(), "closing animates afresh");
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
