//! The layer of encryption on every UDP packet, between us and Discord's server.
//! DAVE, inside this, is what's between us and the others in the call.

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};

const TAG: usize = 16;
const COUNTER: usize = 4;
const RTP_HEADER: usize = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    Aes,
    XChaCha,
}

impl Mode {
    /// The mode to use of those offered: AES when there is, as Discord asks.
    pub fn choose(offered: &[String]) -> Option<Self> {
        let has = |name: &str| offered.iter().any(|m| m == name);
        if has("aead_aes256_gcm_rtpsize") {
            Some(Mode::Aes)
        } else if has("aead_xchacha20_poly1305_rtpsize") {
            Some(Mode::XChaCha)
        } else {
            None
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Mode::Aes => "aead_aes256_gcm_rtpsize",
            Mode::XChaCha => "aead_xchacha20_poly1305_rtpsize",
        }
    }
}

/// A packet of audio, its outer encryption off.
#[derive(Debug, PartialEq, Eq)]
pub struct Rtp {
    pub seq: u16,
    pub ssrc: u32,
    pub payload: Vec<u8>,
    pub payload_type: u8,
    pub marker: bool,
    pub timestamp: u32,
}

/// How long the part of the header that's readable is.
fn clear_length(packet: &[u8]) -> Option<usize> {
    let first = *packet.first()?;
    let csrcs = (first & 0x0f) as usize;
    let extension = first & 0x10 != 0;
    let length = RTP_HEADER + csrcs * 4 + if extension { 4 } else { 0 };
    (packet.len() >= length).then_some(length)
}

fn open(mode: Mode, key: &[u8; 32], counter: &[u8], sealed: &[u8], clear: &[u8]) -> Option<Vec<u8>> {
    let payload = Payload { msg: sealed, aad: clear };
    match mode {
        Mode::Aes => {
            let mut nonce = [0u8; 12];
            nonce[..COUNTER].copy_from_slice(counter);
            Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)).decrypt(Nonce::from_slice(&nonce), payload).ok()
        }
        Mode::XChaCha => {
            let mut nonce = [0u8; 24];
            nonce[..COUNTER].copy_from_slice(counter);
            XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key)).decrypt(XNonce::from_slice(&nonce), payload).ok()
        }
    }
}

/// Reads a packet: `None` if it isn't audio, or doesn't decrypt.
pub fn open_packet(mode: Mode, key: &[u8; 32], packet: &[u8]) -> Option<Rtp> {
    if packet.len() < RTP_HEADER + TAG + COUNTER || packet[0] >> 6 != 2 {
        return None;
    }
    // Sender and receiver reports share the port.
    if (200..=204).contains(&packet[1]) {
        return None;
    }
    let clear = clear_length(packet)?;
    let (body, counter) = packet.split_at(packet.len() - COUNTER);
    let sealed = body.get(clear..)?;
    let mut opened = open(mode, key, counter, sealed, &body[..clear])?;
    // The extension's contents are inside the encryption, ahead of the audio.
    if packet[0] & 0x10 != 0 {
        let words = u16::from_be_bytes([packet[clear - 2], packet[clear - 1]]) as usize;
        let skip = words * 4;
        if skip > opened.len() {
            return None;
        }
        opened.drain(..skip);
    }
    // Padding: filler at the end, its own length in its last byte. Video's often padded,
    // and left on it ends up inside the frame, which then fails DAVE's check.
    if packet[0] & 0x20 != 0 {
        let pad = usize::from(*opened.last()?);
        if pad == 0 || pad > opened.len() {
            return None;
        }
        opened.truncate(opened.len() - pad);
    }
    Some(Rtp {
        seq: u16::from_be_bytes([packet[2], packet[3]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        payload: opened,
        payload_type: packet[1] & 0x7f,
        marker: packet[1] & 0x80 != 0,
        timestamp: u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]),
    })
}

/// Seals an RTCP packet of ours: its first 8 bytes stay readable, as with RTP's header.
pub fn seal_rtcp(mode: Mode, key: &[u8; 32], counter: u32, packet: &[u8]) -> Option<Vec<u8>> {
    const CLEAR: usize = 8;
    let (clear, body) = packet.split_at_checked(CLEAR)?;
    let counter = counter.to_be_bytes();
    let plain = Payload { msg: body, aad: clear };
    let sealed = match mode {
        Mode::Aes => {
            let mut nonce = [0u8; 12];
            nonce[..COUNTER].copy_from_slice(&counter);
            Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)).encrypt(Nonce::from_slice(&nonce), plain).ok()?
        }
        Mode::XChaCha => {
            let mut nonce = [0u8; 24];
            nonce[..COUNTER].copy_from_slice(&counter);
            XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key)).encrypt(XNonce::from_slice(&nonce), plain).ok()?
        }
    };
    let mut out = clear.to_vec();
    out.extend_from_slice(&sealed);
    out.extend_from_slice(&counter);
    Some(out)
}

