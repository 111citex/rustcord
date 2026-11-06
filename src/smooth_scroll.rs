//! Smooth mouse-wheel scrolling. GPUI moves a scroll view the whole notch at
//! once; here wheel notches set a target and the view glides to it, at
//! whatever rate the display refreshes, in about the time a browser takes. Trackpads already send
//! fine-grained pixel deltas with the OS's own momentum, so they are left to
//! GPUI untouched.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use gpui_kit::{
    App, DispatchPhase, EntityId, IntoElement, LiveScroll, ListState, ScrollDelta, ScrollHandle, ScrollLayerId,
    ScrollLayerSpec, ScrollMotion, ScrollWheelEvent, Styled, Window, canvas, point, px,
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

/// A scroll's motion, which either thread can step: the render thread does it for a layer, whether
/// or not the main thread gets around to anything.
#[derive(Default)]
struct Motion(Mutex<MotionState>);

#[derive(Default)]
struct MotionState {
    glide: Glide,
    clock: FrameClock,
}

impl Motion {
    fn state(&self) -> std::sync::MutexGuard<'_, MotionState> {
        self.0.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

impl ScrollMotion for Motion {
    fn step(&self, measured: Option<Duration>) -> f32 {
        let mut state = self.state();
        let frame = state.clock.tick(measured.unwrap_or(FIRST_FRAME).as_secs_f32());
        state.glide.step(Duration::from_secs_f32(frame))
    }

    fn is_active(&self) -> bool {
        self.state().glide.is_active()
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

    /// How far down the content the view is.
    fn top(&self) -> f32 {
        match self {
            ScrollTarget::List(list) => {
                f32::from((-list.scroll_px_offset_for_scrollbar().y).min(list.max_offset_for_scrollbar().y))
            }
            ScrollTarget::Handle(handle) => f32::from(-handle.offset().y),
        }
    }

    /// How far down the content the view can go.
    fn max_top(&self) -> f32 {
        match self {
            ScrollTarget::List(list) => f32::from(list.max_offset_for_scrollbar().y),
            ScrollTarget::Handle(handle) => f32::from(handle.max_offset().y),
        }
    }

    fn set_scroll_layer(&self, spec: ScrollLayerSpec) {
        match self {
            ScrollTarget::List(list) => list.set_scroll_layer(Some(spec)),
            ScrollTarget::Handle(handle) => handle.set_scroll_layer(Some(spec)),
        }
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
    last_frame: Option<Instant>,
    /// Movement not yet shown, being less than a device pixel.
    carry: f32,
    /// Another [`SmoothScroller::tick`] is on its way.
    ticking: bool,
    /// This scroll so far, for its log line: the longest wait between steps, and how many
    /// took long enough to see.
    worst_gap: f32,
    long_gaps: u32,
    steps: u32,
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
    motion: Arc<Motion>,
    state: Rc<RefCell<State>>,
    /// Lives inside a full profile, so that being open doesn't silence it.
    in_modal: bool,
    /// A list whose rows do something when hovered: not while it's moving under the pointer.
    calm_hover: bool,
    /// Moves a layer of what's already painted, instead of rebuilding the list to move it.
    layer: Option<LayerScrolling>,
}

/// What it takes to move a list without rebuilding it.
#[derive(Clone)]
struct LayerScrolling {
    id: ScrollLayerId,
    /// Whether the rest of the view stays the same with the list's top at this place. Set each
    /// render by a view that shows anything following the scroll outside the list.
    outside_stays_right: Rc<RefCell<Option<OutsideCheck>>>,
    /// Moves it from the render thread while nothing stops it from being moved.
    live: Arc<LiveScroll>,
    /// Brings the list up to what `live` has moved, given the window's scale.
    follow: Rc<dyn Fn(f32)>,
}

/// See [`LayerScrolling::outside_stays_right`].
type OutsideCheck = Box<dyn Fn(f32) -> bool>;

/// How far outside the view a layer's content gets painted. Stays under what a view keeps fully
/// built, for the same reason as [`KEPT_BEYOND_LAYER`].
pub const LAYER_OVERSCAN: f32 = 600.;

/// How far beyond [`LAYER_OVERSCAN`] rows a view swaps for spacers stay real anyway: the view's
/// size and place used for choosing them are from the last frame, and may have changed by now.
pub const KEPT_BEYOND_LAYER: f32 = 200.;

/// How long after the last movement a list counts as still scrolling.
const SETTLE: Duration = Duration::from_millis(180);

/// How far a layer can get from where it was painted before asking for a rebuild. It's asked for
/// this early so that even a slow one is done before the view gets to the edge of what's painted.
const REBUILD_AFTER: f32 = LAYER_OVERSCAN / 2.;

/// How often the main thread looks at what the render thread has done with a layer. There's no
/// present here to hang this on, and missing a turn while busy does no harm.
const FOLLOW_EVERY: Duration = Duration::from_millis(4);

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
        SmoothScroller { target, motion: Arc::default(), state: Rc::default(), in_modal: false, calm_hover: false, layer: None }
    }

