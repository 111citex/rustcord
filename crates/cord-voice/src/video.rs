//! Other people's video: their RTP packets put back into frames, then decoded on a
//! thread of each person's own into pictures the UI shows.
//!
//! The network side (`Inbound`) only reorders packets and rebuilds frames, which is
//! cheap. DAVE comes off whole frames (it was put on before packetizing), and what's
//! left goes to a decoder thread, which hands the newest picture to `Pictures`.

use std::collections::{BTreeMap, HashMap};
use std::sync::mpsc::{Receiver, SyncSender, TrySendError, sync_channel};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// RTP payload types, as offered in Select Protocol. Each has its RTX one above it.
pub const H264: u8 = 101;
pub const VP8: u8 = 105;
pub const VP9: u8 = 107;
pub const AV1: u8 = 109;

/// Asks again for a keyframe this often while one's missing.
const PLI_EVERY: Duration = Duration::from_millis(700);
/// Packets kept for a frame not yet whole, before giving it up.
const MOST_HELD: usize = 2000;

/// What we offer to send and receive, for Select Protocol.
pub fn codecs() -> serde_json::Value {
    // `encode`: whether we can send it. Said false, the server took us for unable to send
    // H.264, and what our camera sent never reached anyone as video they could start.
    let video = |name: &str, pt: u8, encode: bool| {
        serde_json::json!({"name": name, "type": "video", "priority": 1000, "payload_type": pt, "rtx_payload_type": pt + 1, "encode": encode, "decode": true})
    };
    serde_json::json!([
        {"name": "opus", "type": "audio", "priority": 1000, "payload_type": 120},
        // Only what can be put back together yet: VP8, VP9 and AV1 come next.
        video("H264", H264, true),
    ])
}

/// Whether `pt` is one of ours for video, and if it's a resend, which it's a resend of.
pub fn video_payload(pt: u8) -> Option<(u8, bool)> {
    match pt {
        H264 | VP8 | VP9 | AV1 => Some((pt, false)),
        p if [H264, VP8, VP9, AV1].contains(&(p.wrapping_sub(1))) => Some((p - 1, true)),
        _ => None,
    }
}

/// A picture to show: tightly packed BGRA.
pub struct Picture {
    pub width: u32,
    pub height: u32,
    pub bgra: Arc<[u8]>,
    /// Goes up with each new picture, so the UI can tell it's new.
    pub number: u64,
    /// The video's own size, as sent, before it was scaled to where it's shown.
    pub source: (u32, u32),
}

/// The newest picture of everyone whose video we're getting.
#[derive(Default)]
pub struct Pictures {
    by_user: Mutex<HashMap<u64, Arc<Picture>>>,
    /// How big each person's video is shown, in device pixels: the decoder makes frames no
    /// bigger (NVDEC scales them for free), instead of 1440p for a box a third of that.
    shown_at: Mutex<HashMap<u64, ((u32, u32), std::time::Instant)>>,
}

impl Pictures {
    pub fn get(&self, user: u64) -> Option<Arc<Picture>> {
        self.by_user.lock().ok()?.get(&user).cloned()
    }

    /// The size `user`'s video was last drawn at.
    /// Drawn twice at once (big, and in a thumbnail), the bigger wins: what was asked for
    /// lately and is larger stays, so the big view isn't decoded at the small one's size.
    ///
    /// Each change makes the decoder start scaling afresh, so it changes as little as it can:
    /// bigger at once (sharp straight away), smaller only once nothing's asked for the bigger
    /// size for a second, and not for a few pixels either way (a resize, a layout settling).
    pub fn show_at(&self, user: u64, size: (u32, u32)) {
        const SHRINK_AFTER: std::time::Duration = std::time::Duration::from_secs(1);
        let area = |(w, h): (u32, u32)| u64::from(w) * u64::from(h);
        if let Ok(mut map) = self.shown_at.lock() {
            let now = std::time::Instant::now();
            let Some((held, asked)) = map.get(&user).copied() else {
                map.insert(user, (size, now));
                return;
            };
            let (old, new) = (area(held), area(size));
            if new * 100 > old * 132 {
                // Clearly bigger: now.
                map.insert(user, (size, now));
            } else if new * 132 >= old * 100 {
                // About the same: still wanted, so it stays.
                map.insert(user, (held, now));
            } else if now.duration_since(asked) >= SHRINK_AFTER {
                // Smaller, and the bigger size hasn't been asked for in a while.
                map.insert(user, (size, now));
            }
        }
    }

    fn shown_size(&self, user: u64) -> Option<(u32, u32)> {
        self.shown_at.lock().ok()?.get(&user).map(|(size, _)| *size)
    }

    pub fn users(&self) -> Vec<u64> {
        self.by_user.lock().map(|m| m.keys().copied().collect()).unwrap_or_default()
    }

    pub(crate) fn set(&self, user: u64, picture: Picture) {
        if let Ok(mut map) = self.by_user.lock() {
            map.insert(user, Arc::new(picture));
        }
    }

    pub(crate) fn remove(&self, user: u64) {
        if let Ok(mut map) = self.by_user.lock() {
            map.remove(&user);
        }
    }
}

