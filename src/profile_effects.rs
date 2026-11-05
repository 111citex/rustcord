//! Profile effects: animated pictures played over a profile. Each is a few APNG
//! layers with their own timing: most play once as the card opens, then one
//! repeats with a pause between. Their frames are big, hundreds of MB if all decoded,
//! so a layer keeps its file and a thread decodes the next few frames as they're due.

use std::cell::RefCell;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::time::{Duration, Instant};

use cord_core::Core;
use cord_core::profile::{EffectLayer, ProfileEffect};
use gpui_kit::{App, AsyncApp, Bounds, ContentMask, Corners, IntoElement, ObjectFit, Pixels, RenderImage, Styled, canvas, point, px, size};

/// Layers kept. Each is its file and a few frames; one effect can have several.
const KEPT: usize = 8;
/// Frames decoded ahead, on top of the one the thread is holding.
const AHEAD: usize = 2;
/// Frames kept after being replaced: a redraw can still be showing one.
const KEEP_OLD: usize = 2;
/// Closed for this long, an effect's frames go. They're the biggest pictures around.
const REST_AFTER: Duration = Duration::from_secs(15);

/// A decoded frame, and when it starts in the animation.
struct Timed {
    frame: image::Frame,
    begin: Duration,
    /// How many times the animation had played through before this frame.
    pass: u64,
}

/// Decodes frames for as long as somebody's receiving them, over and over.
fn decode(bytes: Arc<[u8]>, out: SyncSender<Timed>) {
    use image::AnimationDecoder;
    let mut pass = 0;
    loop {
        let Ok(decoder) = image::codecs::png::PngDecoder::new(std::io::Cursor::new(bytes.clone())) else { return };
        let Ok(frames) = decoder.apng() else { return };
        let (mut begin, mut any) = (Duration::ZERO, false);
        for frame in frames.into_frames() {
            let Ok(mut frame) = frame else { return };
            any = true;
            let delay = Duration::from(frame.delay());
            // GPUI's images are BGRA.
            for pixel in frame.buffer_mut().chunks_exact_mut(4) {
                pixel.swap(0, 2);
            }
            if out.send(Timed { frame, begin, pass }).is_err() {
                return;
            }
            begin += delay;
        }
        if !any {
            return;
        }
        pass += 1;
    }
}

/// What to do with the next decoded frame.
#[derive(Debug, PartialEq)]
enum Next {
    /// It's from an earlier time through, or already past.
    Skip,
    /// It's due.
    Show,
    /// Not time yet.
    Wait,
}

fn next_for(frame_pass: u64, begin: Duration, pass: u64, at: Duration) -> Next {
    if frame_pass < pass {
        Next::Skip
    } else if frame_pass > pass || begin > at {
        Next::Wait
    } else {
        Next::Show
    }
}

/// One layer playing.
struct Playback {
    bytes: Arc<[u8]>,
    frames: Option<Receiver<Timed>>,
    /// The frame that's come out of `frames` and isn't due yet.
    pending: Option<Timed>,
    /// The time through that's wanted, and how far in it was last drawn.
    pass: u64,
    last_at: Option<Duration>,
    /// Which showing of the effect this is playing for.
    began: Option<Instant>,
    current: Option<Arc<RenderImage>>,
    old: Vec<Arc<RenderImage>>,
    failed: bool,
}

impl Playback {
    fn new(bytes: Vec<u8>) -> Self {
        let mut playback = Playback {
            bytes: Arc::from(bytes),
            frames: None,
            pending: None,
            pass: 0,
            last_at: None,
            began: None,
            current: None,
            old: Vec::new(),
            failed: false,
        };
        playback.start();
        playback
    }

    fn start(&mut self) {
        let (out, frames) = sync_channel(AHEAD);
        let bytes = self.bytes.clone();
        let _ = std::thread::Builder::new().name("effect-decode".into()).spawn(move || decode(bytes, out));
        self.frames = Some(frames);
        self.pending = None;
        self.pass = 0;
        self.last_at = None;
    }

    /// Takes the next frame off the decoder, if there is one. A decoder that ended
    /// before giving a single frame means the file's no good.
    fn fetch(&mut self) {
        let (None, Some(frames)) = (&self.pending, &self.frames) else { return };
        match frames.try_recv() {
            Ok(timed) => self.pending = Some(timed),
            Err(TryRecvError::Empty) => {}
            Err(TryRecvError::Disconnected) => {
                self.failed = self.current.is_none();
                self.frames = None;
            }
        }
    }

    /// Whether it has a frame to start with.
    fn ready(&mut self) -> bool {
        if self.frames.is_none() && !self.failed {
            self.start();
        }
        self.fetch();
        self.pending.is_some() || self.current.is_some()
    }

