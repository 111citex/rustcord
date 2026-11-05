//! Pictures that may be animated, drawn without GPUI's `img` animating them.
//! `img` asks for a redraw of the whole window on every display refresh while
//! one is on screen, 180 a second here for a GIF that changes 15 times. These
//! wake only when the next frame is due, and let go of what scrolled away long
//! ago: decoded GIFs are big, and nothing else ever frees them.

use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use gpui_kit::{
    AnyElement, App, Corners, Global, ImageSource, ImgResourceLoader, IntoElement, ObjectFit, RenderImage, Resource, Styled, Window, canvas,
};

use cord_core::Id;

use crate::ui::u;

/// The most often anything here redraws.
const MIN_WAKE: Duration = Duration::from_millis(33);
/// Decoded bytes kept before the least recently drawn are dropped.
const BUDGET: usize = 100 * 1024 * 1024;
/// Off screen for this long, and not near it, a picture goes. On disk, it's quick to
/// get back.
const NOT_SEEN_FOR: Duration = Duration::from_secs(3);
/// How often that's looked into.
const SWEEP_EVERY: Duration = Duration::from_secs(2);
/// With nothing moving, how often it's checked anyway: a full redraw isn't free.
const CHECK_ANYWAY: Duration = Duration::from_secs(30);
/// Past the budget with everything on screen, how often that's looked at again.
const OVER_BUDGET_CHECK: Duration = Duration::from_secs(6);
/// Rows remembered with their pictures, for preparing them when near the screen again.
const ROWS_REMEMBERED: usize = 4000;

struct Decoded {
    image: Arc<RenderImage>,
    bytes: usize,
    used: Instant,
    /// The chat row it was last drawn in.
    row: Option<Id>,
}

thread_local! {
    /// The chat row being laid out or drawn.
    static ROW: std::cell::Cell<Option<Id>> = const { std::cell::Cell::new(None) };
    /// The pictures seen in each chat row.
    static ROW_PICTURES: std::cell::RefCell<HashMap<Id, Vec<Resource>>> = std::cell::RefCell::default();
    /// Says which chat rows are near the screen, given how many screens count as near.
    static NEAR_ROWS: std::cell::RefCell<Option<Rc<dyn Fn(f32, &App) -> Vec<Id>>>> = const { std::cell::RefCell::new(None) };
    /// The top row last time: when it changes, the view has moved.
    static TOP_ROW: std::cell::Cell<usize> = const { std::cell::Cell::new(usize::MAX) };
    static VIEW_MOVED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static SETTLING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static LAST_CHECK: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
    static SCREENS: std::cell::Cell<u8> = const { std::cell::Cell::new(1) };
    static IN_USE: std::cell::RefCell<HashMap<String, Instant>> = std::cell::RefCell::default();
}

/// Bytes of the pictures on screen at the last look, for the setting's estimate.
pub static ON_SCREEN_BYTES: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

/// Notes that the cached thing called `key` is being shown.
pub fn note_used(key: &str) {
    IN_USE.with_borrow_mut(|u| {
        match u.get_mut(key) {
            Some(when) => *when = Instant::now(),
            None => {
                u.insert(key.to_string(), Instant::now());
            }
        }
    });
}

/// Whether `key` hasn't been shown for `long`.
pub fn unused_for(key: &str, long: Duration) -> bool {
    IN_USE.with_borrow(|u| u.get(key).is_none_or(|when| when.elapsed() > long))
}

/// Tells how to find the rows near the screen. Set once by the chat.
pub fn watch_rows(near: Rc<dyn Fn(f32, &App) -> Vec<Id>>) {
    NEAR_ROWS.with_borrow_mut(|n| *n = Some(near));
}

fn near_rows(cx: &App) -> Vec<Id> {
    let near = NEAR_ROWS.with_borrow(|n| n.clone());
    near.map_or_else(Vec::new, |near| near(f32::from(SCREENS.get()), cx))
}

/// A chat row. What's drawn inside it counts as being in that row.
pub struct Row {
    id: Id,
    child: AnyElement,
}

pub fn row(id: Id, child: AnyElement) -> Row {
    Row { id, child }
}

impl gpui_kit::IntoElement for Row {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl gpui_kit::Element for Row {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui_kit::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&gpui_kit::GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui_kit::LayoutId, ()) {
        let outer = ROW.replace(Some(self.id));
        let layout = self.child.request_layout(window, cx);
        ROW.set(outer);
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _: Option<&gpui_kit::GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        _: gpui_kit::Bounds<gpui_kit::Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        let outer = ROW.replace(Some(self.id));
        self.child.prepaint(window, cx);
        ROW.set(outer);
    }

