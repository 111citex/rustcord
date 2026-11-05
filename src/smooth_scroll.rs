//! Smooth mouse-wheel scrolling. GPUI moves a scroll view the whole notch at
//! once; here wheel notches set a target and the view glides to it, at
//! whatever rate the display refreshes, in about the time a browser takes. Trackpads already send
//! fine-grained pixel deltas with the OS's own momentum, so they are left to
//! GPUI untouched.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use gpui_kit::{
    DispatchPhase, IntoElement, ListState, ScrollDelta, ScrollHandle, ScrollWheelEvent, Styled, canvas, point, px,
};

/// Pixels per wheel line.
const LINE_PX: f32 = 50.;
/// Assumed length of the first frame of a glide.
const FIRST_FRAME: Duration = Duration::from_micros(8_333);

/// Chromium's wheel scrolling (`cc/animation/scroll_offset_animation_curve.cc`):
/// an ease-in-out Bézier over 100 to 200 ms, shorter the further there is to go.
/// A notch mid-scroll starts a new curve from the current position, whose first
/// control point is raised so that it begins at the current speed. So there's no
/// seam, and only the first notch has a slow start.
#[derive(Debug, Default)]
pub struct Glide {
    distance: f32,
    /// The curve's length in seconds, and its first control point's y.
    duration: f32,
    y1: f32,
    elapsed: f32,
    /// How far along the curve we've reported moving.
    done: f32,
    active: bool,
}

const X1: f32 = 0.42;
const X2: f32 = 0.58;

/// Chromium's duration for a distance, in units of 1/60 s.
fn duration_for(distance: f32) -> f32 {
    (14. - distance.abs() / 60.).clamp(6., 12.) / 60.
}

fn bezier(a: f32, b: f32, u: f32) -> f32 {
    let v = 1. - u;
    3. * a * u * v * v + 3. * b * u * u * v + u * u * u
}

fn bezier_slope(a: f32, b: f32, u: f32) -> f32 {
    let v = 1. - u;
    3. * a * v * v + 6. * (b - a) * u * v + 3. * (1. - b) * u * u
}

/// The curve's parameter where its time is `tau`.
fn solve(tau: f32) -> f32 {
    let (mut low, mut high) = (0., 1.);
    for _ in 0..24 {
        let u = (low + high) / 2.;
        if bezier(X1, X2, u) < tau {
            low = u;
        } else {
            high = u;
        }
    }
    (low + high) / 2.
}

/// How far along a curve is at `tau` of its time, 0 to 1.
fn eased(y1: f32, tau: f32) -> f32 {
    bezier(y1, 1., solve(tau.clamp(0., 1.)))
}

impl Glide {
    /// Pixels a second right now.
    fn speed(&self) -> f32 {
        if !self.active || self.duration <= 0. {
            return 0.;
        }
        let u = solve((self.elapsed / self.duration).clamp(0., 1.));
        let slope = bezier_slope(self.y1, 1., u) / bezier_slope(X1, X2, u).max(1e-4);
        slope * self.distance / self.duration
    }

    pub fn add(&mut self, pixels: f32) {
        let speed = self.speed();
        let remaining = if self.active { self.distance - self.done } else { 0. } + pixels;
        if remaining.abs() < 0.5 && speed.abs() < 30. {
            *self = Glide::default();
            return;
        }
        let mut duration = duration_for(remaining);
        let mut y1 = 0.;
        // Heading the same way: pick up at the current speed. Against it, from rest.
        if speed * remaining > 0. {
            duration = duration.min(2.5 * remaining / speed);
            y1 = (X1 * speed * duration / remaining).clamp(0., 1.);
        }
        *self = Glide { distance: remaining, duration, y1, elapsed: 0., done: 0., active: true };
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Pixels to move this frame, `dt` after the last one.
    pub fn step(&mut self, dt: Duration) -> f32 {
        if !self.active {
            return 0.;
        }
        self.elapsed += dt.as_secs_f32();
        if self.elapsed >= self.duration {
            let moved = self.distance - self.done;
            *self = Glide::default();
            return moved;
        }
        let now = self.distance * eased(self.y1, self.elapsed / self.duration);
        let moved = now - self.done;
        self.done = now;
        moved
    }
}

/// The shortest frame seen lately: the display's refresh, near enough.
#[derive(Debug, Default)]
struct FrameClock {
    period: f32,
}

impl FrameClock {
    /// `measured` seconds as a whole number of refreshes. A frame's content is
    /// shown at a refresh, not when it was drawn, so that's the time that passed;
    /// going by the clock makes steps alternate long and short.
    fn tick(&mut self, measured: f32) -> f32 {
        const LOWEST_REFRESH: f32 = 1. / 30.;
        self.period = if self.period == 0. { measured.min(LOWEST_REFRESH) } else { measured.min(self.period * 1.002) };
        if self.period <= 0. {
            return measured;
        }
        (measured / self.period).round().max(1.) * self.period
    }
}

/// What a [`SmoothScroller`] moves.
#[derive(Clone)]
pub enum ScrollTarget {
    List(ListState),
    Handle(ScrollHandle),
}

impl ScrollTarget {
    /// Whether there's anywhere to go that way (`dy` above zero is down).
    fn can_move(&self, dy: f32) -> bool {
        let (top, max) = match self {
            ScrollTarget::List(list) => {
                let max = f32::from(list.max_offset_for_scrollbar().y);
                (f32::from(-list.scroll_px_offset_for_scrollbar().y).min(max), max)
            }
            ScrollTarget::Handle(handle) => (f32::from(-handle.offset().y), f32::from(handle.max_offset().y)),
        };
        room_to_move(top, max, dy)
    }