    /// Moves the list by moving a layer of what's painted, whenever that gives the same picture
    /// as rebuilding it would. Only for lists that hover and the like don't change as they
    /// move, which is what [`Self::calm_hover`] sees to.
    pub fn layered(mut self) -> Self {
        debug_assert!(self.calm_hover);
        let live = LiveScroll::new(self.motion.clone());
        let follow: Rc<dyn Fn(f32)> = {
            let (target, live) = (self.target.clone(), live.clone());
            Rc::new(move |scale| {
                let moved = live.take_unsynced();
                if moved != 0 {
                    target.scroll_by(moved as f32 / scale);
                }
                live.set_room(target.top(), target.max_top(), scale);
            })
        };
        self.layer = Some(LayerScrolling { id: ScrollLayerId::new(), outside_stays_right: Rc::default(), live, follow });
        self
    }

    /// Tells a layered list what outside it follows the scroll. See [`LayerScrolling`].
    pub fn outside_stays_right(&self, check: impl Fn(f32) -> bool + 'static) {
        if let Some(layer) = &self.layer {
            *layer.outside_stays_right.borrow_mut() = Some(Box::new(check));
        }
    }

    /// Keeps a layered list going while it moves, off the main thread if nothing stops that.
    fn tick(&self, layer: &LayerScrolling, view: EntityId, window: &mut Window, cx: &mut App) {
        (layer.follow)(window.scale_factor());
        if layer.live.render_driven() {
            self.follow_render_thread(layer, view, window, cx);
        } else {
            self.step_on_this_thread(layer, view, window, cx);
        }
    }

    /// The render thread has the movement. All that's left here is to see that there's always a new
    /// scene coming before that runs out, and to finish up when it's over.
    fn follow_render_thread(&self, layer: &LayerScrolling, view: EntityId, window: &mut Window, cx: &mut App) {
        let mut state = self.state.borrow_mut();
        // In case the main thread takes over again in mid-glide.
        state.last_frame = None;
        state.carry = 0.;
        let outside_right =
            layer.outside_stays_right.borrow().as_ref().is_none_or(|check| check(self.target.top()));
        if !outside_right || layer.live.distance_from_painted() > REBUILD_AFTER {
            cx.notify(view);
        }
        if layer.live.is_active() {
            SCROLLING_UNTIL.set(Some(Instant::now() + SETTLE));
        } else if self.settled(&mut state, layer, view, cx) {
            return;
        }
        drop(state);
        self.schedule_tick(layer.clone(), view, window, cx);
    }

    /// One step of a layered list's glide, at the start of a frame, for when moving it takes this
    /// thread: something's in the way of the render thread doing it, or it can't here. Done there
    /// and not in a render, because no render need happen.
    fn step_on_this_thread(&self, layer: &LayerScrolling, view: EntityId, window: &mut Window, cx: &mut App) {
        let mut state = self.state.borrow_mut();
        if layer.live.is_active() {
            SCROLLING_UNTIL.set(Some(Instant::now() + SETTLE));
            let now = Instant::now();
            let measured = state.last_frame.map(|last| now - last);
            if let Some(measured) = measured {
                let gap = measured.as_secs_f32() * 1000.;
                state.worst_gap = state.worst_gap.max(gap);
                state.long_gaps += u32::from(gap > gpui_kit::LONG_GAP_MS);
                state.steps += 1;
            }
            state.last_frame = Some(now);
            let moved = self.motion.step(measured) + state.carry;
            let (whole, rest) = whole_pixels(moved, window.scale_factor());
            state.carry = if self.motion.is_active() { rest } else { 0. };
            if whole != 0. {
                let before = self.target.top();
                self.target.scroll_by(whole);
                let after = self.target.top();
                let outside_right =
                    layer.outside_stays_right.borrow().as_ref().is_none_or(|check| check(after));
                if after != before && !(window.move_scroll_layer(layer.id, px(before - after)) && outside_right) {
                    cx.notify(view);
                }
            }
            // Keeps frames coming at the compositor's pace, moved or not.
            window.present_this_frame();
        } else {
            state.last_frame = None;
            state.carry = 0.;
            if self.settled(&mut state, layer, view, cx) {
                return;
            }
        }
        drop(state);
        self.schedule_tick(layer.clone(), view, window, cx);
    }

