//! Sound in chat: voice messages as Discord draws them, and other audio files
//! with a seekable player. Playback shares the video player: one at a time.

use std::cell::{Cell, RefCell};
use std::time::Duration;

use cord_core::rich::Attachment;
use gpui_kit::assets::IconName;
use gpui_kit::component::{ActiveTheme, Icon, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder;
use gpui_kit::{AnyElement, App, FontWeight, InteractiveElement, IntoElement, ParentElement, SharedString, StatefulInteractiveElement, Styled, div};

use crate::markdown_view::Env;
use crate::ui::u;

thread_local! {
    /// Muted with the volume button: holds for whatever plays next too.
    static MUTED: Cell<bool> = const { Cell::new(false) };
}

const FLAG_VOICE_MESSAGE: u64 = 1 << 13;
const BARS: usize = 40;
const SEEK_CELLS: usize = 48;

pub fn is_audio(a: &Attachment) -> bool {
    const EXTENSIONS: [&str; 8] = ["mp3", "ogg", "oga", "opus", "wav", "flac", "m4a", "aac"];
    let by_type = a.content_type.as_deref().is_some_and(|t| t.starts_with("audio/"));
    let extension = a.filename.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    !a.url.is_empty() && (by_type || EXTENSIONS.contains(&extension.as_str()))
}

/// `levels` squeezed or stretched to `count` bars, 0 to 1.
fn bars(levels: &[u8], count: usize) -> Vec<f32> {
    if levels.is_empty() {
        return vec![0.25; count];
    }
    (0..count)
        .map(|i| {
            let start = i * levels.len() / count;
            let end = ((i + 1) * levels.len() / count).max(start + 1).min(levels.len());
            let part = &levels[start.min(levels.len() - 1)..end];
            part.iter().map(|l| f32::from(*l)).sum::<f32>() / part.len() as f32 / 255.
        })
        .collect()
}

/// What's known of the sound right now.
struct Sound {
    playing: bool,
    muted: bool,
    position: Duration,
    duration: Duration,
    progress: f32,
}

fn sound(a: &Attachment, cx: &App) -> Sound {
    let known = Duration::from_secs_f32(a.duration_secs.unwrap_or(0.).max(0.));
    let Some(state) = crate::videos::player(&a.url, cx).map(|(p, _)| p.read(cx)) else {
        return Sound { playing: false, muted: false, position: Duration::ZERO, duration: known, progress: 0. };
    };
    let duration = state.duration().unwrap_or(known);
    let position = state.position();
    let progress = if duration.is_zero() { 0. } else { (position.as_secs_f32() / duration.as_secs_f32()).clamp(0., 1.) };
    Sound { playing: state.is_playing(), muted: state.is_muted(), position, duration, progress }
}

/// Keeps the sound's clock moving on screen while it plays. Draws nothing.
fn ticker(a: &Attachment, cx: &App) -> Option<AnyElement> {
    let (player, _) = crate::videos::player(&a.url, cx)?;
    Some(alhazen_gpui::video_view(player).absolute().size_0().into_any_element())
}

fn seek_to(url: &str, duration: Duration, at: f32, cx: &mut App) {
    if let Some((player, _)) = crate::videos::player(url, cx) {
        player.update(cx, |p, cx| p.seek(duration.mul_f32(at), cx));
    }
}

thread_local! {
    /// The sound level for voice and audio, 0 to 1: kept for ones not started yet.
    static VOLUME: Cell<f32> = const { Cell::new(1.) };
    /// Which volume button the pointer's on, and which slider. Either keeps the slider up.
    static OVER_BUTTON: RefCell<Option<SharedString>> = const { RefCell::new(None) };
    static OVER_SLIDER: RefCell<Option<SharedString>> = const { RefCell::new(None) };
}

fn note_hover(state: &'static std::thread::LocalKey<RefCell<Option<SharedString>>>, id: &SharedString, over: bool) {
    state.with_borrow_mut(|held| {
        if over {
            *held = Some(id.clone());
        } else if held.as_ref() == Some(id) {
            *held = None;
        }
    });
}

fn set_volume(url: &str, level: f32, cx: &mut App) {
    // Every pointer move lands here: only an actual change is worth anything.
    if VOLUME.get() == level && !MUTED.get() {
        return;
    }
    VOLUME.set(level);
    MUTED.set(false);
    if let Some((player, _)) = crate::videos::player(url, cx) {
        player.update(cx, |p, cx| {
            p.set_muted(false, cx);
            p.set_volume(level, cx);
        });
    }
    crate::ui::redraw_chat(cx);
}

/// The volume button, with Discord's upright slider while the pointer's on either.
fn volume_button(id: SharedString, a: &Attachment, muted: bool, cx: &App) -> impl IntoElement {
    const STEPS: usize = 20;
    let theme = cx.theme();
    let open = OVER_BUTTON.with_borrow(|b| b.as_ref() == Some(&id)) || OVER_SLIDER.with_borrow(|s| s.as_ref() == Some(&id));
    let muted = muted || MUTED.get();
    let level = if muted { 0. } else { VOLUME.get() };
    let url = a.url.clone();
    let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
    let cells = (0..STEPS).map(|i| {
        // Top cell is full volume.
        let at = (STEPS - i) as f32 / STEPS as f32;
        let (down, moved) = (url.clone(), url.clone());
        div()
            .w_full()
            .h(u(5.))
            .flex()
            .justify_center()
            .on_mouse_down(gpui_kit::MouseButton::Left, move |_, _, cx| {
                cx.stop_propagation();
                set_volume(&down, at, cx);
            })
            .on_mouse_move(move |event: &gpui_kit::MouseMoveEvent, _, cx| {
                if event.pressed_button == Some(gpui_kit::MouseButton::Left) {
                    set_volume(&moved, at, cx);
                }
            })
            .child(
                div()
                    .w(u(6.))
                    .h_full()
                    .when(i == 0, |d| d.rounded_t(u(3.)))
                    .when(i == STEPS - 1, |d| d.rounded_b(u(3.)))
                    .bg(if at <= level + 0.001 { blurple } else { theme.muted_foreground.opacity(0.3) }),
            )
    });
    let (hover_button, hover_slider) = (id.clone(), id.clone());
    let toggle_url = a.url.clone();
    div()
        .relative()
        .flex_shrink_0()
        .child(
            div()
                .id(id.clone())
                .size(u(24.))
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|d| d.opacity(0.75))
                .on_hover(move |over, window, _| {
                    note_hover(&OVER_BUTTON, &hover_button, *over);
                    window.refresh();
                })
                .on_click(move |_, _, cx| {
                    let now = !MUTED.get();
                    MUTED.set(now);
                    if let Some((player, _)) = crate::videos::player(&toggle_url, cx) {
                        player.update(cx, |p, cx| p.set_muted(now, cx));
                    }
                    cx.refresh_windows();
                })
                .child(Icon::new(if muted { IconName::VolumeX } else { IconName::Volume2 }).size(u(20.))),
        )
        .when(open, |d| {
            // Touches the button, with its padding as the bridge, so the pointer never
            // leaves one without being on the other. Over everything: under the message's
            // toolbar, moving onto it would count as leaving.
            d.child(
                gpui_kit::deferred(
                    // The hover covers the gap down to the button too: crossing it mustn't
                    // count as leaving.
                    div()
                        .id(SharedString::from(format!("{id}-slider")))
                        .occlude()
                        .on_hover(move |over, window, _| {
                            note_hover(&OVER_SLIDER, &hover_slider, *over);
                            window.refresh();
                        })
                        .absolute()
                        .bottom(u(24.))
                        .left(u(-4.))
                        .w(u(32.))
                        .pb(u(6.))
                        .child(
                        div()
                            .w_full()
                            .py(u(12.))
                            .rounded(u(16.))
                            .border_1()
                            .border_color(theme.border)
                            .bg(theme.popover)
                            .shadow_md()
                            .child(
                                div()
                                    .relative()
                                    .w_full()
                                    .h(u(STEPS as f32 * 5.))
                                    .children(cells)
                                    .child(
                                        div()
                                            .absolute()
                                            .top(u((1. - level) * STEPS as f32 * 5. - 6.))
                                            .left(u(9.))
                                            .size(u(12.))
                                            .rounded_full()
                                            .bg(gpui_kit::white()),
                                    ),
                            ),
                    ),
                )
                .with_priority(2),
            )
        })
}

