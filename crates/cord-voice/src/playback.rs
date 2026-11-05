//! Plays a call: one thread decodes and mixes 20 ms at a time into a buffer, and the
//! audio callback only ever reads from it, so it never waits on anything.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use opus::{Channels, Decoder};

use crate::Stats;
use crate::jitter::{Jitter, Next};
use crate::sounds::{Bank, Clip, Sound};

/// Discord's silence packets are this long, and don't count as speaking.
const SILENCE_PACKET: usize = 3;
/// How long after the last sound someone still counts as speaking.
const SPEAKING_FOR: std::time::Duration = std::time::Duration::from_millis(350);

const RATE: u32 = 48_000;
/// Samples in 20 ms of stereo.
const FRAME: usize = 960 * 2;
/// How much mixed audio is kept ahead of the sound card.
const AHEAD: usize = FRAME * 2;
/// Someone silent this long gets their decoder let go of.
const FORGET_AFTER: Duration = Duration::from_secs(30);

#[derive(Default)]
pub struct Speakers {
    queues: HashMap<u32, Jitter>,
    /// Whose each stream is, and how loud each person's wanted: 1 is as sent.
    users: HashMap<u32, u64>,
    /// When real sound (not a silence packet) last came from each stream.
    heard: HashMap<u32, Instant>,
    gains: HashMap<u64, f32>,
    pub bank: Bank,
    /// Sounds under way, and how far through each is.
    playing: Vec<(Clip, usize)>,
}

impl Speakers {
    pub fn push(&mut self, ssrc: u32, seq: u16, payload: Vec<u8>) {
        if payload.len() > SILENCE_PACKET {
            self.heard.insert(ssrc, Instant::now());
        }
        self.queues.entry(ssrc).or_default().push(seq, payload);
    }

    pub fn remove(&mut self, ssrc: u32) {
        self.queues.remove(&ssrc);
        self.users.remove(&ssrc);
        self.heard.remove(&ssrc);
    }

    /// Who's making sound right now.
    pub fn speaking(&self) -> Vec<u64> {
        let now = Instant::now();
        self.heard.iter().filter(|(_, at)| now - **at < SPEAKING_FOR).filter_map(|(ssrc, _)| self.users.get(ssrc).copied()).collect()
    }

    pub fn play_sound(&mut self, sound: Sound) {
        let clip = self.bank.get(sound);
        self.playing.push((clip, 0));
    }

    /// Adds what's playing to `out`.
    fn mix_sounds(&mut self, out: &mut [f32]) {
        for (clip, at) in &mut self.playing {
            for (mixed, sample) in out.iter_mut().zip(&clip[*at..]) {
                *mixed += sample;
            }
            *at = (*at + out.len()).min(clip.len());
        }
        self.playing.retain(|(clip, at)| *at < clip.len());
    }

    pub fn set_user(&mut self, ssrc: u32, user: u64) {
        self.users.insert(ssrc, user);
    }

    pub fn set_gain(&mut self, user: u64, gain: f32) {
        self.gains.insert(user, gain);
    }

    fn gain(&self, ssrc: u32) -> f32 {
        self.users.get(&ssrc).and_then(|user| self.gains.get(user)).copied().unwrap_or(1.)
    }
}

struct Voice {
    decoder: Decoder,
    heard: Instant,
    /// Sound is coming from them right now (the last frame wasn't the end of it).
    live: bool,
    /// The last stereo sample played before their volume, to ease out of at the end.
    last: [f32; 2],
    /// The volume the last frame ended at, so the next one moves on from it.
    gain: f32,
}

/// Fades in and out take this many sample frames: 5 ms. Short enough not to blunt a
/// word's start, long enough that nothing steps from silence to sound in one sample.
const FADE: usize = 240;

/// Brings sound in from nothing over the first [`FADE`] frames of `samples` (stereo).
fn fade_in(samples: &mut [f32]) {
    for (i, frame) in samples.chunks_exact_mut(2).take(FADE).enumerate() {
        let level = (i + 1) as f32 / FADE as f32;
        frame[0] *= level;
        frame[1] *= level;
    }
}

/// Adds the last sound played, dying away over [`FADE`] frames: what goes on after
/// someone stops, instead of cutting to silence from wherever the wave was.
fn ease_out(last: [f32; 2], out: &mut [f32]) {
    for (i, frame) in out.chunks_exact_mut(2).take(FADE).enumerate() {
        let level = 1. - (i + 1) as f32 / FADE as f32;
        frame[0] += last[0] * level;
        frame[1] += last[1] * level;
    }
}

/// Scales `samples` from volume `from` at the start to `to` at the end, so that changing
/// it, by a slider or a mute, never steps.
fn ramp(from: f32, to: f32, samples: &mut [f32]) {
    if from == to {
        if to != 1. {
            samples.iter_mut().for_each(|s| *s *= to);
        }
        return;
    }
    let frames = (samples.len() / 2).max(1) as f32;
    for (i, frame) in samples.chunks_exact_mut(2).enumerate() {
        let level = from + (to - from) * (i + 1) as f32 / frames;
        frame[0] *= level;
        frame[1] *= level;
    }
}

