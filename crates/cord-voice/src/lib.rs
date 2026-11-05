//! A voice call: the voice gateway, encrypted audio over UDP, and playing it.
//! So far it only listens. Everything runs apart from the UI: nothing here can make
//! the audio wait on a frame being drawn, or the other way round.

mod jitter;
mod net;
mod playback;
mod sounds;
mod transport;

pub use sounds::{Sound, decode_mp3};

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, Ordering::Relaxed};

/// What's needed to join, from the main gateway. Not `Debug`: it holds a token.
pub struct JoinInfo {
    /// Host of the voice server for this call.
    pub endpoint: String,
    /// The server, or for a private call the channel.
    pub server_id: u64,
    pub channel_id: u64,
    pub user_id: u64,
    pub session_id: String,
    pub token: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Connecting,
    Connected,
    /// It ended, and won't try again.
    Closed,
}

/// How a call is doing, for the UI to look at whenever it likes.
#[derive(Default)]
pub struct Stats {
    state: AtomicU8,
    ping_ms: AtomicU32,
    /// Packets that arrived too late or never, and were covered over.
    concealed: AtomicU32,
    played: AtomicU32,
    /// Frames thrown away to catch up, having fallen behind.
    skipped: AtomicU32,
    /// Times the sound card asked for audio that wasn't ready.
    underruns: Arc<AtomicU32>,
}

impl Stats {
    pub fn state(&self) -> State {
        match self.state.load(Relaxed) {
            0 => State::Connecting,
            1 => State::Connected,
            _ => State::Closed,
        }
    }

    fn set_state(&self, state: State) {
        self.state.store(state as u8, Relaxed);
    }

    /// Round trip to the voice server. `None` until it's been measured.
    pub fn ping_ms(&self) -> Option<u32> {
        Some(self.ping_ms.load(Relaxed)).filter(|ms| *ms > 0)
    }

    /// Frames covered over, and frames played, since the call began.
    pub fn frames(&self) -> (u32, u32) {
        (self.concealed.load(Relaxed), self.played.load(Relaxed))
    }

    /// Frames skipped to catch up, and times the sound card ran dry. Clicks are one of these.
    pub fn glitches(&self) -> (u32, u32) {
        (self.skipped.load(Relaxed), self.underruns.load(Relaxed))
    }
}

/// A call in progress. Dropping it hangs up.
pub struct VoiceCall {
    stats: Arc<Stats>,
    deaf: Arc<AtomicBool>,
    stop: Arc<tokio::sync::Notify>,
    stop_audio: Arc<AtomicBool>,
    speakers: Arc<std::sync::Mutex<playback::Speakers>>,
}

impl VoiceCall {
    /// Joins, on `runtime`, with a thread of its own for playing audio.
    pub fn join(info: JoinInfo, runtime: &tokio::runtime::Handle) -> Self {
        let stats = Arc::new(Stats::default());
        let deaf = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_audio = Arc::new(AtomicBool::new(false));
        let speakers = Arc::new(std::sync::Mutex::new(playback::Speakers::default()));
        playback::start(speakers.clone(), stats.clone(), deaf.clone(), stop_audio.clone());
        runtime.spawn(net::run(info, speakers.clone(), stats.clone(), stop.clone()));
        VoiceCall { stats, deaf, stop, stop_audio, speakers }
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Plays one of the call's little sounds, deafened or not.
    pub fn play_sound(&self, sound: Sound) {
        if let Ok(mut speakers) = self.speakers.lock() {
            speakers.play_sound(sound);
        }
    }

    /// Uses this for `sound` from now on, instead of the stand-in.
    pub fn set_sound(&self, sound: Sound, clip: sounds::Clip) {
        if let Ok(mut speakers) = self.speakers.lock() {
            speakers.bank.set(sound, clip);
        }
    }

    /// How loud someone is for us: 1 is as sent, 0 silent, 2 double.
    pub fn set_user_gain(&self, user: u64, gain: f32) {
        if let Ok(mut speakers) = self.speakers.lock() {
            speakers.set_gain(user, gain);
        }
    }

    pub fn set_deaf(&self, deaf: bool) {
        self.deaf.store(deaf, Relaxed);
    }

    pub fn is_deaf(&self) -> bool {
        self.deaf.load(Relaxed)
    }
}

impl Drop for VoiceCall {
    fn drop(&mut self) {
        self.stop.notify_one();
        self.stop_audio.store(true, Relaxed);
    }
}