fn play_button(id: SharedString, a: &Attachment, playing: bool, size: f32, blurple: bool) -> impl IntoElement {
    let url = a.url.clone();
    div()
        .id(id)
        .size(u(size))
        .flex_shrink_0()
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .cursor_pointer()
        .text_color(gpui_kit::white())
        .when(blurple, |d| d.bg(gpui_kit::rgb(0x5865f2)))
        .hover(|d| d.opacity(0.85))
        .on_click(move |_, _, cx| {
            crate::videos::toggle(&url, cx);
            if let Some((player, _)) = crate::videos::player(&url, cx) {
                player.update(cx, |p, cx| p.set_muted(MUTED.get(), cx));
            }
            // One just started takes the level already set.
            if let Some((player, _)) = crate::videos::player(&url, cx) {
                player.update(cx, |p, cx| p.set_volume(VOLUME.get(), cx));
            }
        })
        .child(Icon::new(if playing { IconName::Pause } else { IconName::Play }).size(u(size * 0.55)))
}

/// A voice message: play, its waveform to seek along, and how long it is.
pub fn voice(env: &Env, a: &Attachment, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let key = format!("{}-{}", env.message.id, a.id);
    let now = sound(a, cx);
    let shown = if now.playing || !now.position.is_zero() { now.position } else { now.duration };
    let levels = bars(&a.waveform_levels(), BARS);
    let played = gpui_kit::white();
    let waiting = theme.muted_foreground.opacity(0.6);
    let waveform = h_flex().flex_1().min_w_0().h(u(28.)).items_center().children(levels.into_iter().enumerate().map(|(i, level)| {
        let (url, duration) = (a.url.clone(), now.duration);
        let at = i as f32 / BARS as f32;
        div()
            .id(SharedString::from(format!("wave-{key}-{i}")))
            .flex_1()
            .h(u(28.))
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .on_click(move |_, _, cx| seek_to(&url, duration, at, cx))
            .child(div().w(u(2.)).h(u(3. + 17. * level)).rounded(u(1.)).bg(if at <= now.progress && now.progress > 0. { played } else { waiting }))
    }));
    h_flex()
        .relative()
        .w_full()
        .max_w(u(400.))
        .mt(u(4.))
        .h(u(52.))
        .pl(u(10.))
        .pr(u(14.))
        .gap(u(12.))
        .items_center()
        .rounded(u(26.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .text_color(theme.muted_foreground)
        .children(ticker(a, cx))
        .child(play_button(format!("voice-play-{key}").into(), a, now.playing, 32., true))
        .child(waveform)
        .child(div().flex_shrink_0().text_size(u(13.)).child(crate::videos::clock(shown)))
        .child(volume_button(format!("voice-mute-{key}").into(), a, now.muted, cx))
        .into_any_element()
}

/// Any other sound file: its name, and a player with a seek bar.
pub fn file(env: &Env, a: &Attachment, size_label: String, cx: &App) -> AnyElement {
    let theme = cx.theme();
    let key = format!("{}-{}", env.message.id, a.id);
    let now = sound(a, cx);
    let blurple = gpui_kit::Hsla::from(gpui_kit::rgb(0x5865f2));
    let seek = h_flex().flex_1().min_w_0().h(u(16.)).items_center().children((0..SEEK_CELLS).map(|i| {
        let (url, duration) = (a.url.clone(), now.duration);
        let at = i as f32 / SEEK_CELLS as f32;
        div()
            .id(SharedString::from(format!("audio-seek-{key}-{i}")))
            .flex_1()
            .h(u(16.))
            .flex()
            .items_center()
            .cursor_pointer()
            .on_click(move |_, _, cx| seek_to(&url, duration, at, cx))
            .child(div().w_full().h(u(4.)).bg(if at <= now.progress && now.progress > 0. { blurple } else { theme.muted_foreground.opacity(0.35) }))
    }));
    v_flex()
        .relative()
        .w_full()
        .max_w(u(CARD_WIDTH))
        .mt(u(4.))
        .p(u(12.))
        .gap(u(8.))
        .rounded(u(8.))
        .border_1()
        .border_color(theme.border)
        .bg(theme.secondary)
        .children(ticker(a, cx))
        .child(
            h_flex()
                .gap(u(8.))
                .items_center()
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .child(div().truncate().text_size(u(14.)).font_weight(FontWeight::MEDIUM).text_color(theme.link).child(a.filename.clone()))
                        .child(div().text_size(u(12.)).text_color(theme.muted_foreground).child(size_label)),
                )
                .child(crate::file_preview::download_button(SharedString::from(format!("audio-dl-{key}")), &a.url, &a.filename, cx)),
        )
        .child(
            h_flex()
                .gap(u(8.))
                .items_center()
                .text_size(u(12.))
                .text_color(theme.muted_foreground)
                .child(play_button(format!("audio-play-{key}").into(), a, now.playing, 28., false))
                .child(div().flex_shrink_0().child(format!("{} / {}", crate::videos::clock(now.position), crate::videos::clock(now.duration))))
                .child(seek)
                .child(volume_button(format!("audio-mute-{key}").into(), a, now.muted, cx)),
        )
        .into_any_element()
}

const CARD_WIDTH: f32 = 400.;

/// Whether this message is a voice message.
pub fn is_voice_message(flags: u64) -> bool {
    flags & FLAG_VOICE_MESSAGE != 0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn named(name: &str, kind: Option<&str>) -> Attachment {
        Attachment { id: cord_core::Id(1), filename: name.into(), url: "https://x/y".into(), proxy_url: String::new(), content_type: kind.map(String::from), width: None, height: None, size: 1, duration_secs: None, waveform: None }
    }

    #[test]
    fn knows_sound_files() {
        assert!(is_audio(&named("voice-message.ogg", Some("audio/ogg"))));
        assert!(is_audio(&named("song.MP3", None)));
        assert!(!is_audio(&named("a.png", Some("image/png"))));
    }

    #[test]
    fn waveforms_fit_any_number_of_bars() {
        assert_eq!(bars(&[0, 255], 2), vec![0., 1.]);
        assert_eq!(bars(&[255, 255, 0, 0], 2), vec![1., 0.]);
        assert_eq!(bars(&[255], 3), vec![1., 1., 1.]);
        assert_eq!(bars(&[], 2), vec![0.25, 0.25]);
    }

    #[test]
    fn the_voice_flag_is_bit_13() {
        assert!(is_voice_message(8192));
        assert!(!is_voice_message(4096));
    }
}
