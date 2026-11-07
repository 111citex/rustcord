//! Our side of a call: the microphone, cleaned of noise, let through only while we're
//! talking (or holding the key), and encoded as Opus for the network side to send.
//!
//! One thread per microphone: the sound card's callback only copies samples into a
//! ring, and everything else happens here, so the card never waits on a model.

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering::Relaxed};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const RATE: u32 = 48_000;
/// What both noise models take at once: 10 ms.
const BLOCK: usize = 480;
/// What's encoded and sent at once: 20 ms.
const FRAME: usize = 960;
/// Voice activity stays open this many frames after the voice drops, so word ends aren't cut.
const HOLD_FRAMES: u32 = 20;
/// And push-to-talk this many after the key is let go.
const RELEASE_FRAMES: u32 = 10;
/// Quieter than this reads as nothing on the meter.
const FLOOR_DB: f32 = -100.;

/// What cleans the noise out of the mic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Suppression {
    Off = 0,
    /// RNNoise, in Rust: light, good with fans and keyboards.
    RnNoise = 1,
    /// DeepFilterNet3: closer to Krisp, a little heavier.
    DeepFilter = 2,
}

impl Suppression {
    fn from_u8(n: u8) -> Self {
        match n {
            1 => Suppression::RnNoise,
            2 => Suppression::DeepFilter,
            _ => Suppression::Off,
        }
    }
}

/// When the mic is let through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InputMode {
    VoiceActivity = 0,
    PushToTalk = 1,
}

/// The mic's settings, and what it's hearing, shared by the call, the settings page
/// and the push-to-talk key. One for the whole app.
pub struct MicControls {
    mode: AtomicU8,
    suppression: AtomicU8,
    /// Voice activity opens above this, in dB.
    threshold_db: AtomicI32,
    ptt_down: AtomicBool,
    muted: AtomicBool,
    /// The input device by name; `None` is the system's default.
    device: Mutex<Option<String>>,
    /// Set when the device changed: the mic reopens.
    reopen: AtomicBool,
    /// Last frame's loudness, in tenths of a dB.
    level: AtomicI32,
    /// Whether what we say is going out now.
    open: AtomicBool,
    /// How loud the mic is taken, as f32 bits: 1 as it comes, up to 2.
    gain: std::sync::atomic::AtomicU32,
    /// Playing ourselves back, to hear how we sound.
    monitor: AtomicBool,
}

impl MicControls {
    fn new() -> Self {
        MicControls {
            mode: AtomicU8::new(InputMode::VoiceActivity as u8),
            suppression: AtomicU8::new(Suppression::RnNoise as u8),
            threshold_db: AtomicI32::new(-50),
            ptt_down: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            device: Mutex::new(None),
            reopen: AtomicBool::new(false),
            level: AtomicI32::new((FLOOR_DB * 10.) as i32),
            open: AtomicBool::new(false),
            gain: std::sync::atomic::AtomicU32::new(1f32.to_bits()),
            monitor: AtomicBool::new(false),
        }
    }

    /// How loud the mic is taken: 1 as it comes, 0 silent, 2 double.
    pub fn set_gain(&self, gain: f32) {
        self.gain.store(gain.clamp(0., 2.).to_bits(), Relaxed);
    }

    fn gain(&self) -> f32 {
        f32::from_bits(self.gain.load(Relaxed))
    }

    /// Plays what the mic hears, cleaned as it's sent, back to us.
    pub fn set_monitor(&self, on: bool) {
        if self.monitor.swap(on, Relaxed) != on {
            self.reopen.store(true, Relaxed);
        }
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitor.load(Relaxed)
    }

    pub fn set_mode(&self, mode: InputMode) {
        self.mode.store(mode as u8, Relaxed);
    }

    pub fn mode(&self) -> InputMode {
        if self.mode.load(Relaxed) == InputMode::PushToTalk as u8 { InputMode::PushToTalk } else { InputMode::VoiceActivity }
    }

    pub fn set_suppression(&self, suppression: Suppression) {
        self.suppression.store(suppression as u8, Relaxed);
    }

    pub fn suppression(&self) -> Suppression {
        Suppression::from_u8(self.suppression.load(Relaxed))
    }