/// Seals a video packet of ours with Discord's header extensions (one-byte profile, 0xBEDE):
/// the transport sequence number (ID 5), playout delay (ID 6, which Discord expects on video)
/// and the RID of the layer it belongs to (ID 11). The extension's own header stays readable;
/// its elements are sealed with the payload, as in Discord's RTP-size modes.
#[allow(clippy::too_many_arguments)]
pub fn seal_video_rtp(mode: Mode, key: &[u8; 32], payload_type: u8, marker: bool, seq: u16, timestamp: u32, ssrc: u32, counter: u32, transport_seq: u16, rid: &str, payload: &[u8]) -> Option<Vec<u8>> {
    let mut elements = Vec::with_capacity(16);
    let element = |out: &mut Vec<u8>, id: u8, data: &[u8]| {
        out.push((id << 4) | (data.len() as u8 - 1));
        out.extend_from_slice(data);
    };
    element(&mut elements, 5, &transport_seq.to_be_bytes());
    // Playout delay: no minimum and no maximum (12 bits each, in 10 ms): play as soon as it can.
    element(&mut elements, 6, &[0, 0, 0]);
    element(&mut elements, 11, rid.as_bytes());
    while elements.len() % 4 != 0 {
        elements.push(0);
    }
    let mut packet = Vec::with_capacity(RTP_HEADER + 4 + elements.len() + payload.len() + TAG + COUNTER);
    packet.extend_from_slice(&[0x90, (payload_type & 0x7f) | if marker { 0x80 } else { 0 }]);
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    packet.extend_from_slice(&[0xBE, 0xDE]);
    packet.extend_from_slice(&((elements.len() / 4) as u16).to_be_bytes());
    let clear = packet.len();
    let mut body = elements;
    body.extend_from_slice(payload);
    let counter = counter.to_be_bytes();
    let plain = Payload { msg: &body, aad: &packet[..clear] };
    let sealed = match mode {
        Mode::Aes => {
            let mut nonce = [0u8; 12];
            nonce[..COUNTER].copy_from_slice(&counter);
            Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)).encrypt(Nonce::from_slice(&nonce), plain).ok()?
        }
        Mode::XChaCha => {
            let mut nonce = [0u8; 24];
            nonce[..COUNTER].copy_from_slice(&counter);
            XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key)).encrypt(XNonce::from_slice(&nonce), plain).ok()?
        }
    };
    packet.extend_from_slice(&sealed);
    packet.extend_from_slice(&counter);
    Some(packet)
}

/// Opens an RTCP packet: its first 8 bytes are readable, the rest sealed. `None` if it isn't
/// RTCP or doesn't open. Returns the whole packet, header and all, opened.
pub fn open_rtcp(mode: Mode, key: &[u8; 32], packet: &[u8]) -> Option<Vec<u8>> {
    const CLEAR: usize = 8;
    if packet.len() < CLEAR + TAG + COUNTER || packet[0] >> 6 != 2 || !(200..=206).contains(&packet[1]) {
        return None;
    }
    let (body, counter) = packet.split_at(packet.len() - COUNTER);
    let opened = open(mode, key, counter, &body[CLEAR..], &body[..CLEAR])?;
    let mut whole = body[..CLEAR].to_vec();
    whole.extend_from_slice(&opened);
    Some(whole)
}

/// Whether an opened RTCP packet asks for a keyframe, and of which SSRC: a picture loss
/// indication (payload-specific feedback, format 1), or a full intra request (format 4).
pub fn keyframe_request(rtcp: &[u8]) -> Option<u32> {
    let mut at = 0;
    // A packet can hold several reports, one after another.
    while at + 12 <= rtcp.len() {
        let (first, kind) = (rtcp[at], rtcp[at + 1]);
        let words = u16::from_be_bytes([rtcp[at + 2], rtcp[at + 3]]) as usize;
        if kind == 206 && matches!(first & 0x1f, 1 | 4) {
            return Some(u32::from_be_bytes([rtcp[at + 8], rtcp[at + 9], rtcp[at + 10], rtcp[at + 11]]));
        }
        at += (words + 1) * 4;
    }
    None
}

