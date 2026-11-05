//! Rustcord: plain counters on what a frame costs. See vendor/README.md.
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

macro_rules! counters {
    ($($name:ident),* $(,)?) => {
        $(pub static $name: AtomicU64 = AtomicU64::new(0);)*
        /// What's been counted since the last call.
        pub fn take() -> Vec<(&'static str, u64)> {
            vec![$((stringify!($name), $name.swap(0, Relaxed))),*]
        }
    };
}

counters!(FRAMES, DRAW_NS, REQUEST_NS, PREPAINT_NS, PAINT_NS, FINISH_NS, PRESENT_NS, LAYOUT_NS, LAYOUT_CALLS, NODES, MEASURES, HITBOXES, MEMO_HITS,
    // The render thread: frames it put on screen, what those cost it, the part of that spent
    // waiting for a surface image, and frames replaced before it got to them.
    RENDER_FRAMES, RENDER_NS, ACQUIRE_NS, RENDER_SUPERSEDED,
    // Frames that moved a scroll layer instead of rebuilding, and moves that needed a rebuild.
    LAYER_MOVES, LAYER_MOVE_REFUSED);

/// Bytes of atlas textures alive right now, and how many. Never reset.
pub static ATLAS_BYTES: AtomicU64 = AtomicU64::new(0);
pub static ATLAS_TEXTURES: AtomicU64 = AtomicU64::new(0);
/// Bytes of those textures really holding something.
pub static ATLAS_USED: AtomicU64 = AtomicU64::new(0);
/// Goes up when the atlas is emptied: images have to be decoded again.
pub static ATLAS_RESETS: AtomicU64 = AtomicU64::new(0);

/// For one scroll, from the last [`take_scroll`]: rebuilds, layer moves, refused moves, and the
/// longest rebuild.
pub static SCROLL_REBUILDS: AtomicU64 = AtomicU64::new(0);
pub static SCROLL_MOVES: AtomicU64 = AtomicU64::new(0);
pub static SCROLL_REFUSED: AtomicU64 = AtomicU64::new(0);
pub static SCROLL_WORST_DRAW_NS: AtomicU64 = AtomicU64::new(0);

pub fn take_scroll() -> (u64, u64, u64, u64) {
    (
        SCROLL_REBUILDS.swap(0, Relaxed),
        SCROLL_MOVES.swap(0, Relaxed),
        SCROLL_REFUSED.swap(0, Relaxed),
        SCROLL_WORST_DRAW_NS.swap(0, Relaxed),
    )
}

pub fn add(counter: &AtomicU64, n: u64) {
    counter.fetch_add(n, Relaxed);
}

pub fn since(counter: &AtomicU64, start: std::time::Instant) {
    counter.fetch_add(start.elapsed().as_nanos() as u64, Relaxed);
}
