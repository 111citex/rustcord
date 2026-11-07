//! Our camera: frames from it, encoded, for the network side to send, and the same frames
//! as our own preview. It runs only while the camera's on: nothing here starts it by itself.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use crate::capture::Outgoing;
use crate::video::{Picture, Pictures};

/// What the camera's asked for, and what it's sent at: Discord's camera standard.
pub const WIDTH: u32 = 1280;
pub const HEIGHT: u32 = 720;
pub const FPS: u32 = 30;
pub const BITRATE: u32 = 2_500_000;
/// The most an RTP packet carries, well under a network's 1500-byte limit with the headers,
/// transport encryption and DAVE's trailer on top.
const MTU: usize = 1100;

/// A keyframe at least this often, even unasked: a request that went missing can't leave
/// someone without a picture for longer.
const KEYFRAME_AT_LEAST: Duration = Duration::from_secs(5);

/// Someone watching asked for a keyframe (they joined, or lost packets): the next frame is one.
pub(crate) static KEYFRAME_WANTED: AtomicBool = AtomicBool::new(false);

/// The camera, while it's on. Dropping it turns it off.
pub struct CameraSend {
    stop: Arc<AtomicBool>,
}

impl CameraSend {
    /// Opens the camera and starts sending. `me` is whose preview the frames are.
    pub fn start(sink: tokio::sync::mpsc::Sender<Outgoing>, pictures: Arc<Pictures>, me: u64) -> CameraSend {
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let spawned = std::thread::Builder::new().name("camera-send".into()).spawn(move || {
            if let Err(error) = run(&sink, &pictures, me, &stopping) {
                tracing::warn!(%error, "camera: stopped");
            }
            pictures.remove(me);
            let _ = sink.blocking_send(Outgoing::VideoOff);
        });
        if let Err(error) = spawned {
            tracing::warn!(%error, "camera: couldn't start");
        }
        CameraSend { stop }
    }
}

impl Drop for CameraSend {
    fn drop(&mut self) {
        self.stop.store(true, Relaxed);
    }
}

fn run(sink: &tokio::sync::mpsc::Sender<Outgoing>, pictures: &Pictures, me: u64, stop: &AtomicBool) -> Result<(), String> {
    let camera = alhazen_encode::capture::Camera::open(None, alhazen_encode::capture::CameraWants { width: WIDTH, height: HEIGHT, fps: FPS }).map_err(|e| e.to_string())?;
    let mut encoder: Option<(Box<dyn alhazen_encode::VideoEncoder>, u32, u32)> = None;
    let mut number = 0u64;
    let mut last_keyframe = std::time::Instant::now();
    // Said on only once the first frame's encoded: cameras take seconds to wake, and viewers
    // told before then would wait on nothing, and time out on it.
    // The first frame anyone gets is a keyframe.
    KEYFRAME_WANTED.store(true, Relaxed);
    while !stop.load(Relaxed) {
        let Some(frame) = camera.take_frame() else {
            std::thread::sleep(Duration::from_millis(3));
            continue;
        };
        // An encoder for the camera's real size (it may not give exactly what was asked).
        if encoder.as_ref().is_none_or(|(_, w, h)| (*w, *h) != (frame.width, frame.height)) {
            // Constrained Baseline: what Discord's clients agree on for H.264. Phones' decoders
            // turned away High profile frames, asked for keyframes again and again, and gave up.
            let config = alhazen_encode::Config {
                codec: alhazen_encode::Codec::H264,
                h264_profile: alhazen_encode::H264Profile::ConstrainedBaseline,
                width: frame.width,
                height: frame.height,
                fps: FPS,
                bitrate: BITRATE,
            };
            let opened = alhazen_encode::open(config).map_err(|e| e.to_string())?;
            tracing::info!(encoder = opened.name(), width = frame.width, height = frame.height, "camera: encoding");
            encoder = Some((opened, frame.width, frame.height));
            KEYFRAME_WANTED.store(true, Relaxed);
        }
        let Some((encoder, _, _)) = encoder.as_mut() else { continue };
        let keyframe = KEYFRAME_WANTED.swap(false, Relaxed) || last_keyframe.elapsed() >= KEYFRAME_AT_LEAST;
        if keyframe {
            last_keyframe = std::time::Instant::now();
        }
        let stride = frame.width as usize * 4;
        let encoded = encoder.encode(&alhazen_encode::Frame { bgra: &frame.bgra, stride, pts: frame.pts }, keyframe).map_err(|e| e.to_string())?;
        if number == 0 {
            let _ = sink.blocking_send(Outgoing::VideoOn);
        }
        // RTP's video clock is 90 kHz.
        let timestamp = (frame.pts * 9 / 100) as u32;
        if sink.try_send(Outgoing::Video { frame: four_byte_start_codes(&encoded.data), keyframe: encoded.keyframe, timestamp }).is_err() {
            // The network's behind: the next frame's a keyframe, so nothing's left broken.
            KEYFRAME_WANTED.store(true, Relaxed);
        }
        // Our own preview: what we're sending, as it was before encoding.
        number += 1;
        pictures.set(me, Picture { width: frame.width, height: frame.height, bgra: Arc::from(frame.bgra), number, source: (frame.width, frame.height) });
    }
    Ok(())
}

