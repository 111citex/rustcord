//! Video attachments: a still with a play button until clicked, then a player
//! with Discord's controls. One plays at a time; the last few keep their place.

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use alhazen_gpui::{PlayerConfig, PlayerEvent, PlayerState, Source, VideoPlayer};
use gpui_kit::assets::IconName;
use gpui_kit::component::{Icon, h_flex};
use gpui_kit::AnimationExt;
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{
    AnyElement, App, AppContext, ElementId, Entity, Global, InteractiveElement, IntoElement, ObjectFit, ParentElement,
    SharedString, StatefulInteractiveElement, Subscription, Styled, StyledImage, div, img,
};

use crate::ui::u;

const POOL: usize = 3;
/// Clickable pieces of the seek bar: positions without measuring its width.
const SEEK_CELLS: usize = 48;
/// Narrower videos leave out the volume bar; the mute button stays.
const MIN_WIDTH_FOR_VOLUME: f32 = 300.;

struct Video {
    url: String,
    player: Entity<VideoPlayer>,
    /// What the person last asked for. The player can start itself up again
    /// (finishing a load or a buffer); this keeps a pause a pause.
    wanted: Rc<Cell<bool>>,
    _guard: Subscription,
}

#[derive(Default)]
struct Videos(Vec<Video>);

impl Global for Videos {}

fn start(url: &str, cx: &mut App) {
    if !cx.has_global::<Videos>() {
        cx.set_global(Videos::default());
    }
    let others: Vec<_> = cx.global::<Videos>().0.iter().map(|v| (v.player.clone(), v.wanted.clone())).collect();
    for (other, wanted) in others {
        wanted.set(false);
        other.update(cx, |p, cx| p.pause(cx));
    }
    let Ok(source) = Source::parse(url) else { return };
    let config = PlayerConfig { autoplay: true, ..PlayerConfig::default() };
    let player = cx.new(|cx| VideoPlayer::new(source, config, cx));
    let wanted = Rc::new(Cell::new(true));
    let _guard = cx.subscribe(&player, {
        let wanted = wanted.clone();
        move |player, event: &PlayerEvent, cx| {
            if let PlayerEvent::StateChanged(PlayerState::Playing | PlayerState::Buffering) = event
                && !wanted.get()
            {
                player.update(cx, |p, cx| p.pause(cx));
            }
        }
    });
    let videos = &mut cx.global_mut::<Videos>().0;
    videos.push(Video { url: url.to_string(), player, wanted, _guard });
    if videos.len() > POOL {
        videos.remove(0);
    }
    cx.refresh_windows();
}

pub fn player(url: &str, cx: &App) -> Option<(Entity<VideoPlayer>, Rc<Cell<bool>>)> {
    cx.try_global::<Videos>()?.0.iter().find(|v| v.url == url).map(|v| (v.player.clone(), v.wanted.clone()))
}

/// Plays `url` from the top, or pauses, resumes or replays it if it's been started.
pub fn toggle(url: &str, cx: &mut App) {
    let Some((player, wanted)) = player(url, cx) else {
        start(url, cx);
        return;
    };
    let ended = player.read(cx).state() == PlayerState::Ended;
    let resume = ended || !player.read(cx).is_playing();
    wanted.set(resume);
    // One sound at a time.
    if resume {
        let others: Vec<_> = cx.global::<Videos>().0.iter().filter(|v| v.url != url).map(|v| (v.player.clone(), v.wanted.clone())).collect();
        for (other, wanted) in others {
            wanted.set(false);
            other.update(cx, |p, cx| p.pause(cx));
        }
    }
    player.update(cx, |p, cx| {
        if ended {
            p.seek(Duration::ZERO, cx);
            p.play(cx);
        } else {
            p.toggle(cx);
        }
    });
}

/// "1:05" from a position.
pub fn clock(at: Duration) -> String {
    let s = at.as_secs();
    format!("{}:{:02}", s / 60, s % 60)
}

/// The video filling the window, if there is one.
#[derive(Default)]
struct Fullscreen {
    url: Option<String>,
    /// We put the window in fullscreen, so taking it out is ours too.
    toggled: bool,
}

impl Global for Fullscreen {}

/// What fills the window: a video's address, or `stream:<user>` for a Go Live.
pub fn fullscreen_key(cx: &App) -> Option<String> {
    cx.try_global::<Fullscreen>().and_then(|f| f.url.clone())
}

fn is_fullscreen(url: &str, cx: &App) -> bool {
    cx.try_global::<Fullscreen>().and_then(|f| f.url.as_deref()) == Some(url)
}

pub fn enter_fullscreen(url: &str, window: &mut gpui_kit::Window, cx: &mut App) {
    let toggled = !window.is_fullscreen();
    if toggled {
        window.toggle_fullscreen();
    }
    cx.set_global(Fullscreen { url: Some(url.to_string()), toggled });
    cx.refresh_windows();
}

