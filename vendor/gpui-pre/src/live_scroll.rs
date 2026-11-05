//! Rustcord: a scroll layer the render thread keeps moving by itself, the way a browser's
//! compositor thread does.
//!
//! All movement is counted in whole device pixels in `moved`, which only ever goes up or down by
//! what's been shown. Each scene notes the count at the place its list was painted, so
//! wherever the count has got to by the time that scene is shown, the difference is exactly how
//! far to move it. That's what lets a scene arrive late with no jump, and the main thread
//! catch its list up whenever it gets around to it, by taking what it hasn't applied yet.

use crate::frame_stats;
use parking_lot::Mutex;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A wait between frames past this is a frame missed at 120 Hz or better, which can be felt.
pub const LONG_GAP_MS: f32 = 8.4;

/// The motion of a scroll, kept by whoever knows how it should feel. Called from the render
/// thread, or from the main thread while that one's moving the list.
pub trait ScrollMotion: Send + Sync {
    /// Logical pixels to move, `measured` after the last call (`None` for the first of a run).
    fn step(&self, measured: Option<Duration>) -> f32;
    /// Whether there's any movement left to come.
    fn is_active(&self) -> bool;
}

/// See the module docs. Shared by the main thread, the render thread and the scenes between them.
pub struct LiveScroll {
    motion: Arc<dyn ScrollMotion>,
    state: Mutex<LiveState>,
    /// Starts the render thread if it's parked. `None` where there's no paced render thread.
    render_waker: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

struct LiveState {
    /// Device pixels moved so far.
    moved: i64,
    /// How much of `moved` the main thread has applied to its list.
    synced: i64,
    /// `moved` at the newest paint.
    painted: i64,
    /// What `moved` can reach before the list itself ends, as far as the main thread last knew.
    low: i64,
    high: i64,
    scale: f32,
    /// Whether the render thread is what moves this right now. If not then the main thread is.
    may_move: bool,
    last_frame: Option<Instant>,
    /// Movement not shown yet: under a device pixel, or past what's painted.
    carry: f32,
    /// Out of painted rows and waiting for a scene with more.
    held: bool,
    /// The paint the last frame was showing, to tell when a new one came in.
    shown_tag: i64,
    last_presented: Option<Instant>,
    stats: LiveStats,
}

#[derive(Default)]
struct LiveStats {
    frames: u32,
    long_gaps: u32,
    worst_gap_ms: f32,
    ran_out: u32,
    held_frames: u32,
    handed_over: u32,
    least_margin: Option<i64>,
}

/// What the render thread did for one scroll, from the last [`LiveScroll::take_stats`].
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct LiveScrollStats {
    /// Frames presented during the glide, past the first.
    pub frames: u32,
    /// How many of those came more than [`LONG_GAP_MS`] after the one before.
    pub long_gaps: u32,
    /// The longest wait between two of them.
    pub worst_gap_ms: f32,
    /// Times the scroll reached the edge of what's painted and had to stop there.
    pub ran_out: u32,
    /// Frames spent stopped there.
    pub held_frames: u32,
    /// New scenes taken over during the glide.
    pub handed_over: u32,
    /// How close the view came to the edge of what's painted, in logical pixels.
    pub least_margin_px: Option<f32>,
}

/// What one step of a scroll layer came to.
pub struct LiveStep {
    /// Device pixels moved so far.
    pub moved: i64,
    /// There's more to come after this.
    pub active: bool,
    /// It's mid-glide, so this frame is to be presented.
    pub changed: bool,
}

impl LiveScroll {
    /// A layer's motion, not moving yet.
    pub fn new(motion: Arc<dyn ScrollMotion>) -> Arc<Self> {
        Arc::new(Self {
            motion,
            state: Mutex::new(LiveState {
                moved: 0,
                synced: 0,
                painted: 0,
                low: 0,
                high: 0,
                scale: 1.,
                may_move: false,
                last_frame: None,
                carry: 0.,
                held: false,
                shown_tag: 0,
                last_presented: None,
                stats: LiveStats::default(),
            }),
            render_waker: Mutex::new(None),
        })
    }

    /// What wakes the render thread. Without one the main thread keeps the movement.
    pub fn set_render_waker(&self, waker: Option<Arc<dyn Fn() + Send + Sync>>) {
        *self.render_waker.lock() = waker;
    }

    /// New movement has come in and the render thread may be parked.
    pub fn wake(&self) {
        if let Some(waker) = self.render_waker.lock().as_ref() {
            waker();
        }
    }

    /// Whether the render thread is what moves this right now.
    pub fn render_driven(&self) -> bool {
        self.state.lock().may_move
    }

    /// Set from each real draw: the layer in it can be moved on its own, or it can't.
    pub fn set_may_move(&self, may_move: bool) {
        let can = may_move && self.render_waker.lock().is_some();
        let mut state = self.state.lock();
        state.may_move = can;
        if !can {
            state.last_frame = None;
            state.last_presented = None;
            state.held = false;
        }
    }

    /// Whether any movement is still to come, including what's waiting for painted rows.
    pub fn is_active(&self) -> bool {
        self.motion.is_active() || self.state.lock().held
    }

    /// Device pixels moved that the main thread hasn't applied to its list, which it should do now.
    pub fn take_unsynced(&self) -> i64 {
        let mut state = self.state.lock();
        let unsynced = state.moved - state.synced;
        state.synced = state.moved;
        unsynced
    }

    /// The motion's been called off: any movement waiting for painted rows is off with it.
    pub fn drop_waiting(&self) {
        let mut state = self.state.lock();
        state.carry = 0.;
        state.held = false;
    }