    /// The motion's over: once it's been over for long enough, wraps up. Returns if it has.
    fn settled(&self, state: &mut State, layer: &LayerScrolling, view: EntityId, cx: &mut App) -> bool {
        match SCROLLING_UNTIL.get() {
            Some(until) if Instant::now() < until => return false,
            Some(_) => {
                SCROLLING_UNTIL.set(None);
                let (rebuilds, moves, refused, worst_draw) = gpui_kit::frame_stats::take_scroll();
                let live = layer.live.take_stats();
                let steps = state.steps + live.frames;
                if steps > 0 {
                    tracing::info!(
                        target: "rustcord::perf",
                        steps,
                        moves,
                        rebuilds,
                        refused,
                        long_gaps = state.long_gaps + live.long_gaps,
                        worst_gap_ms = state.worst_gap.max(live.worst_gap_ms),
                        worst_rebuild_ms = worst_draw as f32 / 1e6,
                        render_thread_frames = live.frames,
                        handed_over = live.handed_over,
                        ran_out = live.ran_out,
                        held_frames = live.held_frames,
                        least_margin_px = live.least_margin_px.unwrap_or(-1.),
                        "scroll"
                    );
                }
                (state.worst_gap, state.long_gaps, state.steps) = (0., 0, 0);
                let after = AFTER_SCROLLING.take();
                cx.defer(move |cx| {
                    if let Some((_, action)) = after {
                        action(cx);
                    }
                });
            }
            None => {}
        }
        // Once more, for real: hover back on, and the hitboxes where the rows are.
        cx.notify(view);
        state.ticking = false;
        true
    }

    fn schedule_tick(&self, layer: LayerScrolling, view: EntityId, window: &Window, cx: &App) {
        self.state.borrow_mut().ticking = true;
        let this = self.clone();
        if layer.live.render_driven() {
            window
                .spawn(cx, async move |cx| {
                    cx.background_executor().timer(FOLLOW_EVERY).await;
                    cx.update(|window, cx| this.tick(&layer, view, window, cx)).ok();
                })
                .detach();
        } else {
            window.on_next_frame(move |window, cx| this.tick(&layer, view, window, cx));
        }
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
        self.motion.state().glide.add(pixels);
        if let Some(layer) = &self.layer {
            layer.live.wake();
        }
    }

    pub fn is_gliding(&self) -> bool {
        match &self.layer {
            Some(layer) => layer.live.is_active(),
            None => self.motion.is_active(),
        }
    }

    /// Drops any glide under way: the view has been put somewhere else. What's been shown of it
    /// goes to the list first, so that it can't land on top of the new place later.
    pub fn stop(&self) {
        self.motion.state().glide = Glide::default();
        if let Some(layer) = &self.layer {
            layer.live.drop_waiting();
            (layer.follow)(layer.live.scale());
        }
    }