    /// Moves the content `dy` pixels toward its end, clamped to its range.
    fn scroll_by(&self, dy: f32) {
        match self {
            // The scrollbar entry point clamps, stops following the tail when
            // moving up, and resumes following when it reaches the bottom.
            ScrollTarget::List(list) => {
                // Following the newest, the list can report its place as past the real
                // end: moving up from there would land back at the end, and stick.
                let top = (-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y);
                list.set_offset_from_scrollbar(point(px(0.), -(top + px(dy))));
            }
            ScrollTarget::Handle(handle) => {
                let mut offset = handle.offset();
                let max = handle.max_offset().y.max(px(0.));
                offset.y = (offset.y - px(dy)).clamp(-max, px(0.));
                handle.set_offset(offset);
            }
        }
    }
}

fn room_to_move(top: f32, max: f32, dy: f32) -> bool {
    if dy > 0. { top < max - 0.5 } else { dy < 0. && top > 0.5 }
}

#[derive(Default)]
struct State {
    glide: Glide,
    last_frame: Option<Instant>,
    clock: FrameClock,
    /// Movement not yet shown, being less than a device pixel.
    carry: f32,
}

/// Splits movement into what lands on whole device pixels and what's left over.
/// Text snaps to the pixel grid while pictures don't, so a fractional offset
/// makes them shimmer against each other.
fn whole_pixels(wanted: f32, scale: f32) -> (f32, f32) {
    let whole = (wanted * scale).round() / scale;
    (whole, wanted - whole)
}

/// Put [`SmoothScroller::layer`] over a scroll view (in a `relative` parent)
/// to make its wheel scrolling smooth.
#[derive(Clone)]
pub struct SmoothScroller {
    target: ScrollTarget,
    state: Rc<RefCell<State>>,
    /// Lives inside a full profile, so that being open doesn't silence it.
    in_modal: bool,
    /// A list whose rows do something when hovered: not while it's moving under the pointer.
    calm_hover: bool,
}

/// How long after the last movement a list counts as still scrolling.
const SETTLE: Duration = Duration::from_millis(180);

/// What the row left under the pointer wants done, and whose it is.
type AfterScrolling = (gpui_kit::SharedString, Box<dyn FnOnce(&mut gpui_kit::App)>);

thread_local! {
    static SCROLLING_UNTIL: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static AFTER_SCROLLING: RefCell<Option<AfterScrolling>> = const { RefCell::new(None) };
}

/// Whether a list with hover effects is moving under the pointer. Rows pass by far too
/// fast for those to mean anything, and each costs a redraw and maybe a download.
pub fn list_scrolling() -> bool {
    SCROLLING_UNTIL.get().is_some_and(|until| Instant::now() < until)
}

/// Does `action` once the list stops, if the pointer's still on row `id` by then.
pub fn after_scrolling(id: gpui_kit::SharedString, action: impl FnOnce(&mut gpui_kit::App) + 'static) {
    AFTER_SCROLLING.set(Some((id, Box::new(action))));
}

/// The pointer's left row `id`: whatever it wanted is off.
pub fn cancel_after_scrolling(id: &gpui_kit::SharedString) {
    AFTER_SCROLLING.with_borrow_mut(|after| {
        if after.as_ref().is_some_and(|(row, _)| row == id) {
            *after = None;
        }
    });
}

impl SmoothScroller {
    pub fn new(target: ScrollTarget) -> Self {
        SmoothScroller { target, state: Rc::default(), in_modal: false, calm_hover: false }
    }

