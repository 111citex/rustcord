//! The little sounds a call makes. Discord's own are fetched by the app when it can get
//! them (see `set_sound`); until then, and if it can't, these made-up tones stand in.

use std::collections::HashMap;
use std::sync::Arc;

const RATE: u32 = 48_000;

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
pub enum Sound {
    /// We've started joining.
    Joining,
    /// We're in.
    Connected,
    SomeoneJoined,
    SomeoneLeft,
    Deafened,
    Undeafened,
    Muted,
    Unmuted,
    /// We left the call: played on its own, after the call's gone.
    Disconnected,
    /// The push-to-talk key, down and up.
    PttStart,
    PttStop,
    StreamStarted,
    StreamEnded,
    /// Someone's calling us.
    Incoming,
    /// We're calling someone and they haven't answered.
    Outgoing,
}

/// Stereo samples at 48 kHz.
pub type Clip = Arc<[f32]>;

/// A rising or falling pair of soft notes.
fn tones(notes: &[(f32, f32)]) -> Clip {
    let mut out = Vec::new();
    for (frequency, seconds) in notes {
        let count = (RATE as f32 * seconds) as usize;
        for i in 0..count {
            let t = i as f32 / RATE as f32;
            let place = i as f32 / count.max(1) as f32;
            // In and out softly, or the edges click.
            let envelope = (place * 40.).min(1.) * (1. - place).powi(2);
            let sample = (t * frequency * std::f32::consts::TAU).sin() * envelope * 0.25;
            out.push(sample);
            out.push(sample);
        }
    }
    out.into()
}

pub fn fallback(sound: Sound) -> Clip {
    match sound {
        Sound::Joining => tones(&[(660., 0.09)]),
        Sound::Connected | Sound::SomeoneJoined => tones(&[(660., 0.09), (880., 0.14)]),
        Sound::SomeoneLeft => tones(&[(880., 0.09), (660., 0.14)]),
        Sound::Deafened => tones(&[(520., 0.07), (390., 0.11)]),
        Sound::Undeafened => tones(&[(390., 0.07), (520., 0.11)]),
        Sound::Muted => tones(&[(600., 0.06), (450., 0.09)]),
        Sound::Unmuted => tones(&[(450., 0.06), (600., 0.09)]),
        Sound::Disconnected => tones(&[(660., 0.09), (440., 0.16)]),
        Sound::PttStart => tones(&[(700., 0.04)]),
        Sound::PttStop => tones(&[(500., 0.04)]),
        Sound::StreamStarted => tones(&[(520., 0.08), (780., 0.14)]),
        Sound::StreamEnded => tones(&[(780., 0.08), (520., 0.14)]),
        // A ring is its two notes twice; the gap between rings comes from the loop.
        Sound::Incoming => tones(&[(784., 0.18), (988., 0.18), (784., 0.18), (988., 0.3)]),
        Sound::Outgoing => tones(&[(440., 0.35), (440., 0.35)]),
    }
}

/// Clips to play, by sound: whatever's been loaded, or the stand-ins.
#[derive(Default)]
pub struct Bank {
    loaded: HashMap<Sound, Clip>,
}

impl Bank {
    pub fn get(&self, sound: Sound) -> Clip {
        self.loaded.get(&sound).cloned().unwrap_or_else(|| fallback(sound))
    }

    pub fn set(&mut self, sound: Sound, clip: Clip) {
        self.loaded.insert(sound, clip);
    }
}