    pub fn set_threshold_db(&self, db: i32) {
        self.threshold_db.store(db.clamp(-100, 0), Relaxed);
    }

    pub fn threshold_db(&self) -> i32 {
        self.threshold_db.load(Relaxed)
    }

    /// The push-to-talk key went down or up.
    pub fn set_ptt(&self, down: bool) {
        self.ptt_down.store(down, Relaxed);
    }

    pub fn set_muted(&self, muted: bool) {
        self.muted.store(muted, Relaxed);
    }

    pub fn is_muted(&self) -> bool {
        self.muted.load(Relaxed)
    }

    /// Uses the input device called `name`, or the default for `None`.
    pub fn set_device(&self, name: Option<String>) {
        if let Ok(mut device) = self.device.lock()
            && *device != name
        {
            *device = name;
            self.reopen.store(true, Relaxed);
        }
    }

    pub fn device(&self) -> Option<String> {
        self.device.lock().ok().and_then(|d| d.clone())
    }

    /// How loud the mic is, in dB, after noise suppression.
    pub fn level_db(&self) -> f32 {
        self.level.load(Relaxed) as f32 / 10.
    }

    /// Whether we're being heard right now: the speaking ring.
    pub fn is_open(&self) -> bool {
        self.open.load(Relaxed)
    }
}

/// The app's mic settings.
pub fn controls() -> &'static MicControls {
    static CONTROLS: OnceLock<MicControls> = OnceLock::new();
    CONTROLS.get_or_init(MicControls::new)
}

/// Where devices are listed and picked from: the sound server, which knows them by the
/// names people recognise, where ALSA itself lists every plugin it has. The system's
/// default still opens as before, through ALSA's default, which goes to the same place.
pub(crate) fn named_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(host) = cpal::host_from_id(cpal::HostId::PulseAudio) {
        return host;
    }
    default_host()
}

/// The system's default device opens through ALSA, as it always did: with the sound
/// server's backend built in, cpal's own default becomes the sound server, whose
/// default buffering delays every sound noticeably.
pub(crate) fn default_host() -> cpal::Host {
    #[cfg(target_os = "linux")]
    if let Ok(host) = cpal::host_from_id(cpal::HostId::Alsa) {
        return host;
    }
    cpal::default_host()
}

fn names(devices: impl Iterator<Item = cpal::Device>) -> Vec<String> {
    let mut names: Vec<String> = devices.filter_map(|d| d.description().ok().map(|d| d.name().to_string())).collect();
    names.dedup();
    names
}

/// Names of the microphones there are. What a speaker's playing ("Monitor of …") isn't one.
pub fn input_devices() -> Vec<String> {
    let Ok(devices) = named_host().input_devices() else { return Vec::new() };
    names(devices).into_iter().filter(|n| !n.starts_with("Monitor of ")).collect()
}

/// Names of the speakers and headphones there are.
pub fn output_devices() -> Vec<String> {
    let Ok(devices) = named_host().output_devices() else { return Vec::new() };
    names(devices)
}

/// What the mic hands the network side.
pub enum Outgoing {
    /// 20 ms of our voice, Opus.
    Frame(Vec<u8>),
    /// We stopped talking.
    Stop,
    /// Our camera came on: say so, with the SSRCs our video goes on.
    VideoOn,
    /// One encoded camera frame (H.264, Annex B), its time on RTP's 90 kHz clock.
    Video { frame: Vec<u8>, keyframe: bool, timestamp: u32 },
    /// Our camera went off.
    VideoOff,
}

/// A running mic. Dropping it closes it.
pub struct Mic {
    stop: Arc<AtomicBool>,
}

impl Mic {
    /// Opens the mic. Its voice goes to `sink`; without one it only feeds the meter (the
    /// settings page's mic test).
    pub fn start(sink: Option<tokio::sync::mpsc::Sender<Outgoing>>) -> Mic {
        let stop = Arc::new(AtomicBool::new(false));
        let stopped = stop.clone();
        let spawned = std::thread::Builder::new().name("voice-mic".into()).spawn(move || run(sink, &stopped));
        if let Err(error) = spawned {
            tracing::error!(%error, "mic: couldn't start");
        }
        Mic { stop }
    }
}