    pub fn calm_hover(mut self) -> Self {
        self.calm_hover = true;
        self
    }

    pub fn in_modal(mut self) -> Self {
        self.in_modal = true;
        self
    }

    /// An invisible layer covering the scroll view: it takes wheel notches
    /// over its bounds and advances the glide each frame.
    /// Starts gliding `pixels` down the content (up, if negative).
    pub fn glide_by(&self, pixels: f32) {
        self.state.borrow_mut().glide.add(pixels);
    }

    pub fn is_gliding(&self) -> bool {
        self.state.borrow().glide.is_active()
    }

    /// Drops any glide under way: the view has been put somewhere else.
    pub fn stop(&self) {
        self.state.borrow_mut().glide = Glide::default();
    }

    pub fn layer(&self) -> impl IntoElement {
        let this = self.clone();
        let stepper = this.clone();
        canvas(
            // Moves before painting: put ahead of the scroll view, the frame shows
            // this step instead of the last one.
            move |_, window, cx| {
                if stepper.calm_hover {
                    if stepper.state.borrow().glide.is_active() {
                        SCROLLING_UNTIL.set(Some(Instant::now() + SETTLE));
                    } else if let Some(until) = SCROLLING_UNTIL.get() {
                        if Instant::now() < until {
                            window.request_animation_frame();
                        } else {
                            SCROLLING_UNTIL.set(None);
                            let after = AFTER_SCROLLING.take();
                            cx.defer(move |cx| {
                                if let Some((_, action)) = after {
                                    action(cx);
                                }
                            });
                            // Once more with hover back on.
                            window.request_animation_frame();
                        }
                    }
                }
                let mut state = stepper.state.borrow_mut();
                if state.glide.is_active() {
                    let now = Instant::now();
                    let measured = state.last_frame.map_or(FIRST_FRAME, |last| now - last).as_secs_f32();
                    state.last_frame = Some(now);
                    let frame = state.clock.tick(measured);
                    let moved = state.glide.step(Duration::from_secs_f32(frame)) + state.carry;
                    let (whole, rest) = whole_pixels(moved, window.scale_factor());
                    state.carry = if state.glide.is_active() { rest } else { 0. };
                    if whole != 0. {
                        stepper.target.scroll_by(whole);
                    }
                    window.request_animation_frame();
                } else {
                    state.last_frame = None;
                    state.carry = 0.;
                }
            },
            move |bounds, _, window, _| {
                let this = this.clone();
                let view = window.current_view();
                window.on_mouse_event(move |event: &ScrollWheelEvent, phase, _, cx| {
                    if phase != DispatchPhase::Capture || !bounds.contains(&event.position) {
                        return;
                    }
                    // The picture viewer or a full profile is over everything: the wheel is theirs.
                    if crate::viewer::is_open(cx) || crate::confirm::is_open(cx) || ((crate::add_people::is_open(cx) || crate::profile_card::full_open()) && !this.in_modal) {
                        return;
                    }
                    let ScrollDelta::Lines(lines) = event.delta else { return };
                    cx.stop_propagation();
                    // Only actual scrolling moves a card out of the way.
                    if this.target.can_move(-lines.y) {
                        crate::profile_card::note_wheel(lines.y.abs(), cx);
                    }
                    if this.calm_hover {
                        SCROLLING_UNTIL.set(Some(Instant::now() + SETTLE));
                    }
                    this.state.borrow_mut().glide.add(-lines.y * LINE_PX);
                    // This view alone: a window refresh redraws the other lists too.
                    cx.notify(view);
                });
            },
        )
        .absolute()
        .top_0()
        .left_0()
        .size_full()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FRAME: Duration = Duration::from_micros(8_333); // 120 Hz

    fn run(glide: &mut Glide) -> (f32, usize) {
        let (mut moved, mut frames) = (0.0, 0);
        while glide.is_active() {
            moved += glide.step(FRAME);
            frames += 1;
            assert!(frames < 1_000, "never settled");
        }
        (moved, frames)
    }

    #[test]
    fn frames_count_in_whole_refreshes() {
        let mut clock = FrameClock::default();
        let refresh = 1. / 180.;
        clock.tick(refresh);
        // A frame drawn a little late or early still shows one refresh later.
        assert!((clock.tick(refresh * 1.2) - refresh).abs() < refresh * 0.01);
        assert!((clock.tick(refresh * 0.95) - refresh * 0.95).abs() < refresh * 0.01);
        let settled = clock.period;
        // A missed refresh counts as two.
        assert!((clock.tick(settled * 2.1) - settled * 2.).abs() < settled * 0.01);
    }

    #[test]
    fn only_moving_counts_as_scrolling() {
        assert!(room_to_move(0., 100., 50.));
        assert!(!room_to_move(0., 100., -50.), "already at the top");
        assert!(!room_to_move(100., 100., 50.), "already at the end");
        assert!(!room_to_move(0., 0., 50.), "nothing to scroll");
        assert!(room_to_move(50., 100., -50.));
    }

    #[test]
    fn matches_chromiums_table() {
        for (tau, expected) in [(0.1, 0.020), (0.2, 0.082), (0.3, 0.187), (0.5, 0.5), (0.7, 0.813), (0.9, 0.980)] {
            assert!((eased(0., tau) - expected).abs() < 0.002, "at {tau}: {}", eased(0., tau));
        }
        for (distance, ms) in [(100., 200.), (120., 200.), (200., 178.), (300., 150.), (480., 100.), (2000., 100.)] {
            assert!((duration_for(distance) * 1000. - ms).abs() < 1., "{distance}");
        }
    }

    #[test]
    fn a_second_notch_keeps_the_speed() {
        // Chromium's own example: at 60 ms into a 100 px notch, another 100 px.
        let mut glide = Glide::default();
        glide.add(100.);
        glide.step(Duration::from_millis(60));
        let before = glide.speed();
        assert!((before - 633.).abs() < 15., "{before}");
        glide.add(100.);
        assert!((glide.speed() - before).abs() < before * 0.03, "{} vs {before}", glide.speed());
        assert!((glide.distance - 181.3).abs() < 1.);
        assert!((glide.duration * 1000. - 183.).abs() < 2.);
        assert!((glide.y1 - 0.27).abs() < 0.02);
    }

    #[test]
    fn speed_rises_and_falls_evenly_like_discords() {
        let mut glide = Glide::default();
        glide.add(300.0);
        let steps: Vec<f32> = std::iter::from_fn(|| glide.is_active().then(|| glide.step(FRAME))).collect();
        let peak = steps.iter().cloned().fold(0., f32::max);
        let at = steps.iter().position(|s| *s == peak).unwrap();
        assert!((at as f32 / steps.len() as f32 - 0.5).abs() < 0.1, "peak at {at} of {}", steps.len());
    }

    #[test]
    fn movement_lands_on_device_pixels_and_loses_nothing() {
        let mut carry = 0.;
        let mut total = 0.;
        for _ in 0..100 {
            let (whole, rest) = whole_pixels(0.37 + carry, 1.5);
            assert_eq!((whole * 1.5).fract(), 0.);
            total += whole;
            carry = rest;
        }
        assert!((total + carry - 37.).abs() < 1e-2);
    }

    #[test]
    fn covers_the_whole_distance_and_settles() {
        let mut glide = Glide::default();
        glide.add(120.0);
        let (moved, frames) = run(&mut glide);
        assert!((moved - 120.0).abs() < 1e-3, "moved {moved}");
        // 200 ms at 120 Hz, with no crawl at the end.
        assert!((23..=25).contains(&frames), "{frames} frames");
        assert_eq!(glide.step(FRAME), 0.0);
    }

    #[test]
    fn starts_gently() {
        let mut glide = Glide::default();
        glide.add(100.0);
        let first = glide.step(FRAME);
        let second = glide.step(FRAME);
        assert!(first < second, "speeds up from rest: {first} then {second}");
    }

    #[test]
    fn never_overshoots() {
        let mut glide = Glide::default();
        glide.add(-80.0);
        let mut moved = 0.0;
        while glide.is_active() {
            let step = glide.step(FRAME);
            assert!(step <= 1e-3, "moved the wrong way");
            moved += step;
            assert!(moved >= -80.0 - 1e-3, "overshot to {moved}");
        }
    }

    #[test]
    fn notches_during_a_glide_add_up() {
        let mut glide = Glide::default();
        glide.add(100.0);
        let first = glide.step(FRAME) + glide.step(FRAME);
        glide.add(100.0);
        let (rest, _) = run(&mut glide);
        assert!((first + rest - 200.0).abs() < 1e-3);
    }

    #[test]
    fn reversing_cancels_out() {
        let mut glide = Glide::default();
        glide.add(60.0);
        glide.add(-60.0);
        assert!(!glide.is_active());
    }

    #[test]
    fn a_long_pause_finishes_at_once() {
        let mut glide = Glide::default();
        glide.add(300.0);
        assert!((glide.step(Duration::from_secs(1)) - 300.0).abs() < 1e-3);
        assert!(!glide.is_active());
    }
}