/// The frame with a four-byte start code before every NAL unit. Receivers put the frame back
/// together that way before taking DAVE's layer off, and DAVE's record of which bytes it left
/// readable counts from the frame as it was sealed: NVENC's three-byte start codes between
/// units moved every one after the first, nothing decrypted, and viewers waited on a keyframe
/// they could never use (Discord's error 2012).
pub fn four_byte_start_codes(frame: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(frame.len() + 16);
    // One of each parameter set at most: Apple's decoder doesn't take them twice in a frame.
    let mut seen_sps = false;
    let mut seen_pps = false;
    for nal in alhazen_core::nal::split_annex_b(frame).into_iter().filter(|n| !n.is_empty()) {
        match nal[0] & 0x1f {
            7 if std::mem::replace(&mut seen_sps, true) => continue,
            8 if std::mem::replace(&mut seen_pps, true) => continue,
            _ => {}
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
    }
    out
}

/// An H.264 frame (Annex B, DAVE's layer on if any) as RTP payloads (RFC 6184): small NAL
/// units whole, big ones in FU-A pieces. The last payload ends the frame (the marker).
pub fn packetize_h264(frame: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for nal in alhazen_core::nal::split_annex_b(frame) {
        if nal.is_empty() {
            continue;
        }
        if nal.len() <= MTU {
            out.push(nal.to_vec());
            continue;
        }
        let (header, body) = (nal[0], &nal[1..]);
        let indicator = (header & 0xe0) | 28;
        let kind = header & 0x1f;
        let pieces = body.chunks(MTU - 2).count();
        for (i, piece) in body.chunks(MTU - 2).enumerate() {
            let start = if i == 0 { 0x80 } else { 0 };
            let end = if i + 1 == pieces { 0x40 } else { 0 };
            let mut payload = Vec::with_capacity(piece.len() + 2);
            payload.push(indicator);
            payload.push(start | end | kind);
            payload.extend_from_slice(piece);
            out.push(payload);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_sets_go_once() {
        let twice = [0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3];
        assert_eq!(four_byte_start_codes(&twice), vec![0, 0, 0, 1, 0x67, 1, 0, 0, 0, 1, 0x68, 2, 0, 0, 0, 1, 0x65, 3]);
    }

    #[test]
    fn start_codes_become_four_bytes() {
        let nvenc = [0, 0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 8, 0, 0, 1, 0x65, 7];
        assert_eq!(four_byte_start_codes(&nvenc), vec![0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 7]);
    }

    #[test]
    fn packets_come_back_as_the_frame() {
        // A small SPS and a big IDR: the IDR goes in pieces, and putting them back gives the frame.
        let mut frame = vec![0, 0, 0, 1, 0x67, 1, 2, 3];
        frame.extend_from_slice(&[0, 0, 0, 1, 0x65]);
        frame.extend((0..3000u32).map(|i| (i % 251) as u8 + 1));
        let packets = packetize_h264(&frame);
        assert!(packets.len() >= 4, "the IDR's split");
        assert!(packets[1..].iter().all(|p| p.len() <= MTU));
        assert_eq!(crate::video::h264_frame(&packets).unwrap(), frame);
    }
}
