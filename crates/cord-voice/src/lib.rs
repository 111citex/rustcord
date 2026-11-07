//! A voice call: the voice gateway, encrypted audio over UDP, and playing it.
//! Everything runs apart from the UI: nothing here can make
//! the audio wait on a frame being drawn, or the other way round.

mod capture;
mod jitter;
mod net;
mod playback;
mod sounds;
mod transport;
mod video;
mod video_send;

pub use video::{Picture, Pictures};

pub use capture::{InputMode, Mic, MicControls, Suppression, controls as mic, input_devices, output_devices};
pub use playback::{Looping, play_looping, play_once, set_output_device, set_output_volume};
pub use sounds::{Clip, Sound, decode_mp3, fallback as stand_in};

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
    /// Watching someone's Go Live: we only receive, and the sounds of a call don't play.
    /// `server_id` is then the stream's `rtc_server_id`, and `channel_id` one less (DAVE's group).
    pub viewer: bool,
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
    /// Packets that came after their turn to play: the line's jittery.
    late: AtomicU32,
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

    /// Packets that came too late to play.
    pub fn late(&self) -> u32 {
        self.late.load(Relaxed)
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
    /// Our microphone, while in the call; none when watching a stream.
    _mic: Option<Mic>,
    /// Holds the way to send open while there's no mic, so the network side waits on it.
    _no_mic: Option<tokio::sync::mpsc::Sender<capture::Outgoing>>,
    /// The way to the network side, for the camera.
    to_send: tokio::sync::mpsc::Sender<capture::Outgoing>,
    /// Our camera, while it's on.
    camera: std::sync::Mutex<Option<video_send::CameraSend>>,
    /// The newest picture of each camera we're getting.
    pictures: Arc<video::Pictures>,
    /// This call's own loudness, on top of the output volume: a stream's slider.
    volume: Arc<AtomicU32>,
}

impl VoiceCall {
    /// Joins, on `runtime`, with a thread of its own for playing audio.
    pub fn join(info: JoinInfo, runtime: &tokio::runtime::Handle) -> Self {
        let stats = Arc::new(Stats::default());
        let deaf = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(tokio::sync::Notify::new());
        let stop_audio = Arc::new(AtomicBool::new(false));
        let speakers = Arc::new(std::sync::Mutex::new(playback::Speakers::default()));
        let volume = Arc::new(AtomicU32::new(1f32.to_bits()));
        playback::start(speakers.clone(), stats.clone(), deaf.clone(), stop_audio.clone(), volume.clone());
        let info_viewer = info.viewer;
        let (to_send, outgoing) = tokio::sync::mpsc::channel(50);
        let pictures = Arc::new(video::Pictures::default());
        runtime.spawn(net::run(info, speakers.clone(), stats.clone(), stop.clone(), outgoing, pictures.clone()));
        let viewer = info_viewer;
        let keep = to_send.clone();
        let (mic, no_mic) = if viewer { (None, Some(to_send)) } else { (Some(Mic::start(Some(to_send))), None) };
        VoiceCall { stats, deaf, stop, stop_audio, speakers, _mic: mic, _no_mic: no_mic, to_send: keep, camera: std::sync::Mutex::new(None), pictures, volume }
    }

    /// The newest picture of `user`'s camera, while it's on and we're getting it.
    pub fn picture(&self, user: u64) -> Option<Arc<Picture>> {
        self.pictures.get(user)
    }

    /// Turns our camera on or off. Only ever because we asked: nothing turns it on by itself.
    /// `me` is our own id, under which our preview is kept.
    pub fn set_camera(&self, on: bool, me: u64) {
        let Ok(mut camera) = self.camera.lock() else { return };
        if on && camera.is_none() {
            *camera = Some(video_send::CameraSend::start(self.to_send.clone(), self.pictures.clone(), me));
        } else if !on {
            // Dropping it stops it; it says it's off as it goes.
            *camera = None;
        }
    }

    /// Whether our camera's on.
    pub fn camera_on(&self) -> bool {
        self.camera.lock().is_ok_and(|c| c.is_some())
    }

    /// How big `user`'s video is drawn, in device pixels, so it's decoded no bigger.
    pub fn show_at(&self, user: u64, size: (u32, u32)) {
        self.pictures.show_at(user, size);
    }

    /// Whose cameras we're getting.
    pub fn cameras(&self) -> Vec<u64> {
        self.pictures.users()
    }

    pub fn stats(&self) -> &Stats {
        &self.stats
    }

    /// Who's making sound right now.
    pub fn speaking(&self) -> Vec<u64> {
        self.speakers.lock().map_or_else(|_| Vec::new(), |speakers| speakers.speaking())
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

    /// How loud this call plays, on top of the output volume: 1 as sent, 0 silent, 2 double.
    pub fn set_volume(&self, volume: f32) {
        self.volume.store(volume.clamp(0., 2.).to_bits(), Relaxed);
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