    pub fn layer(&self) -> impl IntoElement {
        let this = self.clone();
        let stepper = this.clone();
        canvas(
            // Moves before painting: put ahead of the scroll view, the frame shows
            // this step instead of the last one.
            move |_, window, cx| {
                if let Some(layer) = stepper.layer.clone() {
                    stepper.target.set_scroll_layer(ScrollLayerSpec { id: layer.id, overscan: px(LAYER_OVERSCAN) });
                    window.keep_scroll_layer_live(layer.id, &layer.live, &layer.follow);
                    let moving = layer.live.is_active() || SCROLLING_UNTIL.get().is_some();
                    if moving && !stepper.state.borrow().ticking {
                        stepper.schedule_tick(layer, window.current_view(), window, cx);
                    }
                    return;
                }
                if stepper.calm_hover {
                    if stepper.motion.is_active() {
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
                if stepper.motion.is_active() {
                    let now = Instant::now();
                    let measured = state.last_frame.map(|last| now - last);
                    state.last_frame = Some(now);
                    let moved = stepper.motion.step(measured) + state.carry;
                    let (whole, rest) = whole_pixels(moved, window.scale_factor());
                    state.carry = if stepper.motion.is_active() { rest } else { 0. };
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
                    if crate::viewer::is_open(cx) || crate::confirm::is_open(cx) || crate::panels::is_open(cx) || ((crate::add_people::is_open(cx) || crate::profile_card::full_open()) && !this.in_modal) {
                        return;
                    }
                    let ScrollDelta::Lines(lines) = event.delta else { return };
                    cx.stop_propagation();
                    // Only actual scrolling moves a card out of the way.
                    if this.target.can_move(-lines.y) {
                        crate::profile_card::note_wheel(lines.y.abs(), cx);
                    }
                    if this.calm_hover {
                        if SCROLLING_UNTIL.get().is_none() {
                            gpui_kit::frame_stats::take_scroll();
                        }
                        SCROLLING_UNTIL.set(Some(Instant::now() + SETTLE));
                    }
                    let ticking = this.state.borrow().ticking;
                    this.glide_by(-lines.y * LINE_PX);
                    // This view alone: a window refresh redraws the other lists too. If a layer
                    // is moving already then the next step takes this along.
                    if this.layer.is_none() || !ticking {
                        cx.notify(view);
                    }
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

/// The render thread's side of a layer, with stand-ins for the threads: every "frame" here is one
/// it'd make, and every "rebuild" is what the main thread does around a draw.
#[cfg(test)]
mod live_layer_tests {
    use super::*;
    use gpui_kit::{Bounds, ContentMask, EntityId, LiveLayer, Quad, ScaledPixels, Scene, point, size};
    use std::sync::atomic::{AtomicU32, Ordering};

    const FRAME: Duration = Duration::from_micros(5_555);

    /// Moves a set number of pixels a frame, for as many frames as it's told.
    #[derive(Default)]
    struct Steady {
        left: AtomicU32,
        per_frame: Mutex<f32>,
        measured: Mutex<Vec<Option<Duration>>>,
    }

    impl Steady {
        fn run(&self, frames: u32, per_frame: f32) {
            self.left.store(frames, Ordering::SeqCst);
            *self.per_frame.lock().unwrap() = per_frame;
        }
    }

    impl ScrollMotion for Steady {
        fn step(&self, measured: Option<Duration>) -> f32 {
            self.measured.lock().unwrap().push(measured);
            if self.left.load(Ordering::SeqCst) == 0 {
                return 0.;
            }
            self.left.fetch_sub(1, Ordering::SeqCst);
            *self.per_frame.lock().unwrap()
        }

        fn is_active(&self) -> bool {
            self.left.load(Ordering::SeqCst) > 0
        }
    }

    fn live(motion: &Arc<Steady>, top: f32, max: f32) -> Arc<LiveScroll> {
        let live = LiveScroll::new(motion.clone());
        live.set_render_waker(Some(Arc::new(|| {})));
        live.set_may_move(true);
        live.set_room(top, max, 1.);
        live
    }

    fn at(frame: u32) -> Instant {
        // A fixed start: only the differences between these are ever looked at.
        static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
        *START.get_or_init(Instant::now) + FRAME * frame
    }

    #[test]
    fn moves_by_itself_and_the_list_catches_up_exactly_once() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(10, 7.);
        for frame in 0..4 {
            let step = live.advance(at(frame), 0, -600, 600);
            assert_eq!(step.moved, 7 * (frame as i64 + 1));
            assert!(step.active);
        }
        assert_eq!(live.take_unsynced(), 28);
        assert_eq!(live.take_unsynced(), 0, "the same movement must not be applied twice");
        for frame in 4..10 {
            live.advance(at(frame), 0, -600, 600);
        }
        assert_eq!(live.take_unsynced(), 42);
        assert!(!live.is_active());
    }

    #[test]
    fn a_scene_painted_mid_glide_shows_at_the_live_place() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(100, 10.);
        let clip = bounds(0., 400.);
        let painted = bounds(-600., 1600.);

        // What's on screen, as a place in the list: where the shown scene was painted and how
        // far it's been moved since. The old scene goes on showing for all of a slow rebuild.
        let mut list_top = 1000.;
        let mut scene_top = list_top;
        let mut scene = layered_scene(&live, 0, clip, painted);
        let mut rebuilding = None;
        let mut shown = Vec::new();
        for frame in 0..100u32 {
            if frame % 25 == 3 {
                list_top += live.take_unsynced() as f32;
                live.set_room(list_top, 5000., 1.);
                let tag = live.mark_painted();
                rebuilding = Some((frame + 7, layered_scene(&live, tag, clip, painted), list_top));
            }
            if rebuilding.as_ref().is_some_and(|(ready, ..)| *ready == frame) {
                (_, scene, scene_top) = rebuilding.take().unwrap();
            }
            scene.advance_live_layers(at(frame));
            shown.push(scene_top - shown_offset(&scene));
        }
        for pair in shown.windows(2) {
            assert!((pair[1] - pair[0] - 10.).abs() < 0.01, "stepped {pair:?}");
        }
        assert_eq!(live.take_stats().handed_over, 4);
    }

    /// How far the layer's been moved in what would be drawn.
    fn shown_offset(scene: &Scene) -> f32 {
        let mut composed = Scene::default();
        scene.copy_primitives_into(&mut composed);
        composed.compose_scroll_layers();
        composed.quads[0].bounds.origin.y.0 - PAINTED_TOP
    }

    const PAINTED_TOP: f32 = -600.;

    fn bounds(top: f32, height: f32) -> Bounds<ScaledPixels> {
        Bounds { origin: point(ScaledPixels(0.), ScaledPixels(top)), size: size(ScaledPixels(100.), ScaledPixels(height)) }
    }

    fn layered_scene(live: &Arc<LiveScroll>, tag: i64, clip: Bounds<ScaledPixels>, painted: Bounds<ScaledPixels>) -> Scene {
        let mut scene = Scene::default();
        scene.begin_live_scroll_layer(
            ScrollLayerId::new(),
            EntityId::from(1u64),
            clip,
            painted,
            Some(LiveLayer::new(live.clone(), tag)),
        );
        // All of what's painted, so that some of it is always in view.
        scene.insert_primitive(Quad { bounds: painted, content_mask: ContentMask { bounds: painted }, ..Default::default() });
        scene.end_scroll_layer();
        scene
    }

    #[test]
    fn composing_moves_the_primitives_by_the_live_distance() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(3, 10.);
        let mut scene = layered_scene(&live, 0, bounds(0., 400.), bounds(-600., 1600.));
        for frame in 0..3 {
            scene.advance_live_layers(at(frame));
        }
        assert_eq!(shown_offset(&scene), -30.);
    }

    #[test]
    fn running_out_of_rows_holds_at_the_edge_and_loses_nothing() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(10, 30.);
        // Room for 100 pixels of the 300 coming.
        for frame in 0..3 {
            let step = live.advance(at(frame), 0, -600, 100);
            assert!(step.moved <= 90);
        }
        let step = live.advance(at(3), 0, -600, 100);
        assert_eq!(step.moved, 100, "goes right up to the edge");
        assert!(step.active);
        let left = motion.left.load(Ordering::SeqCst);
        for frame in 4..50 {
            let step = live.advance(at(frame), 0, -600, 100);
            assert_eq!(step.moved, 100, "holds there");
            assert!(step.active);
        }
        assert_eq!(motion.left.load(Ordering::SeqCst), left, "the motion waits too");

        // A new scene with more painted: it carries on from there and gets all the way.
        let mut frame = 50;
        let mut step;
        loop {
            step = live.advance(at(frame), 100, -500, 700);
            frame += 1;
            if !step.active {
                break;
            }
        }
        assert_eq!(step.moved, 300);
        let stats = live.take_stats();
        assert_eq!(stats.ran_out, 1, "one run out, however long it lasted");
        assert_eq!(stats.held_frames, 47);
    }

    #[test]
    fn the_motion_lost_no_time_while_held() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(5, 30.);
        live.advance(at(0), 0, -600, 40);
        live.advance(at(1), 0, -600, 40);
        live.advance(at(500), 0, -600, 40);
        live.advance(at(501), 40, -560, 640);
        let measured = motion.measured.lock().unwrap();
        assert_eq!(*measured.last().unwrap(), Some(FRAME), "picks up with one frame's time");
    }

    #[test]
    fn stops_at_the_end_of_the_list_and_turning_back_starts_there() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 1020.);
        motion.run(5, 10.);
        for frame in 0..5 {
            let step = live.advance(at(frame), 0, -600, 600);
            assert!(step.moved <= 20);
        }
        assert_eq!(live.moved(), 20);
        assert_eq!(live.take_stats().ran_out, 0, "the list's end isn't running out of rows");
        motion.run(1, -10.);
        assert_eq!(live.advance(at(5), 0, -600, 600).moved, 10);
    }

