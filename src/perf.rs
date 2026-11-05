//! Plain counters for finding out what the app does with its time: which views
//! render how often, and what the long-lived caches hold. A line goes to the log
//! every minute, so a slow or swollen session can be read after the fact.

use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::RefCell;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

const EVERY: Duration = Duration::from_secs(60);

/// Live bytes by how big each allocation is: under 1 KB, 64 KB, 1 MB, 16 MB, and above.
static LIVE: [AtomicUsize; 5] = [const { AtomicUsize::new(0) }; 5];
/// Bytes of decoded pictures the picture cache holds, and how many.
pub static PICTURE_BYTES: AtomicUsize = AtomicUsize::new(0);
pub static PICTURES: AtomicUsize = AtomicUsize::new(0);

fn bucket(size: usize) -> usize {
    match size {
        0..1024 => 0,
        1024..65536 => 1,
        65536..1048576 => 2,
        1048576..16777216 => 3,
        _ => 4,
    }
}

/// Allocations from a set size up skip mimalloc. It's quick for the small things a frame
/// makes and frees, but it sat on hundreds of MB of pictures long since freed, most of
/// them having been decoded on threads that then went idle. From the system, each of
/// these is its own mapping, and it's gone from the process when it's freed.
/// The setting moves that size: lower keeps less memory, higher makes fewer mappings.
const BIG_LOWEST: usize = 64 * 1024;
const BIG_HIGHEST: usize = 2048 * 1024;
static BIG_FROM: AtomicUsize = AtomicUsize::new(BIG_LOWEST);

/// Applies the setting. Allocations already made stay where they are: freeing one looks
/// at which allocator it came from.
pub fn set_big_from(kb: u16) {
    BIG_FROM.store((kb as usize * 1024).clamp(BIG_LOWEST, BIG_HIGHEST), Relaxed);
}

/// Whether a new allocation goes to the system.
fn goes_to_system(size: usize) -> bool {
    size >= BIG_LOWEST && size >= BIG_FROM.load(Relaxed)
}

/// Whether an allocation of `size` at `ptr` came from the system.
fn came_from_system(ptr: *mut u8, size: usize) -> bool {
    if size < BIG_LOWEST {
        return false;
    }
    if size >= BIG_HIGHEST {
        return true;
    }
    // SAFETY: only looks the address up.
    !unsafe { libmimalloc_sys::mi_is_in_heap_region(ptr.cast()) }
}

/// Sets the system allocator up for that. First thing in `main`.
pub fn set_up_allocator() {
    #[cfg(target_os = "linux")]
    // SAFETY: these only change the allocator's settings, before there are other threads.
    unsafe {
        libc::mallopt(libc::M_MMAP_THRESHOLD, BIG_LOWEST as libc::c_int);
        libc::mallopt(libc::M_TRIM_THRESHOLD, 1024 * 1024);
        // C libraries' threads each get their own pool otherwise.
        libc::mallopt(libc::M_ARENA_MAX, 2);
    }
}

/// Hands memory that's been freed back to the system. Takes a moment: off the UI thread.
pub fn give_back_freed_memory() {
    // SAFETY: only asks the allocator to return what's already free.
    unsafe { libmimalloc_sys::mi_collect(true) };
    #[cfg(target_os = "linux")]
    std::thread::spawn(|| unsafe {
        libc::malloc_trim(0);
    });
}

/// Keeps count of what's live by size, and picks the allocator by size.
pub struct CountingAlloc;