/// One encoded frame, whole, DAVE still on.
pub struct Frame {
    pub codec: u8,
    pub data: Vec<u8>,
    pub keyframe: bool,
}

/// One video SSRC's packets, being put back into frames.
pub struct Inbound {
    codec: u8,
    /// Packets by sequence number (kept as distance from `from`, so wrapping is fine).
    held: BTreeMap<u32, (u32, bool, Vec<u8>)>,
    /// Sequence number the next frame starts at, once known.
    from: Option<u16>,
    /// A frame was lost: nothing decodes until the next keyframe.
    pub broken: bool,
    last_pli: Option<Instant>,
    decoder: SyncSender<Frame>,
    _thread: Option<std::thread::JoinHandle<()>>,
}

impl Inbound {
    pub fn new(user: u64, codec: u8, pictures: Arc<Pictures>) -> Inbound {
        let (decoder, frames) = sync_channel(60);
        let thread = std::thread::Builder::new().name("video-decode".into()).spawn(move || decode(user, &frames, &pictures)).ok();
        Inbound { codec, held: BTreeMap::new(), from: None, broken: true, last_pli: None, decoder, _thread: thread }
    }

    /// Whether a keyframe should be asked for now (a PLI sent).
    pub fn wants_keyframe(&mut self) -> bool {
        if !self.broken {
            return false;
        }
        let due = self.last_pli.is_none_or(|at| at.elapsed() >= PLI_EVERY);
        if due {
            self.last_pli = Some(Instant::now());
        }
        due
    }

    /// Takes a packet; returns a frame when one's complete.
    pub fn push(&mut self, seq: u16, timestamp: u32, marker: bool, payload: Vec<u8>) -> Option<Frame> {
        let from = *self.from.get_or_insert(seq);
        let ahead = seq.wrapping_sub(from);
        if ahead >= 0x8000 {
            return None; // Older than the frame being built: already given up on.
        }
        self.held.insert(u32::from(ahead), (timestamp, marker, payload));
        if self.held.len() > MOST_HELD {
            self.lose();
            return None;
        }
        self.complete()
    }

    /// A frame's packets all here: the run from `from` up to a marker, with no gaps.
    fn complete(&mut self) -> Option<Frame> {
        let end = self.held.iter().find(|(_, (_, marker, _))| *marker).map(|(k, _)| *k)?;
        let have = self.held.range(..=end).count() as u32;
        if have != end + 1 {
            // Packets missing before the marker. A later frame's marker means they're lost.
            if self.held.range(end + 1..).any(|(_, (_, marker, _))| *marker) {
                self.lose();
            }
            return None;
        }
        let packets: Vec<Vec<u8>> = (0..=end).filter_map(|k| self.held.remove(&k)).map(|(_, _, p)| p).collect();
        let shift = end + 1;
        self.held = std::mem::take(&mut self.held).into_iter().map(|(k, v)| (k - shift, v)).collect();
        self.from = self.from.map(|f| f.wrapping_add(shift as u16));
        let data = match self.codec {
            H264 => h264_frame(&packets)?,
            _ => return None,
        };
        let keyframe = match self.codec {
            H264 => h264_is_keyframe(&data),
            _ => false,
        };
        if keyframe {
            self.broken = false;
        }
        if self.broken {
            return None;
        }
        Some(Frame { codec: self.codec, data, keyframe })
    }

    /// Gives up on what's held: wait for the next keyframe.
    fn lose(&mut self) {
        self.held.clear();
        self.from = None;
        self.broken = true;
    }

    /// Hands a frame, DAVE off, to the decoder.
    pub fn decode(&mut self, frame: Frame) {
        if let Err(TrySendError::Full(_)) = self.decoder.try_send(frame) {
            // The decoder's behind: skip to the next keyframe rather than queue up delay.
            self.broken = true;
        }
    }
}

/// Puts H.264 RTP payloads (RFC 6184) back into an Annex B frame, every NAL unit with a
/// four-byte start code, as the sender's encoder made it and DAVE saw it.
pub fn h264_frame(packets: &[Vec<u8>]) -> Option<Vec<u8>> {
    const START: [u8; 4] = [0, 0, 0, 1];
    let mut out = Vec::new();
    for payload in packets {
        let header = *payload.first()?;
        match header & 0x1f {
            1..=23 => {
                out.extend_from_slice(&START);
                out.extend_from_slice(payload);
            }
            // STAP-A: several NAL units, each with a 16-bit size.
            24 => {
                let mut at = 1;
                while at + 2 <= payload.len() {
                    let size = usize::from(u16::from_be_bytes([payload[at], payload[at + 1]]));
                    at += 2;
                    let nal = payload.get(at..at + size)?;
                    out.extend_from_slice(&START);
                    out.extend_from_slice(nal);
                    at += size;
                }
            }
            // FU-A: one NAL unit in pieces; the first carries its header.
            28 => {
                let fu = *payload.get(1)?;
                if fu & 0x80 != 0 {
                    out.extend_from_slice(&START);
                    out.push((header & 0xe0) | (fu & 0x1f));
                }
                out.extend_from_slice(payload.get(2..)?);
            }
            _ => return None,
        }
    }
    (!out.is_empty()).then_some(out)
}