    fn paint(
        &mut self,
        _: Option<&gpui_kit::GlobalElementId>,
        _: Option<&gpui_kit::InspectorElementId>,
        _: gpui_kit::Bounds<gpui_kit::Pixels>,
        _: &mut (),
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) {
        self.child.paint(window, cx);
    }
}

/// Called by the chat every frame: `top` is its top row, `screens` the setting. When
/// the view has moved, starts on the pictures of rows now near it.
pub fn note_view(top: usize, screens: u8, window: &mut Window, cx: &mut App) {
    SCREENS.set(screens);
    if TOP_ROW.replace(top) == top {
        return;
    }
    VIEW_MOVED.set(true);
    if screens == 0 {
        return;
    }
    let wanted: Vec<(Id, Resource)> = ROW_PICTURES.with_borrow(|rows| {
        near_rows(cx).into_iter().flat_map(|id| rows.get(&id).into_iter().flatten().map(move |r| (id, r.clone()))).collect()
    });
    for (id, resource) in wanted {
        if let Some(Ok(image)) = window.use_asset::<ImgResourceLoader>(&resource, cx) {
            let outer = ROW.replace(Some(id));
            touch(&resource, &image, cx);
            ROW.set(outer);
        }
    }
}

struct Pictures {
    started: Instant,
    held: HashMap<Resource, Decoded>,
    total: usize,
    /// A full redraw was asked for at this moment, to see what's really on screen.
    sweeping: Option<Instant>,
    resets: u64,
}

impl Global for Pictures {}

fn pictures(cx: &mut App) -> &mut Pictures {
    if !cx.has_global::<Pictures>() {
        cx.set_global(Pictures { started: Instant::now(), held: HashMap::new(), total: 0, sweeping: None, resets: 0 });
    }
    cx.global_mut::<Pictures>()
}

/// Notes `resource` as drawn just now.
fn touch(resource: &Resource, image: &Arc<RenderImage>, cx: &mut App) {
    let now = Instant::now();
    let row = ROW.get();
    if let Some(id) = row {
        ROW_PICTURES.with_borrow_mut(|rows| {
            if rows.len() >= ROWS_REMEMBERED && !rows.contains_key(&id) {
                rows.clear();
            }
            let pictures = rows.entry(id).or_default();
            if !pictures.contains(resource) {
                pictures.push(resource.clone());
            }
        });
    }
    let list = pictures(cx);
    match list.held.get_mut(resource) {
        Some(known) => {
            known.used = now;
            if row.is_some() {
                known.row = row;
            }
        }
        None => {
            let frames = image.frame_count();
            let size = image.size(0);
            let bytes = (size.width.0.max(0) as usize) * (size.height.0.max(0) as usize) * 4 * frames;
            list.total += bytes;
            list.held.insert(resource.clone(), Decoded { image: image.clone(), bytes, used: now, row });
            note_size(list);
        }
    }
}

fn note_size(list: &Pictures) {
    crate::perf::PICTURE_BYTES.store(list.total, std::sync::atomic::Ordering::Relaxed);
    crate::perf::PICTURES.store(list.held.len(), std::sync::atomic::Ordering::Relaxed);
}

fn forget(resource: &Resource, cx: &mut App) {
    let list = pictures(cx);
    let Some(dropped) = list.held.remove(resource) else { return };
    list.total -= dropped.bytes;
    note_size(list);
    cx.remove_asset::<ImgResourceLoader>(resource);
    cx.drop_image(dropped.image, None);
}

/// A picture, as `to_drop` sees it.
struct Candidate<K> {
    key: K,
    bytes: usize,
    used: Instant,
    near: bool,
}

/// Which pictures can go: those drawn since `redraw` are on screen, and stay whatever
/// the budget says. Otherwise everything not near the screen and not seen for a while,
/// then past the budget the least recently seen, near or not.
fn to_drop<K: Clone>(mut pictures: Vec<Candidate<K>>, redraw: Instant, now: Instant, budget: usize) -> Vec<K> {
    let mut total: usize = pictures.iter().map(|p| p.bytes).sum();
    pictures.sort_by_key(|p| p.used);
    let mut going = Vec::new();
    let mut staying = Vec::new();
    for picture in pictures {
        if picture.used >= redraw {
            continue;
        }
        if !picture.near && now.duration_since(picture.used) > NOT_SEEN_FOR {
            total -= picture.bytes;
            going.push(picture.key);
        } else {
            staying.push(picture);
        }
    }
    for picture in staying {
        if total <= budget {
            break;
        }
        total -= picture.bytes;
        going.push(picture.key);
    }
    going
}