// SAFETY: every call goes straight to an allocator, the same one for an allocation's
// whole life since it's picked by size. The counters don't touch the memory.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        LIVE[bucket(layout.size())].fetch_add(layout.size(), Relaxed);
        if goes_to_system(layout.size()) {
            unsafe { System.alloc(layout) }
        } else {
            unsafe { mimalloc::MiMalloc.alloc(layout) }
        }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        LIVE[bucket(layout.size())].fetch_sub(layout.size(), Relaxed);
        if came_from_system(ptr, layout.size()) {
            unsafe { System.dealloc(ptr, layout) }
        } else {
            unsafe { mimalloc::MiMalloc.dealloc(ptr, layout) }
        }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        LIVE[bucket(layout.size())].fetch_add(layout.size(), Relaxed);
        if goes_to_system(layout.size()) {
            unsafe { System.alloc_zeroed(layout) }
        } else {
            unsafe { mimalloc::MiMalloc.alloc_zeroed(layout) }
        }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let (was_system, now_system) = (came_from_system(ptr, layout.size()), goes_to_system(new_size));
        if was_system == now_system {
            LIVE[bucket(layout.size())].fetch_sub(layout.size(), Relaxed);
            LIVE[bucket(new_size)].fetch_add(new_size, Relaxed);
            return if now_system {
                unsafe { System.realloc(ptr, layout, new_size) }
            } else {
                unsafe { mimalloc::MiMalloc.realloc(ptr, layout, new_size) }
            };
        }
        // Moving between them.
        // SAFETY: the caller guarantees a valid size for this alignment.
        let new_layout = unsafe { Layout::from_size_align_unchecked(new_size, layout.align()) };
        let new = unsafe { self.alloc(new_layout) };
        if !new.is_null() {
            unsafe {
                std::ptr::copy_nonoverlapping(ptr, new, layout.size().min(new_size));
                self.dealloc(ptr, layout);
            }
        }
        new
    }
}

/// Megabytes of what the app has allocated and not freed.
pub fn live_mb() -> usize {
    LIVE.iter().map(|l| l.load(Relaxed)).sum::<usize>() / (1 << 20)
}

/// Megabytes of memory that's this process's alone, near enough.
pub fn private_mb() -> u64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let mut fields = statm.split_whitespace().skip(1).filter_map(|f| f.parse::<u64>().ok());
    let (resident, shared) = (fields.next().unwrap_or(0), fields.next().unwrap_or(0));
    resident.saturating_sub(shared) * 4096 / (1 << 20)
}

/// How much of the GPU's picture memory holds anything.
pub fn gpu_line() -> String {
    use gpui_kit::frame_stats as f;
    format!("{} of {} MB", f::ATLAS_USED.load(Relaxed) / (1 << 20), f::ATLAS_BYTES.load(Relaxed) / (1 << 20))
}

fn live_line() -> String {
    let mb = |i: usize| LIVE[i].load(Relaxed) / (1 << 20);
    format!(
        "tiny={} small={} mid={} big={} huge={} pictures={}MB/{}",
        mb(0),
        mb(1),
        mb(2),
        mb(3),
        mb(4),
        PICTURE_BYTES.load(Relaxed) / (1 << 20),
        PICTURES.load(Relaxed)
    )
}

#[derive(Default)]
struct Counters {
    since: Option<Instant>,
    counts: BTreeMap<&'static str, u64>,
}

thread_local! {
    static COUNTERS: RefCell<Counters> = RefCell::default();
}

/// Counts one occurrence of `what` (a view rendering, a row being built).
pub fn count(what: &'static str) {
    COUNTERS.with_borrow_mut(|c| *c.counts.entry(what).or_default() += 1);
}