    /// Moves on to the frame for `at` into the animation. Returns frames no longer needed.
    fn advance(&mut self, at: Duration, began: Instant) -> Vec<Arc<RenderImage>> {
        if self.began.replace(began) != Some(began) && self.began.is_some() && (self.pass > 0 || self.last_at.is_some()) {
            // The profile was opened again: from the top, without the old last frame.
            self.old.extend(self.current.take());
            self.start();
        }
        if self.frames.is_none() && !self.failed {
            self.start();
        }
        if self.last_at.is_some_and(|last| at < last) {
            self.pass += 1;
        }
        self.last_at = Some(at);
        let mut latest = None;
        loop {
            self.fetch();
            let Some(timed) = self.pending.take() else { break };
            match next_for(timed.pass, timed.begin, self.pass, at) {
                Next::Skip => {}
                Next::Show => latest = Some(timed.frame),
                Next::Wait => {
                    self.pending = Some(timed);
                    break;
                }
            }
        }
        if let Some(frame) = latest
            && let Some(previous) = self.current.replace(Arc::new(RenderImage::new([frame])))
        {
            self.old.push(previous);
        }
        let excess = self.old.len().saturating_sub(KEEP_OLD);
        self.old.drain(..excess).collect()
    }

    /// Done playing: keeps only the file, in case the profile's opened again.
    fn finish(&mut self) -> Vec<Arc<RenderImage>> {
        self.frames = None;
        self.pending = None;
        self.old.drain(..).chain(self.current.take()).collect()
    }
}

enum State {
    Loading,
    Ready(Playback),
    Failed,
}

thread_local! {
    static LAYERS: RefCell<Vec<(String, State)>> = const { RefCell::new(Vec::new()) };
}

/// Starts decoding the effect's layers.
pub fn warm(effect: &ProfileEffect, core: &Arc<Core>, cx: &mut App) {
    for layer in &effect.effects {
        let src = layer.src.clone();
        if LAYERS.with_borrow(|l| l.iter().any(|(known, _)| *known == src)) {
            continue;
        }
        crate::anim::note_used(&src);
        let dropped = LAYERS.with_borrow_mut(|l| {
            l.push((src.clone(), State::Loading));
            if l.len() > KEPT { Some(l.remove(0).1) } else { None }
        });
        if let Some(State::Ready(mut playback)) = dropped {
            for image in playback.finish() {
                cx.drop_image(image, None);
            }
        }
        let core = core.clone();
        cx.spawn(async move |cx: &mut AsyncApp| {
            let bytes = crate::image_disk::cdn_bytes(&core, src.clone(), cx).await;
            LAYERS.with_borrow_mut(|l| {
                if let Some((_, state)) = l.iter_mut().find(|(known, _)| *known == src) {
                    *state = match bytes {
                        Some(bytes) => State::Ready(Playback::new(bytes)),
                        None => State::Failed,
                    };
                }
            });
            let _ = cx.update(|cx| cx.refresh_windows());
        })
        .detach();
    }
}

/// How far into its own animation a layer is `t` after the effect began, if it's
/// showing at all then.
fn layer_time(layer: &EffectLayer, t: Duration) -> Option<Duration> {
    let (start, length) = (Duration::from_millis(layer.start), Duration::from_millis(layer.duration));
    let into = t.checked_sub(start)?;
    if layer.looping {
        let period = length + Duration::from_millis(layer.loop_delay);
        if period.is_zero() {
            return None;
        }
        let at = Duration::from_nanos((into.as_nanos() % period.as_nanos()) as u64);
        (at < length).then_some(at)
    } else {
        (into < length).then_some(into)
    }
}

/// When anything next needs drawing, `t` after the effect began. `None`: never.
fn next_wake(layers: &[EffectLayer], t: Duration) -> Option<Duration> {
    const FRAME: Duration = Duration::from_millis(33);
    if layers.iter().any(|l| layer_time(l, t).is_some()) {
        return Some(FRAME);
    }
    layers
        .iter()
        .filter_map(|l| {
            let start = Duration::from_millis(l.start);
            if t < start {
                return Some(start - t);
            }
            l.looping.then(|| {
                let period = Duration::from_millis(l.duration + l.loop_delay).max(FRAME);
                let into = Duration::from_nanos(((t - start).as_nanos() % period.as_nanos()) as u64);
                period - into
            })
        })
        .min()
        .map(|wait| wait.max(FRAME))
}

/// Lets go of effects that aren't showing.
pub fn rest(cx: &mut App) {
    let gone: Vec<State> = LAYERS.with_borrow_mut(|l| {
        let mut gone = Vec::new();
        l.retain_mut(|(src, state)| {
            if crate::anim::unused_for(src, REST_AFTER) && !matches!(state, State::Loading) {
                gone.push(std::mem::replace(state, State::Failed));
                false
            } else {
                true
            }
        });
        gone
    });
    for state in gone {
        if let State::Ready(mut playback) = state {
            for image in playback.finish() {
                cx.drop_image(image, None);
            }
        }
    }
}