/// `input` (interleaved, at `rate`, `channels` wide) as stereo at 48 kHz.
pub fn to_stereo_48k(input: &[f32], rate: u32, channels: usize) -> Vec<f32> {
    let channels = channels.max(1);
    let frames = input.len() / channels;
    if frames == 0 || rate == 0 {
        return Vec::new();
    }
    let at = |frame: usize, side: usize| input[frame * channels + side.min(channels - 1)];
    let out_frames = (frames as u64 * u64::from(RATE) / u64::from(rate)) as usize;
    let mut out = Vec::with_capacity(out_frames * 2);
    for i in 0..out_frames {
        let place = i as f64 * f64::from(rate) / f64::from(RATE);
        let (whole, part) = (place as usize, place.fract() as f32);
        let next = (whole + 1).min(frames - 1);
        for side in 0..2 {
            out.push(at(whole, side) * (1. - part) + at(next, side) * part);
        }
    }
    out
}

/// A whole MP3 file, as a clip. `None` if it's no good.
pub fn decode_mp3(bytes: Vec<u8>) -> Option<Clip> {
    use symphonia::core::audio::SampleBuffer;
    use symphonia::core::codecs::{CODEC_TYPE_NULL, DecoderOptions};
    use symphonia::core::formats::FormatOptions;
    use symphonia::core::io::MediaSourceStream;
    use symphonia::core::meta::MetadataOptions;
    use symphonia::core::probe::Hint;

    let source = MediaSourceStream::new(Box::new(std::io::Cursor::new(bytes)), Default::default());
    let mut hint = Hint::new();
    hint.with_extension("mp3");
    let mut format = symphonia::default::get_probe()
        .format(&hint, source, &FormatOptions::default(), &MetadataOptions::default())
        .ok()?
        .format;
    let track = format.tracks().iter().find(|t| t.codec_params.codec != CODEC_TYPE_NULL)?;
    let (id, params) = (track.id, track.codec_params.clone());
    let mut decoder = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).ok()?;
    let (mut samples, mut rate, mut channels) = (Vec::new(), 0, 0);
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != id {
            continue;
        }
        let Ok(decoded) = decoder.decode(&packet) else { continue };
        let spec = *decoded.spec();
        rate = spec.rate;
        channels = spec.channels.count();
        let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buffer.copy_interleaved_ref(decoded);
        samples.extend_from_slice(buffer.samples());
    }
    let stereo = to_stereo_48k(&samples, rate, channels);
    (!stereo.is_empty()).then(|| stereo.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stand_ins_are_short_and_quiet_and_start_silently() {
        for sound in [Sound::Joining, Sound::Connected, Sound::SomeoneJoined, Sound::SomeoneLeft, Sound::Deafened, Sound::Undeafened] {
            let clip = fallback(sound);
            assert!(clip.len() < RATE as usize, "{sound:?} is under half a second");
            assert!(clip.iter().all(|s| s.abs() <= 0.3));
            assert!(clip[0].abs() < 0.01, "no click at the start");
        }
    }

    #[test]
    fn rings_are_quiet_and_start_silently() {
        for sound in [Sound::Incoming, Sound::Outgoing] {
            let clip = fallback(sound);
            assert!(clip.len() < RATE as usize * 4, "{sound:?} is under two seconds");
            assert!(clip.iter().all(|s| s.abs() <= 0.3));
            assert!(clip[0].abs() < 0.01);
        }
    }

    #[test]
    fn audio_converts_to_stereo_at_48k() {
        let mono: Vec<f32> = vec![0.5; 24_000];
        let out = to_stereo_48k(&mono, 24_000, 1);
        assert_eq!(out.len(), 48_000 * 2);
        assert!(out.iter().all(|s| (*s - 0.5).abs() < 0.001));
        assert!(to_stereo_48k(&[], 48_000, 2).is_empty());
    }

    #[test]
    fn the_bank_prefers_what_was_loaded() {
        let mut bank = Bank::default();
        assert_eq!(bank.get(Sound::Deafened).len(), fallback(Sound::Deafened).len());
        bank.set(Sound::Deafened, vec![0.1, 0.1].into());
        assert_eq!(bank.get(Sound::Deafened).len(), 2);
    }

    #[test]
    fn junk_isnt_a_sound() {
        assert!(decode_mp3(vec![1, 2, 3, 4]).is_none());
    }
}