    #[test]
    fn sub_pixel_movement_waits_for_a_whole_pixel_and_scale_counts() {
        let motion = Arc::new(Steady::default());
        let live = LiveScroll::new(motion.clone());
        live.set_render_waker(Some(Arc::new(|| {})));
        live.set_may_move(true);
        live.set_room(1000., 5000., 1.5);
        motion.run(10, 0.4);
        let mut last = 0;
        for frame in 0..9 {
            last = live.advance(at(frame), 0, -900, 900).moved;
        }
        // 3.6 logical pixels at 1.5 is 5.4 device pixels, rounded each step as it's built up.
        assert_eq!(last, 5);
        let unsynced = live.take_unsynced();
        assert_eq!(unsynced, 5);
    }

    #[test]
    fn nothing_moves_unless_it_may() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        live.set_may_move(false);
        motion.run(5, 10.);
        let step = live.advance(at(0), 0, -600, 600);
        assert!(!step.active && !step.changed);
        assert_eq!(step.moved, 0);
        assert_eq!(motion.left.load(Ordering::SeqCst), 5, "the main thread has it");
    }

    #[test]
    fn may_not_move_without_a_render_thread_to_do_it() {
        let live = LiveScroll::new(Arc::new(Steady::default()));
        live.set_may_move(true);
        assert!(!live.render_driven());
    }