/// Draws the effect over its parent (`relative`, clipping), as wide as the
/// parent and from its top, starting once every layer is ready.
pub fn overlay(effect: ProfileEffect, began: std::rc::Rc<std::cell::Cell<Option<Instant>>>, radius: f32) -> impl IntoElement {
    canvas(|_, _, _| {}, move |bounds: Bounds<Pixels>, _, window, cx| {
        for layer in &effect.effects {
            crate::anim::note_used(&layer.src);
        }
        // Starts once every layer has a frame to show. Only until then: a layer that's
        // finished is done with its frames, and asking would start it up again.
        let ready = began.get().is_some() || LAYERS.with_borrow_mut(|l| {
            effect.effects.iter().all(|layer| {
                match l.iter_mut().find(|(known, _)| *known == layer.src).map(|(_, s)| s) {
                    Some(State::Ready(playback)) => playback.ready() || playback.failed,
                    Some(State::Failed) => true,
                    _ => false,
                }
            })
        });
        if !ready {
            // The decoder doesn't wake anything by itself.
            alhazen_gpui::redraw_after(Duration::from_millis(33), window, cx);
            return;
        }
        let began_at = began.get().unwrap_or_else(|| {
            let now = Instant::now();
            began.set(Some(now));
            now
        });
        let t = began_at.elapsed();
        let mut retired = Vec::new();
        let mut showing: Vec<(i64, Arc<RenderImage>)> = LAYERS.with_borrow_mut(|l| {
            effect
                .effects
                .iter()
                .filter_map(|layer| {
                    let Some((_, State::Ready(playback))) = l.iter_mut().find(|(known, _)| *known == layer.src) else { return None };
                    let Some(at) = layer_time(layer, t) else {
                        if !layer.looping && t >= Duration::from_millis(layer.start + layer.duration) {
                            retired.extend(playback.finish());
                        }
                        return None;
                    };
                    retired.extend(playback.advance(at, began_at));
                    Some((layer.z_index as i64, playback.current.clone()?))
                })
                .collect()
        });
        for image in retired {
            let _ = window.drop_image(image);
        }
        showing.sort_by_key(|(z_index, _)| *z_index);
        // The art's as tall as the tallest of profiles: what's past this card's end is cut.
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            for (_, image) in &showing {
                let natural = image.size(0);
                let scale = f32::from(bounds.size.width) / (natural.width.0 as f32).max(1.);
                // A taller column than the art, like the full profile: stretched down to its end.
                let height = (natural.height.0 as f32 * scale).max(f32::from(bounds.size.height));
                let area = Bounds::new(point(bounds.left(), bounds.top()), size(bounds.size.width, px(height)));
                let corners = Corners::all(window.rem_size() * (radius / 16.));
                let _ = window.paint_image(area, ObjectFit::Fill.get_bounds(area, natural), corners, image.clone(), 0, false);
            }
        });
        let layers: Vec<EffectLayer> = effect.effects.clone();
        if let Some(wait) = next_wake(&layers, t) {
            alhazen_gpui::redraw_after(wait, window, cx);
        }
    })
    .absolute()
    .top_0()
    .left_0()
    .size_full()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn once(start: u64, duration: u64) -> EffectLayer {
        EffectLayer { src: String::new(), looping: false, duration, start, loop_delay: 0, z_index: 0 }
    }

    fn repeats(start: u64, duration: u64, loop_delay: u64) -> EffectLayer {
        EffectLayer { looping: true, loop_delay, ..once(start, duration) }
    }

    const MS: fn(u64) -> Duration = Duration::from_millis;

    #[test]
    fn frames_show_when_due_and_only_for_this_time_through() {
        assert_eq!(next_for(0, MS(100), 0, MS(50)), Next::Wait, "not time yet");
        assert_eq!(next_for(0, MS(100), 0, MS(100)), Next::Show);
        assert_eq!(next_for(0, MS(100), 1, MS(0)), Next::Skip, "left over from the last time through");
        assert_eq!(next_for(1, MS(0), 0, MS(900)), Next::Wait, "the next time through hasn't begun");
    }

    #[test]
    fn a_once_layer_plays_once() {
        let l = once(100, 1000);
        assert_eq!(layer_time(&l, MS(50)), None);
        assert_eq!(layer_time(&l, MS(600)), Some(MS(500)));
        assert_eq!(layer_time(&l, MS(1100)), None);
    }

    #[test]
    fn a_looping_layer_repeats_with_its_pause() {
        // Discord's Boost Relic: 2880 ms from 5760, then every 2880 + 5760.
        let l = repeats(5760, 2880, 5760);
        assert_eq!(layer_time(&l, MS(5000)), None);
        assert_eq!(layer_time(&l, MS(5760)), Some(MS(0)));
        assert_eq!(layer_time(&l, MS(8000)), Some(MS(2240)));
        assert_eq!(layer_time(&l, MS(9000)), None);
        assert_eq!(layer_time(&l, MS(5760 + 8640)), Some(MS(0)));
    }

    #[test]
    fn wakes_only_when_there_is_something_to_draw() {
        let layers = [once(0, 1000), repeats(5000, 1000, 4000)];
        assert_eq!(next_wake(&layers, MS(500)), Some(MS(33)));
        // Between them: sleeps until the loop starts.
        assert_eq!(next_wake(&layers, MS(2000)), Some(MS(3000)));
        // In the loop's pause: until it comes round.
        assert_eq!(next_wake(&layers, MS(7000)), Some(MS(3000)));
        // Nothing that repeats: done for good.
        assert_eq!(next_wake(&[once(0, 1000)], MS(2000)), None);
    }
}