/// What a receiver report (RTCP 201, or the report blocks of a sender report, 200) says about
/// `ssrc`: (fraction lost out of 256, packets lost in all, the highest sequence number seen).
pub fn report_about(rtcp: &[u8], ssrc: u32) -> Option<(u8, u32, u32)> {
    let mut at = 0;
    while at + 8 <= rtcp.len() {
        let (first, kind) = (rtcp[at], rtcp[at + 1]);
        let words = u16::from_be_bytes([rtcp[at + 2], rtcp[at + 3]]) as usize;
        let count = (first & 0x1f) as usize;
        // Report blocks start after the sender's SSRC (RR), or after its 20-byte sender info (SR).
        let blocks = match kind {
            201 => Some(at + 8),
            200 => Some(at + 28),
            _ => None,
        };
        if let Some(mut block) = blocks {
            for _ in 0..count {
                let Some(b) = rtcp.get(block..block + 24) else { break };
                if u32::from_be_bytes([b[0], b[1], b[2], b[3]]) == ssrc {
                    let lost = u32::from_be_bytes([0, b[5], b[6], b[7]]);
                    let highest = u32::from_be_bytes([b[8], b[9], b[10], b[11]]);
                    return Some((b[4], lost, highest));
                }
                block += 24;
            }
        }
        at += (words + 1) * 4;
    }
    None
}

/// A request for a keyframe (RTCP picture loss indication), from `ours` about `theirs`.
pub fn pli(ours: u32, theirs: u32) -> [u8; 12] {
    let mut packet = [0u8; 12];
    packet[0] = 0x81; // version 2, format 1: PLI
    packet[1] = 206; // payload-specific feedback
    packet[2..4].copy_from_slice(&2u16.to_be_bytes()); // length in words, minus one
    packet[4..8].copy_from_slice(&ours.to_be_bytes());
    packet[8..12].copy_from_slice(&theirs.to_be_bytes());
    packet
}

/// Seals an audio packet of ours (Opus, payload type 120).
pub fn seal_packet(mode: Mode, key: &[u8; 32], seq: u16, timestamp: u32, ssrc: u32, counter: u32, payload: &[u8]) -> Option<Vec<u8>> {
    seal_rtp(mode, key, 0x78, false, seq, timestamp, ssrc, counter, payload)
}

