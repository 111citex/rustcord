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
    let mut plan: Vec<(u32, Next, f32)> = {
        let Ok(mut speakers) = speakers.lock() else { return false };
        let ssrcs: Vec<u32> = speakers.queues.keys().copied().collect();
        ssrcs
            .into_iter()
            .filter_map(|ssrc| {
                let gain = speakers.gain(ssrc);
                let queue = speakers.queues.get_mut(&ssrc)?;
                let next = queue.next();
                stats.skipped.fetch_add(queue.take_skipped(), Relaxed);
                Some((ssrc, next, gain))
            })
            .collect()
    };
    plan.retain(|(_, next, _)| *next != Next::Nothing);
    let mut any = false;
    let mut one = [0f32; FRAME];
    for (ssrc, next, gain) in plan {
        if let std::collections::hash_map::Entry::Vacant(slot) = voices.entry(ssrc)
            && let Ok(decoder) = Decoder::new(RATE, Channels::Stereo)
        {
            slot.insert(Voice { decoder, heard: Instant::now() });
        }
        let Some(voice) = voices.get_mut(&ssrc) else { continue };
        voice.heard = Instant::now();
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
        for (mixed, sample) in out.iter_mut().zip(&one[..(frames * 2).min(FRAME)]) {
            *mixed += sample * gain;
        }
        any = true;
    }
    voices.retain(|_, voice| voice.heard.elapsed() < FORGET_AFTER);
    if any {
        for sample in out.iter_mut() {
            *sample = limit(*sample);
        }
    }
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
    while !stop.load(Relaxed) {
        if writer.buffer().capacity() - writer.slots() >= AHEAD {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let any = mix(speakers, &mut voices, stats, &mut frame);
        if deaf.load(Relaxed) || !any {
            frame.fill(0.);
        }
        if let Ok(mut speakers) = speakers.lock() {
            speakers.mix_sounds(&mut frame);
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
}