/// Lets go of pictures no longer on screen or near it. A view that hasn't redrawn in a
/// while still shows its pictures without touching them, so first everything's redrawn,
/// and whatever wasn't drawn by the next look really is off screen. Waits for the view
/// to stop moving, since a redraw of the whole window is a frame that could be missed.
fn sweep(cx: &mut App) {
    let now = Instant::now();
    let resets = gpui_kit::frame_stats::ATLAS_RESETS.load(std::sync::atomic::Ordering::Relaxed);
    if pictures(cx).resets != resets {
        // Their pixels are gone with the atlas: they have to be loaded again.
        pictures(cx).resets = resets;
        let all: Vec<Resource> = pictures(cx).held.keys().cloned().collect();
        for resource in all {
            forget(&resource, cx);
        }
        cx.refresh_windows();
        return;
    }
    // Still moving: later. When it stops is when something's gone off screen.
    if VIEW_MOVED.replace(false) {
        SETTLING.set(true);
        return;
    }
    let Some(redraw) = pictures(cx).sweeping.take() else {
        let since = LAST_CHECK.get().map_or(Duration::MAX, |last| now.duration_since(last));
        let over_budget = pictures(cx).total > BUDGET && since > OVER_BUDGET_CHECK;
        if SETTLING.get() || over_budget || since > CHECK_ANYWAY {
            SETTLING.set(false);
            LAST_CHECK.set(Some(now));
            pictures(cx).sweeping = Some(now);
            cx.refresh_windows();
        }
        return;
    };
    let list = pictures(cx);
    // Nothing drawn at all: the window's hidden, which says nothing about what's on screen.
    if !list.held.values().any(|d| d.used >= redraw) {
        return;
    }
    let on_screen: usize = list.held.values().filter(|d| d.used >= redraw).map(|d| d.bytes).sum();
    ON_SCREEN_BYTES.store(on_screen, std::sync::atomic::Ordering::Relaxed);
    let near: std::collections::HashSet<Id> = near_rows(cx).into_iter().collect();
    let list = pictures(cx);
    let entries = list
        .held
        .iter()
        .map(|(r, d)| Candidate { key: r.clone(), bytes: d.bytes, used: d.used, near: d.row.is_some_and(|id| near.contains(&id)) })
        .collect();
    let going = to_drop(entries, redraw, now, BUDGET);
    let any = !going.is_empty();
    for resource in going {
        forget(&resource, cx);
    }
    crate::animated::drop_undrawn_since(redraw, cx);
    if any {
        crate::perf::give_back_freed_memory();
    }
}

/// Every `img` in the window gets its picture through this, so that they're all let
/// go of the same way. Put on the window's root.
pub struct PictureCache {
    _sweeper: gpui_kit::Task<()>,
}

impl PictureCache {
    pub fn new(cx: &mut gpui_kit::Context<Self>) -> Self {
        let sweeper = cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor().timer(SWEEP_EVERY).await;
                cx.update(|cx| {
                    sweep(cx);
                    crate::gifs::rest_unseen(cx);
                    crate::profile_effects::rest(cx);
                    crate::nameplates::rest(cx);
                    crate::stickers::rest(cx);
                });
            }
        });
        PictureCache { _sweeper: sweeper }
    }
}

impl gpui_kit::ImageCache for PictureCache {
    fn load(&mut self, resource: &Resource, window: &mut Window, cx: &mut App) -> Option<Result<Arc<RenderImage>, gpui_kit::ImageCacheError>> {
        let loaded = window.use_asset::<ImgResourceLoader>(resource, cx);
        if let Some(Ok(image)) = &loaded {
            touch(resource, image, cx);
        }
        loaded
    }
}

/// Which frame is showing `elapsed` in, and how long until the next.
pub fn frame_at(delays: &[Duration], elapsed: Duration) -> (usize, Duration) {
    let total: Duration = delays.iter().sum();
    if total.is_zero() {
        return (0, MIN_WAKE);
    }
    let mut left = Duration::from_nanos((elapsed.as_nanos() % total.as_nanos()) as u64);
    for (i, delay) in delays.iter().enumerate() {
        if left < *delay {
            return (i, *delay - left);
        }
        left -= *delay;
    }
    (0, MIN_WAKE)
}