/// The NAL units of an Annex B frame.
pub fn nal_units(frame: &[u8]) -> Vec<&[u8]> {
    alhazen_core::nal::split_annex_b(frame)
}

fn h264_is_keyframe(frame: &[u8]) -> bool {
    nal_units(frame).iter().any(|n| n.first().is_some_and(|h| h & 0x1f == 5))
}

/// Decodes one person's frames until the network side lets go.
fn decode(user: u64, frames: &Receiver<Frame>, pictures: &Pictures) {
    let registry = alhazen_core::backend::Registry::with_defaults();
    let mut decoder: Option<Box<dyn alhazen_core::decode::VideoDecoder>> = None;
    let mut params: Option<(Vec<u8>, Vec<u8>)> = None;
    let mut source = (0, 0);
    let mut number = 0u64;
    let mut bgra = Vec::new();
    let started = Instant::now();
    while let Ok(frame) = frames.recv() {
        if frame.codec != H264 {
            continue;
        }
        let units = nal_units(&frame.data);
        let sps = units.iter().find(|n| n.first().is_some_and(|h| h & 0x1f == 7));
        let pps = units.iter().find(|n| n.first().is_some_and(|h| h & 0x1f == 8));
        if let (Some(sps), Some(pps)) = (sps, pps) {
            let new = (sps.to_vec(), pps.to_vec());
            // New parameters (a new size): a new decoder.
            if params.as_ref() != Some(&new) {
                let mut stream = alhazen_core::demux::StreamInfo::new(0, alhazen_core::demux::StreamKind::Video, alhazen_core::demux::Codec::H264);
                if let Some((w, h)) = alhazen_core::nal::h264_sps_size(sps) {
                    stream.width = w;
                    stream.height = h;
                }
                stream.extradata = Some(alhazen_core::nal::avcc_from(sps, pps));
                let prefs = alhazen_core::OutputPrefs { gpu_bgra: true, native: false, ten_bit: false };
                match registry.open_video_decoder(&stream, 2, prefs, None) {
                    Ok(opened) => {
                        tracing::info!(user, width = stream.width, height = stream.height, "video: decoding");
                        source = (stream.width, stream.height);
                        decoder = Some(opened);
                        params = Some(new);
                    }
                    Err(error) => {
                        tracing::warn!(user, %error, "video: no decoder");
                        continue;
                    }
                }
            }
        }
        let Some(dec) = decoder.as_mut() else { continue };
        dec.set_output_hint(pictures.shown_size(user));
        // Length-prefixed, as the decoders take it; the parameter sets are in its setup.
        let mut data = Vec::with_capacity(frame.data.len());
        for nal in units.iter().filter(|n| !n.first().is_some_and(|h| matches!(h & 0x1f, 7..=9))) {
            data.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            data.extend_from_slice(nal);
        }
        let packet = alhazen_core::demux::Packet { stream: 0, pts: started.elapsed(), keyframe: frame.keyframe, data, generation: 0 };
        if let Err(error) = dec.send_packet(&packet) {
            tracing::debug!(user, %error, "video: couldn't decode a frame");
            continue;
        }
        while let Ok(Some(out)) = dec.receive_frame() {
            let picture = match out {
                alhazen_core::decode::DecodedFrame::Bgra { width, height, bgra, .. } => Picture { width, height, bgra, number, source },
                alhazen_core::decode::DecodedFrame::Yuv(yuv) => {
                    if alhazen_core::convert::frame_to_bgra(&yuv, &mut bgra).is_err() {
                        continue;
                    }
                    Picture { width: yuv.width, height: yuv.height, bgra: Arc::from(bgra.as_slice()), number, source }
                }
                #[allow(unreachable_patterns)]
                _ => continue,
            };
            number += 1;
            pictures.set(user, picture);
        }
    }
    pictures.remove(user);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn h264_packets_come_back_as_annex_b() {
        // A single NAL, a STAP-A with SPS+PPS, and an IDR in two FU-A pieces.
        let single = vec![0x41, 1, 2];
        let stap = vec![0x18, 0, 2, 0x67, 9, 0, 1, 0x68];
        let fu_start = vec![0x7c, 0x85, 0xAA];
        let fu_end = vec![0x7c, 0x45, 0xBB];
        let frame = h264_frame(&[stap, fu_start, fu_end, single]).unwrap();
        assert_eq!(frame, vec![0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 0, 0, 0, 1, 0x65, 0xAA, 0xBB, 0, 0, 0, 1, 0x41, 1, 2]);
        assert!(h264_is_keyframe(&frame));
    }

    #[test]
    fn resends_are_known_by_their_payload_type() {
        assert_eq!(video_payload(101), Some((101, false)));
        assert_eq!(video_payload(102), Some((101, true)));
        assert_eq!(video_payload(110), Some((109, true)));
        assert_eq!(video_payload(120), None);
    }
}