/// Seals an RTP packet of ours with `payload_type`, `marker` set on a frame's last packet.
#[allow(clippy::too_many_arguments)]
pub fn seal_rtp(mode: Mode, key: &[u8; 32], payload_type: u8, marker: bool, seq: u16, timestamp: u32, ssrc: u32, counter: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let mut packet = Vec::with_capacity(RTP_HEADER + payload.len() + TAG + COUNTER);
    packet.extend_from_slice(&[0x80, (payload_type & 0x7f) | if marker { 0x80 } else { 0 }]);
    packet.extend_from_slice(&seq.to_be_bytes());
    packet.extend_from_slice(&timestamp.to_be_bytes());
    packet.extend_from_slice(&ssrc.to_be_bytes());
    let counter = counter.to_be_bytes();
    let plain = Payload { msg: payload, aad: &packet[..RTP_HEADER] };
    let sealed = match mode {
        Mode::Aes => {
            let mut nonce = [0u8; 12];
            nonce[..COUNTER].copy_from_slice(&counter);
            Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(key)).encrypt(Nonce::from_slice(&nonce), plain).ok()?
        }
        Mode::XChaCha => {
            let mut nonce = [0u8; 24];
            nonce[..COUNTER].copy_from_slice(&counter);
            XChaCha20Poly1305::new(chacha20poly1305::Key::from_slice(key)).encrypt(XNonce::from_slice(&nonce), plain).ok()?
        }
    };
    packet.extend_from_slice(&sealed);
    packet.extend_from_slice(&counter);
    Some(packet)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7; 32];

    #[test]
    fn a_packet_opens_again_in_either_mode() {
        for mode in [Mode::Aes, Mode::XChaCha] {
            let packet = seal_packet(mode, &KEY, 5, 960, 42, 1, b"hello").unwrap();
            let opened = open_packet(mode, &KEY, &packet).unwrap();
            assert_eq!((opened.seq, opened.ssrc, opened.payload, opened.payload_type, opened.timestamp), (5, 42, b"hello".to_vec(), 0x78, 960));
        }
    }

    #[test]
    fn the_wrong_key_or_tampering_gets_nothing() {
        let mut packet = seal_packet(Mode::Aes, &KEY, 5, 960, 42, 1, b"hello").unwrap();
        assert!(open_packet(Mode::Aes, &[8; 32], &packet).is_none());
        packet[2] ^= 1;
        assert!(open_packet(Mode::Aes, &KEY, &packet).is_none(), "the header is covered too");
    }

    #[test]
    fn padding_comes_off() {
        let key = KEY;
        // Three bytes of audio, then three of padding ending in its length.
        let mut packet = seal_packet(Mode::Aes, &key, 1, 0, 9, 1, &[1, 2, 3, 0, 0, 3]).unwrap();
        packet[0] |= 0x20;
        // The header's covered by the seal: seal again with the flag set.
        let sealed = {
            let mut header = packet[..RTP_HEADER].to_vec();
            header[0] |= 0x20;
            let counter = 1u32.to_be_bytes();
            let mut nonce = [0u8; 12];
            nonce[..4].copy_from_slice(&counter);
            let body = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&key))
                .encrypt(Nonce::from_slice(&nonce), Payload { msg: &[1, 2, 3, 0, 0, 3], aad: &header })
                .unwrap();
            [header, body, counter.to_vec()].concat()
        };
        assert_eq!(open_packet(Mode::Aes, &key, &sealed).unwrap().payload, vec![1, 2, 3]);
    }

    #[test]
    fn video_packets_carry_discords_extensions() {
        let packet = seal_video_rtp(Mode::Aes, &KEY, 101, true, 9, 90_000, 77, 5, 3, "100", b"frame").unwrap();
        assert_eq!(packet[0], 0x90, "the extension bit");
        assert_eq!(&packet[12..14], &[0xBE, 0xDE]);
        // Our own receiving side takes the extension off and gets the payload back.
        let opened = open_packet(Mode::Aes, &KEY, &packet).unwrap();
        assert_eq!((opened.payload.as_slice(), opened.marker, opened.payload_type), (&b"frame"[..], true, 101));
    }

    #[test]
    fn keyframe_requests_are_read() {
        let pli = pli(7, 42);
        let sealed = seal_rtcp(Mode::Aes, &KEY, 3, &pli).unwrap();
        let opened = open_rtcp(Mode::Aes, &KEY, &sealed).unwrap();
        assert_eq!(keyframe_request(&opened), Some(42));
        let marked = seal_rtp(Mode::Aes, &KEY, 101, true, 1, 2, 3, 4, b"x").unwrap();
        assert_eq!(marked[1], 0x80 | 101, "marker and payload type");
    }

    #[test]
    fn reports_and_scraps_are_ignored() {
        assert!(open_packet(Mode::Aes, &KEY, &[0x80, 200, 0, 0]).is_none());
        let mut report = vec![0x80, 201];
        report.resize(64, 0);
        assert!(open_packet(Mode::Aes, &KEY, &report).is_none());
    }

    #[test]
    fn an_extension_comes_off_the_audio() {
        // One word of extension, flagged in the first byte, sealed along with the audio.
        let mut packet = vec![0x90, 0x78, 0, 9, 0, 0, 0, 0, 0, 0, 0, 42, 0xBE, 0xDE, 0, 1];
        let plain = b"\xAA\xBB\xCC\xDDaudio";
        let counter = 3u32.to_be_bytes();
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&counter);
        let sealed = Aes256Gcm::new(Key::<Aes256Gcm>::from_slice(&KEY))
            .encrypt(Nonce::from_slice(&nonce), Payload { msg: plain, aad: &packet })
            .unwrap();
        packet.extend_from_slice(&sealed);
        packet.extend_from_slice(&counter);
        assert_eq!(open_packet(Mode::Aes, &KEY, &packet).unwrap().payload, b"audio");
    }

    #[test]
    fn modes_are_picked_as_discord_asks() {
        let offered = |names: &[&str]| names.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(Mode::choose(&offered(&["aead_xchacha20_poly1305_rtpsize", "aead_aes256_gcm_rtpsize"])), Some(Mode::Aes));
        assert_eq!(Mode::choose(&offered(&["aead_xchacha20_poly1305_rtpsize"])), Some(Mode::XChaCha));
        assert_eq!(Mode::choose(&offered(&["xsalsa20_poly1305"])), None);
    }
}
