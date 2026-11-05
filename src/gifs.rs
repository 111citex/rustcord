//! Looping, muted playback for GIF embeds (Discord's `gifv`: an MP4 posing
//! as a GIF). Players live in a small app-wide pool: the chat list warms the
//! ones for the rows it is about to draw, and the embeds look them up. The
//! least recently drawn are dropped past `POOL`, which stops their decoding.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use alhazen_gpui::{PlayerConfig, PlayerEvent, PlayerState, Source, VideoPlayer};
use cord_core::model::Message;
use gpui_kit::{App, AppContext, Entity, Global, Subscription};

const POOL: usize = 6;
/// Off screen this long, a GIF stops decoding.
const REST_AFTER: Duration = Duration::from_millis(1500);
/// And this long, its player goes, with its threads and buffers.
const DROP_AFTER: Duration = Duration::from_secs(20);

struct Gif {
    url: String,
    player: Entity<VideoPlayer>,
    _looping: Subscription,
    /// When its row was last built: it's on screen, or about to be.
    seen: Instant,
    /// Paused here for being off screen, as opposed to the window losing focus.
    resting: bool,
}

#[derive(Default)]
struct Gifs {
    players: VecDeque<Gif>,
}

impl Global for Gifs {}

/// The video behind a gifv embed, if the message has one.
pub fn gif_videos(message: &Message) -> impl Iterator<Item = &str> {
    message
        .embeds
        .iter()
        .filter(|e| e.kind.as_deref() == Some("gifv"))
        .filter_map(|e| e.video.as_ref().map(|v| v.display_url()))
}

/// Starts (or keeps) a player for each GIF in `message`.
pub fn warm(message: &Message, cx: &mut App) {
    for url in gif_videos(message) {
        warm_url(url, cx);
    }
}

/// Starts (or keeps) a looping, muted player for `url`.
pub fn warm_url(url: &str, cx: &mut App) {
    if !cx.has_global::<Gifs>() {
        cx.set_global(Gifs::default());
    }
    let players = &mut cx.global_mut::<Gifs>().players;
    if let Some(i) = players.iter().position(|g| g.url == url) {
        let mut entry = players.remove(i).expect("index in range");
        entry.seen = Instant::now();
        let wake = std::mem::take(&mut entry.resting).then(|| entry.player.clone());
        players.push_back(entry);
        if let Some(player) = wake {
            player.update(cx, |p, cx| p.play(cx));
        }
        return;
    }
    let Ok(source) = Source::parse(url) else { return };
    crate::perf::count("gif_player_started");
    let config = PlayerConfig { autoplay: true, ..PlayerConfig::default() };
    let player = cx.new(|cx| {
        let mut player = VideoPlayer::new(source, config, cx);
        player.set_muted(true, cx);
        player
    });
    let looping = cx.subscribe(&player, |player, event: &PlayerEvent, cx| match event {
        // GIFs loop.
        PlayerEvent::StateChanged(PlayerState::Ended) => player.update(cx, |p, cx| {
            p.seek(Duration::ZERO, cx);
            p.play(cx);
        }),
        // The view only keeps asking for frames once it has painted a playing
        // player; the chat list doesn't observe players, so wake it here.
        PlayerEvent::StateChanged(PlayerState::Playing) => crate::ui::redraw_chat(cx),
        _ => {}
    });
    let players = &mut cx.global_mut::<Gifs>().players;
    players.push_back(Gif { url: url.to_string(), player, _looping: looping, seen: Instant::now(), resting: false });
    while players.len() > POOL {
        players.pop_front();
    }
}

/// Pauses GIFs scrolled off screen, and lets go of those long gone.
pub fn rest_unseen(cx: &mut App) {
    if !cx.has_global::<Gifs>() {
        return;
    }
    let now = Instant::now();
    cx.global_mut::<Gifs>().players.retain(|g| now.duration_since(g.seen) < DROP_AFTER);
    let stale: Vec<_> = cx.global::<Gifs>().players.iter().filter(|g| !g.resting && now.duration_since(g.seen) > REST_AFTER).map(|g| g.player.clone()).collect();
    for player in &stale {
        if player.read(cx).is_playing() {
            player.update(cx, |p, cx| p.pause(cx));
        }
    }
    for gif in cx.global_mut::<Gifs>().players.iter_mut() {
        if stale.iter().any(|p| *p == gif.player) {
            gif.resting = true;
        }
    }
}

/// Stops or restarts every GIF: they rest while the window isn't focused.
pub fn set_playing(playing: bool, cx: &mut App) {
    let Some(gifs) = cx.try_global::<Gifs>() else { return };
    let players: Vec<_> = gifs.players.iter().map(|g| g.player.clone()).collect();
    for player in players {
        player.update(cx, |p, cx| if playing { p.play(cx) } else { p.pause(cx) });
    }
}

/// The player for `url`, once `warm` has started one.
pub fn player(url: &str, cx: &App) -> Option<Entity<VideoPlayer>> {
    cx.try_global::<Gifs>()?.players.iter().find(|g| g.url == url).map(|g| g.player.clone())
}