pub fn leave_fullscreen(window: &mut gpui_kit::Window, cx: &mut App) {
    if std::mem::take(cx.default_global::<Fullscreen>()).toggled {
        window.toggle_fullscreen();
    }
    cx.refresh_windows();
}

/// Escape leaves a fullscreen video; for the window root's key handler.
pub fn on_key(event: &gpui_kit::KeyDownEvent, window: &mut gpui_kit::Window, cx: &mut App) {
    if event.keystroke.key == "escape" && cx.try_global::<Fullscreen>().is_some_and(|f| f.url.is_some()) {
        leave_fullscreen(window, cx);
        cx.stop_propagation();
    }
}

/// The file's name in its URL, for saving.
fn name_in(url: &str) -> String {
    let name = url.split(['?', '#']).next().and_then(|u| u.rsplit('/').next()).unwrap_or("").trim();
    if name.is_empty() { "video.mp4".to_string() } else { name.to_string() }
}

fn show_menu(position: gpui_kit::Point<gpui_kit::Pixels>, url: String, cx: &mut App) {
    use crate::context_menu::{self as menu, Entry};
    let (save, open) = (url.clone(), url.clone());
    let entries = vec![
        Entry::item("Save Video", move |_, cx| crate::file_preview::download(save.clone(), name_in(&save), cx)),
        Entry::copy("Copy Link", url),
        Entry::item("Open in Browser", move |_, cx| cx.open_url(&open)),
    ];
    menu::show(position, entries, cx);
}

/// The fullscreen video over everything, or nothing.
pub fn overlay(cx: &App) -> Option<AnyElement> {
    let url = cx.try_global::<Fullscreen>()?.url.clone()?;
    let (player, wanted) = player(&url, cx)?;
    let frame = div().id("video-fullscreen").absolute().top_0().left_0().size_full().bg(gpui_kit::black()).occlude();
    Some(gpui_kit::deferred(surface(frame, player, wanted, &url, true, f32::MAX, cx)).with_priority(4).into_any_element())
}