    #[test]
    fn gaps_are_between_frames_presented() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(4, 10.);
        live.presented(at(0));
        live.presented(at(1));
        live.presented(at(3));
        let stats = live.take_stats();
        assert_eq!(stats.frames, 2);
        assert_eq!(stats.long_gaps, 1);
        assert!((stats.worst_gap_ms - 11.11).abs() < 0.1, "{}", stats.worst_gap_ms);
        // A later glide doesn't count the wait since the one before.
        motion.run(0, 0.);
        live.presented(at(4));
        motion.run(2, 10.);
        live.presented(at(100));
        assert_eq!(live.take_stats().long_gaps, 0);
    }

    #[test]
    fn calling_it_off_while_held_ends_it() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(3, 30.);
        live.advance(at(0), 0, -600, 40);
        live.advance(at(1), 0, -600, 40);
        assert!(live.is_active());
        motion.run(0, 0.);
        live.drop_waiting();
        assert!(!live.is_active());
        let step = live.advance(at(2), 0, -600, 40);
        assert!(!step.active);
        assert_eq!(step.moved, 40);
        assert_eq!(live.take_unsynced(), 40, "what was shown still counts");
    }

    #[test]
    fn a_margin_is_recorded_toward_the_way_it_went() {
        let motion = Arc::new(Steady::default());
        let live = live(&motion, 1000., 5000.);
        motion.run(2, 100.);
        live.advance(at(0), 0, -600, 600);
        live.advance(at(1), 0, -600, 600);
        assert_eq!(live.take_stats().least_margin_px, Some(400.));
    }

    /// The real glide, moved on the render thread, lands every notch where it did on the main
    /// thread, in the same steps.
    #[test]
    fn the_glide_feels_the_same_from_the_render_thread() {
        let scale = 1.5;
        let run = |live_side: bool| {
            let motion = Arc::new(Motion::default());
            let live = LiveScroll::new(motion.clone());
            live.set_render_waker(Some(Arc::new(|| {})));
            live.set_may_move(true);
            live.set_room(1000., 50_000., scale);
            motion.state().glide.add(300.);
            let (mut carry, mut total, mut last) = (0., 0., None);
            let mut steps = Vec::new();
            let mut frame = 0;
            while motion.is_active() {
                let now = at(frame);
                if frame == 12 {
                    motion.state().glide.add(150.);
                }
                let shown = if live_side {
                    live.advance(now, 0, -100_000, 100_000).moved as f32 / scale
                } else {
                    let measured = last.map(|last| now - last);
                    last = Some(now);
                    let moved = motion.step(measured) + carry;
                    let (whole, rest) = whole_pixels(moved, scale);
                    carry = if motion.is_active() { rest } else { 0. };
                    total += whole;
                    total
                };
                steps.push(shown);
                frame += 1;
                assert!(frame < 1000);
            }
            steps
        };
        let (here, there) = (run(false), run(true));
        assert_eq!(here.len(), there.len());
        for (a, b) in here.iter().zip(&there) {
            assert!((a - b).abs() <= 1. / scale, "{here:?}\n{there:?}");
        }
        assert!((there.last().unwrap() - 450.).abs() <= 1. / scale);
    }
}
