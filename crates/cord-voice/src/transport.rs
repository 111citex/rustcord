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
    Some(Rtp {
        seq: u16::from_be_bytes([packet[2], packet[3]]),
        ssrc: u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]),
        payload: opened,
    })
}

/// Seals a packet of ours.
pub fn seal_packet(mode: Mode, key: &[u8; 32], seq: u16, timestamp: u32, ssrc: u32, counter: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let mut packet = Vec::with_capacity(RTP_HEADER + payload.len() + TAG + COUNTER);
    packet.extend_from_slice(&[0x80, 0x78]);
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
            assert_eq!(opened, Rtp { seq: 5, ssrc: 42, payload: b"hello".to_vec() });
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
