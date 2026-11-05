//! Discord's `compress=zlib-stream`: one zlib context per connection, each
//! message ends with a sync flush (`00 00 FF FF`), and a message may arrive
//! split across several WebSocket frames.

use flate2::{Decompress, FlushDecompress};

pub(crate) const SUFFIX: [u8; 4] = [0x00, 0x00, 0xff, 0xff];

#[derive(Debug, thiserror::Error)]
pub enum InflateError {
    #[error("zlib: {0}")]
    Zlib(#[from] flate2::DecompressError),
    #[error("payload is not UTF-8")]
    Utf8,
}

pub struct Inflater {
    z: Decompress,
    pending: Vec<u8>,
}

impl Default for Inflater {
    fn default() -> Self {
        Self::new()
    }
}

impl Inflater {
    pub fn new() -> Self {
        Inflater { z: Decompress::new(true), pending: Vec::new() }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Option<String>, InflateError> {
        self.pending.extend_from_slice(chunk);
        if !self.pending.ends_with(&SUFFIX) {
            return Ok(None);
        }
        let input = std::mem::take(&mut self.pending);
        let mut out = Vec::with_capacity(input.len() * 4);
        let mut offset = 0;
        loop {
            if out.len() == out.capacity() {
                out.reserve(out.capacity().max(16 * 1024));
            }
            let (in_before, out_before) = (self.z.total_in(), out.len());
            self.z.decompress_vec(&input[offset..], &mut out, FlushDecompress::Sync)?;
            offset += (self.z.total_in() - in_before) as usize;
            let progressed = self.z.total_in() != in_before || out.len() != out_before;
            // Done when all input is consumed and zlib had room to spare.
            if (offset == input.len() && out.len() < out.capacity()) || !progressed {
                break;
            }
        }
        String::from_utf8(out).map(Some).map_err(|_| InflateError::Utf8)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{Compress, Compression, FlushCompress};

    /// Compresses like Discord: one shared context, a sync flush per message.
    fn deflate(z: &mut Compress, msg: &str) -> Vec<u8> {
        let mut out = Vec::with_capacity(msg.len() * 2 + 64);
        z.compress_vec(msg.as_bytes(), &mut out, FlushCompress::Sync).unwrap();
        assert!(out.ends_with(&SUFFIX));
        out
    }

    #[test]
    fn inflates_consecutive_messages_with_shared_context() {
        let mut z = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();
        let a = deflate(&mut z, r#"{"op":10,"d":{"heartbeat_interval":41250}}"#);
        let b = deflate(&mut z, r#"{"op":11}"#);
        assert_eq!(inf.push(&a).unwrap().as_deref(), Some(r#"{"op":10,"d":{"heartbeat_interval":41250}}"#));
        assert_eq!(inf.push(&b).unwrap().as_deref(), Some(r#"{"op":11}"#));
    }

    #[test]
    fn waits_for_suffix_across_chunks() {
        let mut z = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();
        let msg = deflate(&mut z, r#"{"op":0,"t":"X","s":1,"d":{}}"#);
        let (head, tail) = msg.split_at(msg.len() / 2);
        assert_eq!(inf.push(head).unwrap(), None);
        assert_eq!(inf.push(tail).unwrap().as_deref(), Some(r#"{"op":0,"t":"X","s":1,"d":{}}"#));
    }

    #[test]
    fn large_message_grows_output() {
        let big = format!(r#"{{"op":0,"d":"{}"}}"#, "a".repeat(2_000_000));
        let mut z = Compress::new(Compression::default(), true);
        let mut inf = Inflater::new();
        assert_eq!(inf.push(&deflate(&mut z, &big)).unwrap().unwrap().len(), big.len());
    }

    #[test]
    fn garbage_is_an_error() {
        let mut inf = Inflater::new();
        assert!(inf.push(&[1, 2, 3, 4, 0, 0, 0xff, 0xff]).is_err());
    }
}