/// `kB` of a field of /proc/self/status.
fn status_kb(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| s.lines().find(|l| l.starts_with(field)).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

/// What GPUI's own counters say since last asked: milliseconds per frame by phase
/// (layout is inside prepaint), and nodes per frame.
pub fn frame_line() -> String {
    use gpui_kit::frame_stats as f;
    let stats = f::take();
    let get = |name: &str| stats.iter().find(|(n, _)| *n == name).map_or(0, |(_, v)| *v) as f32;
    let frames = get("FRAMES").max(1.);
    let ms = |name: &str| get(name) / frames / 1e6;
    format!(
        "frames={} draw={:.2} request={:.2} prepaint={:.2} layout={:.2} paint={:.2} present={:.2} render={:.2} acquire={:.2} rendered={} superseded={} nodes={:.0} layouts={:.1} memo_hits={:.1} hitboxes={:.0} atlas={}MB/{} used={}MB",
        get("FRAMES"),
        ms("DRAW_NS"),
        ms("REQUEST_NS"),
        ms("PREPAINT_NS"),
        ms("LAYOUT_NS"),
        ms("PAINT_NS"),
        ms("PRESENT_NS"),
        get("RENDER_NS") / get("RENDER_FRAMES").max(1.) / 1e6,
        get("ACQUIRE_NS") / get("RENDER_FRAMES").max(1.) / 1e6,
        get("RENDER_FRAMES"),
        get("RENDER_SUPERSEDED"),
        get("NODES") / frames,
        get("LAYOUT_CALLS") / frames,
        get("MEMO_HITS") / frames,
        get("HITBOXES") / frames,
        f::ATLAS_BYTES.load(std::sync::atomic::Ordering::Relaxed) / (1 << 20),
        f::ATLAS_TEXTURES.load(std::sync::atomic::Ordering::Relaxed),
        f::ATLAS_USED.load(std::sync::atomic::Ordering::Relaxed) / (1 << 20),
    )
}

/// Logs and resets the counters once a minute. Called from a render.
pub fn tick() {
    let line = COUNTERS.with_borrow_mut(|c| {
        let now = Instant::now();
        let since = *c.since.get_or_insert(now);
        let elapsed = now - since;
        if elapsed < EVERY {
            return None;
        }
        c.since = Some(now);
        let rates: Vec<String> =
            c.counts.iter().map(|(what, n)| format!("{what}={:.1}/s", *n as f32 / elapsed.as_secs_f32())).collect();
        c.counts.clear();
        Some(rates.join(" "))
    });
    if let Some(line) = line {
        tracing::info!(rss_mb = status_kb("VmRSS") / 1024, threads = status_kb("Threads"), live = %live_line(), counters = %line, gpui = %frame_line(), "perf");
    }
}

/// Frame times over a stretch of scrolling.
#[derive(Default)]
pub struct Intervals {
    samples: Vec<f32>,
}

impl Intervals {
    pub fn push(&mut self, seconds: f32) {
        self.samples.push(seconds * 1000.);
    }

    /// "p50 p95 p99 max, and how many took over 1.5 refreshes", and starts over.
    pub fn take_summary(&mut self, refresh_ms: f32) -> String {
        let mut s = std::mem::take(&mut self.samples);
        if s.is_empty() {
            return String::new();
        }
        s.sort_by(f32::total_cmp);
        let at = |q: f32| s[((s.len() - 1) as f32 * q) as usize];
        let late = s.iter().filter(|v| **v > refresh_ms * 1.5).count();
        format!(
            "n={} p50={:.1} p95={:.1} p99={:.1} max={:.1} late={:.1}%",
            s.len(),
            at(0.5),
            at(0.95),
            at(0.99),
            at(1.),
            late as f32 * 100. / s.len() as f32
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freeing_finds_the_right_allocator_whatever_the_setting_says_now() {
        let alloc = CountingAlloc;
        let layout = Layout::from_size_align(BIG_LOWEST * 2, 8).unwrap();
        // SAFETY: each pointer's freed once, with its own layout.
        unsafe {
            set_big_from(64);
            let from_system = alloc.alloc(layout);
            set_big_from(2048);
            let from_mimalloc = alloc.alloc(layout);
            assert!(came_from_system(from_system, layout.size()));
            assert!(!came_from_system(from_mimalloc, layout.size()));
            alloc.dealloc(from_system, layout);
            alloc.dealloc(from_mimalloc, layout);
            set_big_from(64);
        }
    }

    #[test]
    fn growing_and_shrinking_across_allocators_keeps_the_contents() {
        let alloc = CountingAlloc;
        let small = Layout::from_size_align(1000, 8).unwrap();
        // SAFETY: each pointer's used with the layout it was last given.
        unsafe {
            let ptr = alloc.alloc(small);
            ptr.write_bytes(7, 1000);
            let ptr = alloc.realloc(ptr, small, BIG_LOWEST * 2);
            assert_eq!(*ptr.add(999), 7);
            ptr.add(BIG_LOWEST).write(9);
            let big = Layout::from_size_align(BIG_LOWEST * 2, 8).unwrap();
            let ptr = alloc.realloc(ptr, big, BIG_LOWEST + 1);
            assert_eq!((*ptr.add(999), *ptr.add(BIG_LOWEST)), (7, 9));
            let bigger = Layout::from_size_align(BIG_LOWEST + 1, 8).unwrap();
            let ptr = alloc.realloc(ptr, bigger, 500);
            assert_eq!(*ptr.add(499), 7);
            alloc.dealloc(ptr, Layout::from_size_align(500, 8).unwrap());
        }
    }

    #[test]
    fn summary_counts_late_frames() {
        let mut i = Intervals::default();
        for _ in 0..9 {
            i.push(0.005);
        }
        i.push(0.020);
        let line = i.take_summary(5.5);
        assert!(line.contains("n=10") && line.contains("late=10.0%"), "{line}");
        assert!(i.take_summary(5.5).is_empty());
    }
}