    /// The scale the movement was last counted in.
    pub fn scale(&self) -> f32 {
        self.state.lock().scale
    }

    /// Where the list is, for the main thread to call right after applying [`Self::take_unsynced`].
    /// `top` and `max` are logical pixels. The movement stops where the list does.
    pub fn set_room(&self, top: f32, max: f32, scale: f32) {
        let mut state = self.state.lock();
        state.scale = scale;
        // Outward, so that the list itself gets to clamp and the last step lands right on its end.
        state.low = state.synced - (top * scale).ceil() as i64;
        state.high = state.synced + ((max - top).max(0.) * scale).ceil() as i64;
    }

    /// A paint is starting with the list at the applied movement. What goes in its scene.
    pub fn mark_painted(&self) -> i64 {
        let mut state = self.state.lock();
        state.painted = state.synced;
        state.synced
    }

    /// Device pixels moved so far.
    pub fn moved(&self) -> i64 {
        self.state.lock().moved
    }

    /// How far from the newest paint it's got, in logical pixels.
    pub fn distance_from_painted(&self) -> f32 {
        let state = self.state.lock();
        (state.moved - state.painted).abs() as f32 / state.scale
    }

    /// One frame on the render thread, for a scene painted at `painted_at`, whose rows reach as
    /// far as `moved` being `lowest` to `highest`. Reaching those holds the movement for
    /// the next scene instead of dropping it, by stopping the motion's time along with it.
    pub fn advance(&self, now: Instant, painted_at: i64, lowest: i64, highest: i64) -> LiveStep {
        let mut state = self.state.lock();
        let state = &mut *state;
        if painted_at != state.shown_tag {
            state.shown_tag = painted_at;
            if state.may_move && (state.held || self.motion.is_active()) {
                state.stats.handed_over += 1;
            }
        }
        if !state.may_move || !(state.held || self.motion.is_active()) {
            state.last_frame = None;
            state.carry = 0.;
            state.held = false;
            return LiveStep { moved: state.moved, active: false, changed: false };
        }
        let measured = state.last_frame.map(|last| now - last);
        state.last_frame = Some(now);
        let scale = state.scale;
        // Whatever comes next, a scene that's too old never moves it backward.
        let (lowest, highest) = (lowest.min(state.moved), highest.max(state.moved));

        let waiting = (state.carry * scale).round() as i64;
        let blocked = (waiting > 0 && state.moved >= highest) || (waiting < 0 && state.moved <= lowest);
        let wanted = state.carry + if blocked { 0. } else { self.motion.step(measured) };
        let whole = (wanted * scale).round() as i64;
        let rest = wanted - whole as f32 / scale;
        // Past the list's end it's lost, as it always was: turning back starts from the end.
        let (low, high) = (state.low.min(state.moved), state.high.max(state.moved));
        let ended = (state.moved + whole).clamp(low, high);
        let placed = ended.clamp(lowest, highest);
        let held = placed != ended;
        let moving = self.motion.is_active();
        state.carry = if held {
            rest + (ended - placed) as f32 / scale
        } else if moving {
            rest
        } else {
            0.
        };

        if whole != 0 {
            let margin = if whole > 0 { highest - placed } else { placed - lowest };
            let least = state.stats.least_margin.get_or_insert(margin);
            *least = margin.min(*least);
        }
        if held {
            state.stats.held_frames += 1;
            if !state.held {
                state.stats.ran_out += 1;
            }
        }
        state.held = held;
        if placed != state.moved {
            frame_stats::add(&frame_stats::LAYER_MOVES, 1);
            frame_stats::add(&frame_stats::SCROLL_MOVES, 1);
        }
        state.moved = placed;
        // Frames keep coming while it's moving, moved this time or not, which is what paces it.
        LiveStep { moved: placed, active: moving || held, changed: true }
    }

    /// A frame of the glide has gone to the compositor.
    pub fn presented(&self, now: Instant) {
        let mut state = self.state.lock();
        let state = &mut *state;
        if !state.may_move {
            state.last_presented = None;
            return;
        }
        if let Some(last) = state.last_presented {
            let gap = (now - last).as_secs_f32() * 1000.;
            state.stats.frames += 1;
            state.stats.worst_gap_ms = state.stats.worst_gap_ms.max(gap);
            state.stats.long_gaps += u32::from(gap > LONG_GAP_MS);
        }
        state.last_presented = (state.held || self.motion.is_active()).then_some(now);
    }

    /// What's happened on the render thread since the last call.
    pub fn take_stats(&self) -> LiveScrollStats {
        let mut state = self.state.lock();
        let scale = state.scale;
        let stats = std::mem::take(&mut state.stats);
        LiveScrollStats {
            frames: stats.frames,
            long_gaps: stats.long_gaps,
            worst_gap_ms: stats.worst_gap_ms,
            ran_out: stats.ran_out,
            held_frames: stats.held_frames,
            handed_over: stats.handed_over,
            least_margin_px: stats.least_margin.map(|margin| margin as f32 / scale),
        }
    }
}

/// A scroll layer's tie to its [`LiveScroll`], as painted.
#[derive(Clone)]
pub struct LiveLayer {
    pub(crate) live: Arc<LiveScroll>,
    /// [`LiveScroll`]'s count where this was painted.
    pub(crate) painted_at: i64,
}

impl LiveLayer {
    /// A layer painted with `live`'s count at `painted_at`.
    pub fn new(live: Arc<LiveScroll>, painted_at: i64) -> Self {
        Self { live, painted_at }
    }
}