/// Keeps loud moments from clipping, leaving everything quieter alone.
fn limit(sample: f32) -> f32 {
    const KNEE: f32 = 0.8;
    let size = sample.abs();
    if size <= KNEE {
        return sample;
    }
    let over = size - KNEE;
    sample.signum() * (KNEE + (1. - KNEE) * (over / (1. - KNEE + over)))
}

/// Mixes the next 20 ms into `out`. Whether anyone's in it.
fn mix(speakers: &Mutex<Speakers>, voices: &mut HashMap<u32, Voice>, stats: &Stats, out: &mut [f32]) -> bool {
    out.fill(0.);
    let plan: Vec<(u32, Next, f32, bool)> = {
        let Ok(mut speakers) = speakers.lock() else { return false };
        let ssrcs: Vec<u32> = speakers.queues.keys().copied().collect();
        ssrcs
            .into_iter()
            .filter_map(|ssrc| {
                let gain = speakers.gain(ssrc);
                let queue = speakers.queues.get_mut(&ssrc)?;
                let next = queue.next();
                stats.skipped.fetch_add(queue.take_skipped(), Relaxed);
                Some((ssrc, next, gain, queue.take_restarted()))
            })
            .collect()
    };
    let mut any = false;
    let mut one = [0f32; FRAME];
    for (ssrc, next, gain, restarted) in plan {
        if next == Next::Nothing {
            // They've stopped: what's left of the wave dies away rather than being cut.
            if let Some(voice) = voices.get_mut(&ssrc).filter(|v| v.live) {
                voice.live = false;
                ease_out([voice.last[0] * voice.gain, voice.last[1] * voice.gain], out);
                any = true;
            }
            continue;
        }
        if let std::collections::hash_map::Entry::Vacant(slot) = voices.entry(ssrc)
            && let Ok(decoder) = Decoder::new(RATE, Channels::Stereo)
        {
            slot.insert(Voice { decoder, heard: Instant::now(), live: false, last: [0.; 2], gain });
        }
        let Some(voice) = voices.get_mut(&ssrc) else { continue };
        voice.heard = Instant::now();
        // Starting from silence: the decoder is still as the last burst left it, which
        // made the first frame click, so it starts afresh and the sound comes in.
        let starting = !voice.live || restarted;
        if starting {
            let _ = voice.decoder.reset_state();
        }
        let decoded = match &next {
            Next::Play(packet) => voice.decoder.decode_float(packet, &mut one, false),
            Next::Recover(packet) => voice.decoder.decode_float(packet, &mut one, true),
            Next::Conceal => voice.decoder.decode_float(&[], &mut one, false),
            Next::Nothing => continue,
        };
        if !matches!(next, Next::Play(_)) {
            stats.concealed.fetch_add(1, Relaxed);
        }
        stats.played.fetch_add(1, Relaxed);
        let Ok(frames) = decoded else { continue };
        let len = (frames * 2).min(FRAME);
        if len < 2 {
            continue;
        }
        let heard = &mut one[..len];
        if starting {
            fade_in(heard);
            voice.gain = gain;
        }
        voice.live = true;
        voice.last = [heard[len - 2], heard[len - 1]];
        ramp(voice.gain, gain, heard);
        voice.gain = gain;
        for (mixed, sample) in out.iter_mut().zip(heard.iter()) {
            *mixed += sample;
        }
        any = true;
    }
    voices.retain(|_, voice| voice.heard.elapsed() < FORGET_AFTER);
    any
}

/// Starts playing on a thread of its own, until `stop`.
/// Plays one sound on its own, in or out of a call.
pub fn play_once(clip: Clip) {
    let spawned = std::thread::Builder::new().name("sound".into()).spawn(move || {
        if let Err(error) = sound_alone(&clip) {
            tracing::warn!(%error, "couldn't play a sound");
        }
    });
    if let Err(error) = spawned {
        tracing::warn!(%error, "couldn't start a sound");
    }
}

fn sound_alone(clip: &Clip) -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = usize::from(default.channels()).max(1);
    let mut config: cpal::StreamConfig = default.into();
    config.sample_rate = RATE;
    let (samples, mut at) = (clip.clone(), 0usize);
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                for frame in data.chunks_mut(channels) {
                    let (left, right) = (samples.get(at).copied().unwrap_or(0.), samples.get(at + 1).copied().unwrap_or(0.));
                    at += 2;
                    for (i, sample) in frame.iter_mut().enumerate() {
                        *sample = if i % 2 == 0 { left } else { right };
                    }
                }
            },
            |error| tracing::warn!(%error, "sound: output trouble"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    // Stereo at 48 kHz, and a little over so that the end isn't cut off.
    std::thread::sleep(std::time::Duration::from_millis(clip.len() as u64 / 96 + 200));
    Ok(())
}