/// A banner: its still first frame, and its animation only while the pointer's on it.
pub fn hover_banner(id: impl Into<gpui_kit::SharedString>, url: String, radius: f32) -> AnyElement {
    use gpui_kit::{InteractiveElement as _, ParentElement as _, StatefulInteractiveElement as _};
    let id: gpui_kit::SharedString = id.into();
    let still = url.replace(".gif?", ".png?");
    let moving = url.contains(".gif?") && crate::transition::pointer_on(&id);
    gpui_kit::div()
        .id(id.clone())
        .relative()
        .size_full()
        .on_hover(crate::transition::track_pointer(id))
        .child(image_top(still, ObjectFit::Cover, radius).size_full())
        .children(moving.then(|| image_top(url, ObjectFit::Cover, radius).absolute().top_0().left_0().size_full()))
        .into_any_element()
}

/// A picture, animated if it has frames. Size it with the usual styles.
pub fn image(source: impl Into<ImageSource>, fit: ObjectFit, radius: f32) -> impl IntoElement + Styled {
    image_with(source.into(), None, fit, radius)
}

/// `image` with only its top corners rounded: a banner.
pub fn image_top(source: impl Into<ImageSource>, fit: ObjectFit, radius: f32) -> impl IntoElement + Styled {
    image_with_corners(source.into(), None, fit, radius, true)
}

thread_local! {
    /// Pictures that weren't ready when first drawn: `None` while they load, then
    /// when they arrived, while they fade in.
    static ARRIVING: std::cell::RefCell<std::collections::HashMap<String, Option<std::time::Instant>>> = std::cell::RefCell::default();
}

const FADE_SECS: f32 = 0.2;

/// Whether the picture at `url` is still on its way.
pub fn is_loading(url: &str) -> bool {
    ARRIVING.with_borrow(|a| a.get(url).is_some_and(Option::is_none))
}

/// `image`, that fades in from `cover`, its placeholder's colour, if it had to be
/// waited for. One that was already there just shows.
pub fn image_fading(url: String, fallback: Option<String>, fit: ObjectFit, radius: f32, cover: gpui_kit::Hsla) -> impl IntoElement + Styled {
    let key = url.clone();
    let source: ImageSource = url.clone().into();
    canvas(
        move |_, window, cx| {
            let ImageSource::Resource(resource) = source else { return None };
            let mut loaded = window.use_asset::<ImgResourceLoader>(&resource, cx);
            let mut held = resource;
            if let (Some(Err(_)), Some(ImageSource::Resource(other))) = (&loaded, fallback.map(ImageSource::from)) {
                loaded = window.use_asset::<ImgResourceLoader>(&other, cx);
                held = other;
            }
            ARRIVING.with_borrow_mut(|a| match &loaded {
                None => {
                    if !a.contains_key(&url) {
                        if a.len() > 2000 {
                            a.retain(|_, at| at.is_none());
                        }
                        a.insert(url.clone(), None);
                        // The spinner's drawn with the rest of the page, next frame.
                        window.request_animation_frame();
                    }
                }
                Some(Ok(_)) => {
                    if let Some(at) = a.get_mut(&url).filter(|at| at.is_none()) {
                        *at = Some(std::time::Instant::now());
                    }
                }
                Some(Err(_)) => {
                    a.remove(&url);
                }
            });
            let image = loaded.and_then(Result::ok)?;
            touch(&held, &image, cx);
            Some(image)
        },
        move |bounds, image: Option<Arc<RenderImage>>, window, cx| {
            let Some(image) = image else { return };
            let image = crate::animated::frame(&image, window, cx);
            let count = image.frame_count();
            let frame = if count > 1 {
                let delays: Vec<Duration> = (0..count).map(|i| Duration::from(image.delay(i))).collect();
                let (frame, wait) = frame_at(&delays, pictures(cx).started.elapsed());
                if window.is_window_active() {
                    alhazen_gpui::redraw_after(wait.max(MIN_WAKE), window, cx);
                }
                frame
            } else {
                0
            };
            let corners = Corners::all(window.rem_size() * (radius / 16.));
            let fitted = fit.get_bounds(bounds, image.size(frame));
            let _ = window.paint_image(bounds, fitted, corners, image, frame, false);
            // The placeholder's colour thinning out over it looks the same as fading in.
            let began = ARRIVING.with_borrow(|a| a.get(&key).copied().flatten());
            if let Some(began) = began {
                let t = began.elapsed().as_secs_f32() / FADE_SECS;
                if t >= 1. {
                    ARRIVING.with_borrow_mut(|a| a.remove(&key));
                } else {
                    window.paint_quad(gpui_kit::fill(bounds, cover.opacity(1. - crate::transition::ease(t))).corner_radii(corners));
                }
                window.request_animation_frame();
            }
        },
    )
}