impl Drop for Mic {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

/// Opens the device and works on what it hears, reopening it when it changes or fails.
fn run(sink: Option<tokio::sync::mpsc::Sender<Outgoing>>, stop: &AtomicBool) {
    let controls = controls();
    let mut work = Work::new(sink);
    while !stop.load(Relaxed) {
        controls.reopen.store(false, Relaxed);
        if let Err(error) = listen(&mut work, stop) {
            tracing::warn!(%error, "mic: trouble, trying again");
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    work.close();
    controls.level.store((FLOOR_DB * 10.) as i32, Relaxed);
}

fn find_device(name: Option<&str>) -> Option<cpal::Device> {
    if let Some(name) = name
        && let Ok(mut devices) = named_host().input_devices()
        && let Some(device) = devices.find(|d| d.description().is_ok_and(|desc| desc.name() == name))
    {
        return Some(device);
    }
    default_host().default_input_device()
}

/// One opening of the device, until it's to be reopened or stopped.
fn listen(work: &mut Work, stop: &AtomicBool) -> Result<(), String> {
    let controls = controls();
    let device = find_device(controls.device().as_deref()).ok_or("no microphone")?;
    let default = device.default_input_config().map_err(|e| e.to_string())?;
    let channels = usize::from(default.channels()).max(1);
    let buffer = small_buffer(default.buffer_size());
    let mut config: cpal::StreamConfig = default.into();
    config.sample_rate = RATE;
    config.buffer_size = buffer;
    let (mut writer, mut reader) = rtrb::RingBuffer::<f32>::new(RATE as usize);
    let failed = Arc::new(AtomicBool::new(false));
    let failing = failed.clone();
    let stream = device
        .build_input_stream(
            config,
            move |data: &[f32], _| {
                // Mono: the channels averaged.
                for frame in data.chunks(channels) {
                    let _ = writer.push(frame.iter().sum::<f32>() / channels as f32);
                }
            },
            move |error| {
                tracing::warn!(%error, "mic: input trouble");
                failing.store(true, Relaxed);
            },
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    tracing::info!(device = ?controls.device(), "mic: open");
    // Hearing ourselves: what's cleaned goes to the speakers too.
    let _monitor = if controls.is_monitoring() {
        let (to_speakers, from_mic) = rtrb::RingBuffer::<f32>::new(RATE as usize / 2);
        work.monitor = Some(to_speakers);
        monitor_stream(from_mic).map_err(|error| tracing::warn!(%error, "mic: can't play back")).ok()
    } else {
        work.monitor = None;
        None
    };
    let mut block = Vec::with_capacity(BLOCK);
    while !stop.load(Relaxed) && !controls.reopen.load(Relaxed) {
        if failed.load(Relaxed) {
            return Err("the device went away".into());
        }
        let ready = reader.slots();
        if ready < BLOCK - block.len() {
            std::thread::sleep(Duration::from_millis(4));
            continue;
        }
        let Ok(chunk) = reader.read_chunk(ready) else { continue };
        for sample in chunk {
            block.push(sample);
            if block.len() == BLOCK {
                work.block(&block);
                block.clear();
            }
        }
    }
    Ok(())
}

/// About 10 ms of sound card buffer, if the card lets us choose: its default can be
/// several times that, each way, and it all adds to the delay.
fn small_buffer(supported: &cpal::SupportedBufferSize) -> cpal::BufferSize {
    match supported {
        cpal::SupportedBufferSize::Range { min, max } => cpal::BufferSize::Fixed((BLOCK as u32).clamp(*min, *max)),
        cpal::SupportedBufferSize::Unknown => cpal::BufferSize::Default,
    }
}

/// Plays the mic back as it's cleaned, a little behind so it never runs dry.
fn monitor_stream(mut from_mic: rtrb::Consumer<f32>) -> Result<cpal::Stream, String> {
    let device = crate::playback::output_device().ok_or("no output device")?;
    let default = device.default_output_config().map_err(|e| e.to_string())?;
    let channels = usize::from(default.channels()).max(1);
    let buffer = small_buffer(default.buffer_size());
    let mut config: cpal::StreamConfig = default.into();
    config.sample_rate = RATE;
    config.buffer_size = buffer;
    let mut started = false;
    let stream = device
        .build_output_stream(
            config,
            move |data: &mut [f32], _| {
                // 20 ms in hand before starting; past 60 ms the mic has got ahead of the
                // speakers' clock, and what's extra goes, so the delay can't grow.
                if !started && from_mic.slots() >= BLOCK * 2 {
                    started = true;
                }
                let extra = from_mic.slots().saturating_sub(BLOCK * 6);
                if extra > 0
                    && let Ok(chunk) = from_mic.read_chunk(extra)
                {
                    chunk.commit_all();
                }
                for frame in data.chunks_mut(channels) {
                    let sample = if started { from_mic.pop().unwrap_or(0.) } else { 0. };
                    frame.fill(sample);
                }
            },
            |error| tracing::warn!(%error, "mic: play back trouble"),
            None,
        )
        .map_err(|e| e.to_string())?;
    stream.play().map_err(|e| e.to_string())?;
    Ok(stream)
}

/// Turning 10 ms blocks into frames to send.
struct Work {
    sink: Option<tokio::sync::mpsc::Sender<Outgoing>>,
    rnnoise: Option<Box<nnnoiseless::DenoiseState<'static>>>,
    deep: Option<deep_filter_rt::tract::DfTract>,
    /// Gave up on DeepFilterNet (it failed to load): stays off rather than retrying each block.
    deep_failed: bool,
    frame: Vec<f32>,
    encoder: Option<opus::Encoder>,
    /// Frames left before the gate closes.
    open_for: u32,
    was_open: bool,
    /// Where we hear ourselves, while that's on.
    monitor: Option<rtrb::Producer<f32>>,
}

impl Work {
    fn new(sink: Option<tokio::sync::mpsc::Sender<Outgoing>>) -> Self {
        Work { sink, rnnoise: None, deep: None, deep_failed: false, frame: Vec::with_capacity(FRAME), encoder: None, open_for: 0, was_open: false, monitor: None }
    }

    fn block(&mut self, input: &[f32]) {
        let gain = controls().gain();
        let louder: Vec<f32> = input.iter().map(|s| (s * gain).clamp(-1., 1.)).collect();
        let mut clean = [0f32; BLOCK];
        self.suppress(&louder, &mut clean);
        // The meter: every 10 ms, straight up to a louder level and easing down from it.
        let shown = controls().level_db();
        let db = loudness_db(&clean);
        let level = if db >= shown { db } else { (shown - 1.5).max(db) };
        controls().level.store((level * 10.) as i32, Relaxed);
        if let Some(monitor) = &mut self.monitor {
            for sample in clean {
                let _ = monitor.push(sample);
            }
        }
        self.frame.extend_from_slice(&clean);
        if self.frame.len() >= FRAME {
            let frame: Vec<f32> = self.frame.drain(..FRAME).collect();
            self.send(&frame);
        }
    }

    /// Takes the noise out, as chosen; models not in use are let go.
    fn suppress(&mut self, input: &[f32], out: &mut [f32; BLOCK]) {
        match controls().suppression() {
            Suppression::Off => {
                self.rnnoise = None;
                self.deep = None;
                out.copy_from_slice(input);
            }
            Suppression::RnNoise => {
                self.deep = None;
                // RNNoise works on 16-bit sized numbers.
                let scaled: Vec<f32> = input.iter().map(|s| s * 32_767.).collect();
                let state = self.rnnoise.get_or_insert_with(nnnoiseless::DenoiseState::new);
                state.process_frame(out, &scaled);
                for sample in out.iter_mut() {
                    *sample /= 32_767.;
                }
            }
            Suppression::DeepFilter => {
                self.rnnoise = None;
                if self.deep.is_none() && !self.deep_failed {
                    match std::panic::catch_unwind(deep_filter_rt::tract::DfTract::default) {
                        Ok(model) => self.deep = Some(model),
                        Err(_) => {
                            tracing::warn!("mic: DeepFilterNet wouldn't load, sending as is");
                            self.deep_failed = true;
                        }
                    }
                }
                let Some(model) = &mut self.deep else {
                    out.copy_from_slice(input);
                    return;
                };
                let noisy = ndarray::ArrayView2::from_shape((1, BLOCK), input).expect("one channel of a block");
                let enhanced = ndarray::ArrayViewMut2::from_shape((1, BLOCK), &mut out[..]).expect("one channel of a block");
                if let Err(error) = model.process(noisy, enhanced) {
                    tracing::warn!(%error, "mic: DeepFilterNet failed on a block");
                    out.copy_from_slice(input);
                }
            }
        }
    }

    /// Measures a frame, decides whether it goes, and sends it if so.
    fn send(&mut self, frame: &[f32]) {
        let controls = controls();
        let db = loudness_db(frame);
        let wanted = match controls.mode() {
            InputMode::VoiceActivity => db >= controls.threshold_db() as f32,
            InputMode::PushToTalk => controls.ptt_down.load(Relaxed),
        };
        let hold = if controls.mode() == InputMode::PushToTalk { RELEASE_FRAMES } else { HOLD_FRAMES };
        self.open_for = if wanted { hold } else { self.open_for.saturating_sub(1) };
        let open = self.open_for > 0 && !controls.is_muted();
        controls.open.store(open && self.sink.is_some(), Relaxed);
        if !open {
            if self.was_open {
                self.close();
            }
            return;
        }
        self.was_open = true;
        let Some(sink) = self.sink.clone() else { return };
        let Some(encoder) = self.encoder() else { return };
        // Discord's Opus is stereo: the same voice in both ears.
        let stereo: Vec<f32> = frame.iter().flat_map(|s| [*s, *s]).collect();
        match encoder.encode_vec_float(&stereo, 4000) {
            Ok(packet) => {
                let _ = sink.try_send(Outgoing::Frame(packet));
            }
            Err(error) => tracing::warn!(%error, "mic: couldn't encode"),
        }
    }

    fn encoder(&mut self) -> Option<&mut opus::Encoder> {
        if self.encoder.is_none() {
            match opus::Encoder::new(RATE, opus::Channels::Stereo, opus::Application::Voip) {
                Ok(mut encoder) => {
                    let _ = encoder.set_bitrate(opus::Bitrate::Bits(64_000));
                    // Some of each frame rides along in the next, to cover a lost one.
                    let _ = encoder.set_inband_fec(true);
                    let _ = encoder.set_packet_loss_perc(10);
                    self.encoder = Some(encoder);
                }
                Err(error) => {
                    tracing::warn!(%error, "mic: no Opus encoder");
                    return None;
                }
            }
        }
        self.encoder.as_mut()
    }

    /// We've stopped talking: the network side sends the silence that ends it.
    fn close(&mut self) {
        self.was_open = false;
        controls().open.store(false, Relaxed);
        if let Some(sink) = &self.sink {
            let _ = sink.try_send(Outgoing::Stop);
        }
        if let Some(encoder) = &mut self.encoder {
            let _ = encoder.reset_state();
        }
    }
}

/// A frame's loudness: its RMS in dB, where full scale is 0.
fn loudness_db(frame: &[f32]) -> f32 {
    if frame.is_empty() {
        return FLOOR_DB;
    }
    let mean = frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32;
    if mean <= 0. { FLOOR_DB } else { (10. * mean.log10()).max(FLOOR_DB) }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loudness_is_in_db_of_full_scale() {
        assert_eq!(loudness_db(&[0.; FRAME]), FLOOR_DB);
        assert!((loudness_db(&[1.; FRAME])).abs() < 0.01);
        assert!((loudness_db(&[0.1; FRAME]) + 20.).abs() < 0.01);
    }

    /// What this machine has: `cargo test -p cord-voice -- --ignored --nocapture lists`.
    #[test]
    #[ignore = "needs a sound server"]
    fn lists_devices() {
        println!("inputs: {:#?}", input_devices());
        println!("outputs: {:#?}", output_devices());
    }

    #[test]
    fn suppressors_keep_the_block_size() {
        assert_eq!(nnnoiseless::DenoiseState::FRAME_SIZE, BLOCK);
    }
}