pub fn start(speakers: Arc<Mutex<Speakers>>, stats: Arc<Stats>, deaf: Arc<AtomicBool>, stop: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new().name("voice-playback".into()).spawn(move || {
        if let Err(error) = play(&speakers, &stats, &deaf, &stop) {
            tracing::error!(%error, "voice: couldn't play audio");
        }
    });
    if let Err(error) = spawned {
        tracing::error!(%error, "voice: couldn't start playing");
    }
}

fn play(speakers: &Mutex<Speakers>, stats: &Stats, deaf: &AtomicBool, stop: &AtomicBool) -> Result<(), String> {
    let device = cpal::default_host().default_output_device().ok_or("no output device")?;
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = default.channels() as usize;
    let mut config: cpal::StreamConfig = default.into();
    config.sample_rate = RATE;
    let (mut writer, mut reader) = rtrb::RingBuffer::<f32>::new(FRAME * 8);
    let underruns = stats.underruns.clone();
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                let mut ran_dry = false;
                for frame in data.chunks_mut(channels.max(1)) {
                    let (left, right) = match (reader.pop(), reader.pop()) {
                        (Ok(left), Ok(right)) => (left, right),
                        _ => {
                            ran_dry = true;
                            (0., 0.)
                        }
                    };
                    for (i, sample) in frame.iter_mut().enumerate() {
                        *sample = if i % 2 == 0 { left } else { right };
                    }
                }
                if ran_dry {
                    underruns.fetch_add(1, Relaxed);
                }
            },
            |error| tracing::warn!(%error, "voice: output trouble"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;

    let mut voices = HashMap::new();
    let mut frame = vec![0f32; FRAME];
    let mut heard_level = 1f32;
    while !stop.load(Relaxed) {
        if writer.buffer().capacity() - writer.slots() >= AHEAD {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let any = mix(speakers, &mut voices, stats, &mut frame);
        // Deafening fades the call out over the frame, and undeafening back in.
        let wanted = if deaf.load(Relaxed) { 0. } else { 1. };
        if any {
            ramp(heard_level, wanted, &mut frame);
        } else {
            frame.fill(0.);
        }
        heard_level = wanted;
        if let Ok(mut speakers) = speakers.lock() {
            speakers.mix_sounds(&mut frame);
        }
        // Voices and sounds together may pass full scale: the limiter keeps them from clipping.
        for sample in frame.iter_mut() {
            *sample = limit(*sample);
        }
        for sample in &frame {
            if writer.push(*sample).is_err() {
                break;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_limiter_leaves_normal_levels_alone_and_never_clips() {
        assert_eq!(limit(0.5), 0.5);
        assert_eq!(limit(-0.8), -0.8);
        for loud in [0.9f32, 1.5, 4., 50.] {
            assert!(limit(loud) < 1. && limit(loud) > 0.8, "{loud} became {}", limit(loud));
            assert_eq!(limit(-loud), -limit(loud));
        }
    }

    #[test]
    fn louder_stays_louder_through_the_limiter() {
        assert!(limit(2.) > limit(1.2));
    }

    #[test]
    fn a_fade_in_starts_from_silence_and_reaches_full() {
        let mut samples = vec![1f32; FADE * 2 + 40];
        fade_in(&mut samples);
        assert!(samples[0] < 0.01 && samples[1] < 0.01, "the first sample is nearly nothing");
        assert!((samples[FADE * 2 - 1] - 1.).abs() < 1e-6, "full by the end of the fade");
        assert_eq!(samples[FADE * 2 + 5], 1., "and untouched after it");
        assert!(samples.windows(2).step_by(2).all(|w| w[0] <= w[1] + 1e-6), "rising");
    }

    #[test]
    fn easing_out_goes_from_where_the_wave_was_to_nothing() {
        let mut out = vec![0f32; FADE * 2 + 8];
        ease_out([0.5, -0.5], &mut out);
        assert!(out[0] > 0.49, "it carries on from the last sample");
        assert!(out[FADE * 2 - 2].abs() < 1e-6 && out[FADE * 2 - 1].abs() < 1e-6, "and is gone by the end");
        assert_eq!(out[FADE * 2 + 2], 0.);
    }

    #[test]
    fn a_volume_change_moves_in_steps_no_bigger_than_its_share() {
        let mut samples = vec![1f32; 960 * 2];
        ramp(0., 1., &mut samples);
        let steps: Vec<f32> = samples.chunks_exact(2).map(|f| f[0]).collect::<Vec<_>>().windows(2).map(|w| w[1] - w[0]).collect();
        assert!(steps.iter().all(|s| (s - 1. / 960.).abs() < 1e-5), "an even slope, not a jump");
        assert!((samples[samples.len() - 1] - 1.).abs() < 1e-6);
        let mut same = vec![0.5f32; 8];
        ramp(1., 1., &mut same);
        assert_eq!(same, vec![0.5; 8]);
        ramp(0.5, 0.5, &mut same);
        assert_eq!(same, vec![0.25; 8]);
    }
}