fn image_with(source: ImageSource, fallback: Option<String>, fit: ObjectFit, radius: f32) -> impl IntoElement + Styled {
    image_with_corners(source, fallback, fit, radius, false)
}

fn image_with_corners(source: ImageSource, fallback: Option<String>, fit: ObjectFit, radius: f32, top_only: bool) -> impl IntoElement + Styled {
    canvas(
        move |_, window, cx| match source {
            ImageSource::Resource(resource) => {
                let loaded = window.use_asset::<ImgResourceLoader>(&resource, cx);
                // The resized copy can be missing while the file itself is there.
                let (resource, loaded) = match (loaded, fallback.map(ImageSource::from)) {
                    (Some(Err(_)), Some(ImageSource::Resource(other))) => {
                        let again = window.use_asset::<ImgResourceLoader>(&other, cx);
                        (other, again)
                    }
                    (loaded, _) => (resource, loaded),
                };
                let image = loaded.and_then(Result::ok)?;
                touch(&resource, &image, cx);
                Some(image)
            }
            ImageSource::Render(image) => Some(image),
            _ => None,
        },
        move |bounds, image: Option<Arc<RenderImage>>, window, cx| {
            let Some(image) = image else { return };
            let image = crate::animated::frame(&image, window, cx);
            let count = image.frame_count();
            let frame = if count > 1 {
                let delays: Vec<Duration> = (0..count).map(|i| Duration::from(image.delay(i))).collect();
                let (frame, wait) = frame_at(&delays, pictures(cx).started.elapsed());
                // Not the focused window: stays put, as `img` does.
                if window.is_window_active() {
                    alhazen_gpui::redraw_after(wait.max(MIN_WAKE), window, cx);
                }
                frame
            } else {
                0
            };
            let round = window.rem_size() * (radius / 16.);
            let corners = Corners { top_left: round, top_right: round, bottom_left: if top_only { gpui_kit::px(0.) } else { round }, bottom_right: if top_only { gpui_kit::px(0.) } else { round } };
            let fitted = fit.get_bounds(bounds, image.size(frame));
            let _ = window.paint_image(bounds, fitted, corners, image, frame, false);
        },
    )
}

/// An emoji's picture: a plain `img` unless it's animated.
pub fn emoji(url: String, size: f32) -> AnyElement {
    if url.contains(".gif") {
        let device = (size * cord_core::cdn::device_scale()).ceil() as u32;
        let url = if url.contains('#') { url } else { format!("{url}#fit={device}x{device}") };
        image(url, ObjectFit::Contain, 0.).size(u(size)).flex_shrink_0().into_any_element()
    } else {
        gpui_kit::img(url).size(u(size)).flex_shrink_0().into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(key: &'static str, bytes: usize, used: Instant, near: bool) -> Candidate<&'static str> {
        Candidate { key, bytes, used, near }
    }

    #[test]
    fn what_is_on_screen_stays_whatever_the_budget() {
        let now = Instant::now();
        let redraw = now - Duration::from_secs(1);
        let drawn = now - Duration::from_millis(500);
        assert!(to_drop(vec![picture("a", 100, drawn, false)], redraw, now, 10).is_empty());
    }

    #[test]
    fn unseen_pictures_go_unless_near_the_screen() {
        let now = Instant::now();
        let redraw = now - Duration::from_secs(1);
        let old = now - NOT_SEEN_FOR - Duration::from_secs(1);
        let going = to_drop(vec![picture("far", 1, old, false), picture("near", 1, old, true)], redraw, now, 1000);
        assert_eq!(going, vec!["far"]);
    }

    #[test]
    fn the_budget_outranks_being_near() {
        let now = Instant::now();
        let redraw = now - Duration::from_secs(1);
        let (a, b) = (now - Duration::from_secs(9), now - Duration::from_secs(5));
        let going = to_drop(vec![picture("newer", 60, b, true), picture("older", 60, a, true)], redraw, now, 100);
        assert_eq!(going, vec!["older"]);
    }

    #[test]
    fn frames_follow_the_clock_and_loop() {
        let ms = Duration::from_millis;
        let delays = [ms(100), ms(50)];
        assert_eq!(frame_at(&delays, ms(0)), (0, ms(100)));
        assert_eq!(frame_at(&delays, ms(120)), (1, ms(30)));
        assert_eq!(frame_at(&delays, ms(150)), (0, ms(100)));
        assert_eq!(frame_at(&[], ms(5)).0, 0);
    }
}