/// A `w` x `h` video box: `poster` until played, the video and controls after.
pub fn video_box(id: ElementId, url: &str, poster: &str, (w, h): (f32, f32), cx: &App) -> AnyElement {
    // Keeps its shape when narrower than the video, as pictures do.
    // Black behind it: a video of another shape than promised is letterboxed in its box.
    let frame = div().id(id).relative().bg(gpui_kit::black()).w(u(w)).max_w_full().min_w_0().aspect_ratio(w / h.max(1.)).rounded(u(8.)).bg(gpui_kit::black());
    let frame = {
        let link = url.to_string();
        frame.on_mouse_down(gpui_kit::MouseButton::Right, move |event: &gpui_kit::MouseDownEvent, _, cx: &mut App| {
            cx.stop_propagation();
            show_menu(event.position, link.clone(), cx);
        })
    };
    let Some((player, wanted)) = player(url, cx) else {
        let url = url.to_string();
        return frame
            .cursor_pointer()
            .on_click(move |_, _, cx| start(&url, cx))
            .child(img(poster.to_string()).absolute().top_0().left_0().size_full().rounded(u(8.)).object_fit(ObjectFit::Contain).with_fallback(|| div().into_any_element()))
            .child(
                div().absolute().inset_0().flex().items_center().justify_center().child(
                    div()
                        .size(u(48.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded_full()
                        .bg(gpui_kit::black().opacity(0.7))
                        .text_color(gpui_kit::white())
                        .child(Icon::new(IconName::Play).size(u(24.))),
                ),
            )
            .into_any_element();
    };
    // Only one place draws a video at a time.
    if is_fullscreen(url, cx) {
        return frame.into_any_element();
    }
    surface(frame, player, wanted, url, false, w, cx)
}

/// `frame` holding the playing video and its controls.
fn surface(
    frame: gpui_kit::Stateful<gpui_kit::Div>,
    player: Entity<VideoPlayer>,
    wanted: Rc<Cell<bool>>,
    url: &str,
    fullscreen: bool,
    width: f32,
    cx: &App,
) -> AnyElement {
    let state = player.read(cx);
    let (playing, muted, position, volume) = (state.is_playing(), state.is_muted(), state.position(), state.volume());
    let duration = state.duration().unwrap_or_default();
    let ended = state.state() == PlayerState::Ended;
    let waiting = matches!(state.state(), PlayerState::Loading | PlayerState::Buffering);
    let progress = if duration.is_zero() { 0. } else { (position.as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.) };
    let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
    let key = SharedString::from(format!("{}{url}", if fullscreen { "fs-" } else { "" }));
    let button = |name: &str, icon: IconName| {
        div()
            .id(SharedString::from(format!("{name}-{key}")))
            .size(u(24.))
            .flex_shrink_0()
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|d| d.opacity(0.75))
            .child(Icon::new(icon).size(u(18.)))
    };
    let seek = h_flex().w_full().min_w_0().h(u(14.)).items_center().children((0..SEEK_CELLS).map(|i| {
        let filled = (i as f32 + 0.5) / (SEEK_CELLS as f32) <= progress;
        let player = player.clone();
        div()
            .id(SharedString::from(format!("seek-{i}-{key}")))
            .flex_1()
            .h(u(14.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_click(move |_, _, cx| {
                cx.stop_propagation();
                let to = duration.mul_f32(i as f32 / SEEK_CELLS as f32);
                player.update(cx, |p, cx| p.seek(to, cx));
            })
            .child(div().w_full().h(u(4.)).bg(if filled { blurple } else { gpui_kit::white().opacity(0.3) }))
    }));
    let toggle = {
        let player = player.clone();
        move |_: &gpui_kit::ClickEvent, _: &mut gpui_kit::Window, cx: &mut App| {
            wanted.set(ended || !player.read(cx).is_playing());
            player.update(cx, |p, cx| {
                if ended {
                    p.seek(Duration::ZERO, cx);
                    p.play(cx);
                } else {
                    p.toggle(cx);
                }
            });
        }
    };
    let mute = player.clone();
    let level = if muted { 0. } else { volume };
    let change: std::rc::Rc<dyn Fn(f32, &mut App)> = {
        let player = player.clone();
        std::rc::Rc::new(move |to, cx| {
            player.update(cx, |p, cx| {
                p.set_muted(false, cx);
                p.set_volume(to, cx);
            });
        })
    };
    let volume_bar = div().w(u(66.)).flex_shrink_0().h(u(16.)).px(u(6.)).child(crate::slider::slider(format!("volume-{key}"), level, false, gpui_kit::white(), gpui_kit::white().opacity(0.3), change));
    frame
        .overflow_hidden()
        .on_click(toggle.clone())
        .child(alhazen_gpui::video_view(player).corner_radius(u(if fullscreen { 0. } else { 8. })).object_fit(ObjectFit::Contain).absolute().top_0().left_0().size_full())
        .when(waiting, |d| {
            d.child(
                div()
                    .absolute()
                    .inset_0()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(gpui_kit::component::spinner::Spinner::new().color(gpui_kit::white()))
                    .with_animation(
                        SharedString::from(format!("video-wait-{key}")),
                        gpui_kit::Animation::new(Duration::from_millis(600)),
                        |d, t| d.opacity((t * 2. - 1.).max(0.)),
                    ),
            )
        })
        .child(
            h_flex()
                .id(SharedString::from(format!("controls-{key}")))
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .flex_col()
                .pt(u(2.))
                .pb(u(4.))
                .px(u(8.))
                .items_start()
                .rounded_b(u(if fullscreen { 0. } else { 8. }))
                .bg(gpui_kit::black().opacity(0.65))
                .text_color(gpui_kit::white())
                .text_size(u(12.))
                // Clicks on the bar stay on the bar.
                .on_click(|_, _, cx| cx.stop_propagation())
                // The seek bar has its own line, so a narrow video still has all of it.
                .child(seek)
                .child(h_flex().w_full().min_w_0().gap(u(8.)).items_center().child(button("playpause", if playing { IconName::Pause } else { IconName::Play }).on_click(move |e, w, cx| {
                    cx.stop_propagation();
                    toggle(e, w, cx);
                }))
                .child(div().flex_shrink_0().child(format!("{} / {}", clock(position), clock(duration))))
                .child(div().flex_1().min_w_0())
                .child(button("mute", if muted { IconName::VolumeX } else { IconName::Volume2 }).on_click(move |_, _, cx| {
                    cx.stop_propagation();
                    mute.update(cx, |p, cx| {
                        let now = !p.is_muted();
                        p.set_muted(now, cx);
                    });
                }))
                .when(width >= MIN_WIDTH_FOR_VOLUME, |d| d.child(volume_bar))
                .child(button("fullscreen", if fullscreen { IconName::Minimize } else { IconName::Maximize }).on_click({
                    let url = url.to_string();
                    move |_, window, cx| {
                        cx.stop_propagation();
                        if fullscreen {
                            leave_fullscreen(window, cx);
                        } else {
                            enter_fullscreen(&url, window, cx);
                        }
                    }
                }))),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    #[test]
    fn saved_videos_take_their_name_from_the_url() {
        assert_eq!(super::name_in("https://cdn.discordapp.com/attachments/1/2/clip.mp4?ex=1&hm=2"), "clip.mp4");
        assert_eq!(super::name_in("https://x/"), "video.mp4");
    }

    #[test]
    fn clocks_read_like_discords() {
        assert_eq!(super::clock(std::time::Duration::from_secs(5)), "0:05");
        assert_eq!(super::clock(std::time::Duration::from_secs(65)), "1:05");
    }
}
