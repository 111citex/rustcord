//! Evens out one person's packets: they arrive early, late, out of order or not at
//! all, and are played one every 20 ms regardless.

use std::collections::BTreeMap;

/// Frames (20 ms each) held before someone starts playing, at least: on a calm line.
const TARGET: usize = 3;
/// And at most, on a line whose packets keep coming in late.
const MOST_TARGET: usize = 8;
/// Frames played without a late packet before the wait comes down by one.
const CALM_FOR: u32 = 500;
/// Holding more than this, they've fallen behind: skip ahead.
const TOO_MANY: usize = 14;
/// Frames in a row to cover over before taking it that they've stopped talking.
const MAX_CONCEALED: u32 = 5;

/// What to play next for someone.
#[derive(Debug, PartialEq, Eq)]
pub enum Next {
    /// Nothing: not talking, or still filling up.
    Nothing,
    Play(Vec<u8>),
    /// The packet's missing, but the next one carries a rough copy of it.
    Recover(Vec<u8>),
    /// The packet's missing: make something up to cover the gap.
    Conceal,
}

pub struct Jitter {
    /// Keyed by distance from `next`, so that sequence numbers can wrap around.
    held: BTreeMap<u32, Vec<u8>>,
    /// The sequence number of the frame to play next; `None` when not playing.
    next: Option<u16>,
    /// Where `held`'s keys count from.
    base: u32,
    concealed: u32,
    /// Frames dropped to catch up, not yet told about.
    skipped: u32,
    /// Started again after a silence, not yet told about.
    restarted: bool,
    /// Frames held before starting: grows when packets come too late to play, as a
    /// jittery line needs, and comes back down once it's calm.
    target: usize,
    /// Frames played since the last late packet.
    calm: u32,
    /// Packets that came after their turn, not yet told about.
    late: u32,
}

impl Default for Jitter {
    fn default() -> Self {
        Jitter { held: BTreeMap::new(), next: None, base: 0, concealed: 0, skipped: 0, restarted: false, target: TARGET, calm: 0, late: 0 }
    }
}

impl Jitter {
    fn key(&self, seq: u16) -> Option<u32> {
        let Some(next) = self.next else { return Some(self.base.wrapping_add(seq as u32)) };
        let ahead = seq.wrapping_sub(next);
        // More than half way round is behind us: too late.
        (ahead < 0x8000).then(|| self.base.wrapping_add(ahead as u32))
    }

    pub fn push(&mut self, seq: u16, payload: Vec<u8>) {
        match self.key(seq) {
            // Already played past, or covered over: too late. Wait longer from the next burst.
            Some(key) if key < self.base && self.next.is_some() => self.too_late(),
            Some(key) => {
                self.held.insert(key, payload);
            }
            None => self.too_late(),
        }
    }

    fn too_late(&mut self) {
        self.late += 1;
        self.calm = 0;
        self.target = (self.target + 1).min(MOST_TARGET);
    }

    /// Packets that came too late to play, since last asked.
    pub fn take_late(&mut self) -> u32 {
        std::mem::take(&mut self.late)
    }

    /// Frames dropped to catch up since last asked.
    pub fn take_skipped(&mut self) -> u32 {
        std::mem::take(&mut self.skipped)
    }

    /// Whether a new burst of speech began since last asked, after a gap no cover could
    /// bridge: what's played from here has nothing to do with what came before.
    pub fn take_restarted(&mut self) -> bool {
        std::mem::take(&mut self.restarted)
    }

    pub fn is_empty(&self) -> bool {
        self.held.is_empty() && self.next.is_none()
    }

    /// Starts from the oldest held, once there's enough to ride out a late packet.
    fn begin(&mut self) -> bool {
        if self.held.len() < self.target {
            return false;
        }
        // Not playing: keys are plain sequence numbers. Find where a wrap would be.
        let keys: Vec<u32> = self.held.keys().copied().collect();
        let (first, last) = (keys[0], keys[keys.len() - 1]);
        let wrapped = last - first > 0x8000;
        let start = if wrapped { *keys.iter().find(|k| **k >= 0x8000).unwrap_or(&first) } else { first };
        let rekeyed: BTreeMap<u32, Vec<u8>> = std::mem::take(&mut self.held)
            .into_iter()
            .map(|(key, payload)| ((key as u16).wrapping_sub(start as u16) as u32, payload))
            .collect();
        self.held = rekeyed;
        self.next = Some(start as u16);
        self.base = 0;
        self.concealed = 0;
        true
    }

    fn advance(&mut self) {
        self.next = self.next.map(|n| n.wrapping_add(1));
        self.base = self.base.wrapping_add(1);
        self.calm += 1;
        if self.calm >= CALM_FOR && self.target > TARGET {
            self.calm = 0;
            self.target -= 1;
        }
    }

    /// The next 20 ms.
    pub fn next(&mut self) -> Next {
        if self.next.is_none() && !self.begin() {
            return Next::Nothing;
        }
        // Too far behind: drop the oldest, to catch up rather than stay late.
        while self.held.len() > TOO_MANY {
            let Some((&oldest, _)) = self.held.iter().next() else { break };
            self.held.remove(&oldest);
            self.skipped += 1;
            if oldest >= self.base {
                let skipped = oldest + 1 - self.base;
                self.base += skipped;
                self.next = self.next.map(|n| n.wrapping_add(skipped as u16));
            }
        }
        let wanted = self.base;
        self.held.retain(|key, _| *key >= wanted);
        if let Some(payload) = self.held.remove(&wanted) {
            self.advance();
            self.concealed = 0;
            return Next::Play(payload);
        }
        if self.concealed >= MAX_CONCEALED {
            // Not talking any more. What's held was sent after a gap, and covering the
            // whole gap would play noise for as long as it lasted: start again from it.
            let (next, base) = (self.next, self.base);
            self.held = std::mem::take(&mut self.held)
                .into_iter()
                .map(|(key, payload)| (next.map_or(key, |n| n.wrapping_add(key.wrapping_sub(base) as u16) as u32), payload))
                .collect();
            let anything = !self.held.is_empty();
            self.next = None;
            self.base = 0;
            self.concealed = 0;
            self.restarted = anything;
            return if anything { self.next() } else { Next::Nothing };
        }
        self.concealed += 1;
        let following = self.held.get(&wanted.wrapping_add(1)).cloned();
        self.advance();
        match following {
            Some(payload) => Next::Recover(payload),
            None => Next::Conceal,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(n: u8) -> Vec<u8> {
        vec![n]
    }

    fn fill(j: &mut Jitter, seqs: &[u16]) {
        for s in seqs {
            j.push(*s, p(*s as u8));
        }
    }

    #[test]
    fn it_waits_to_fill_up_then_plays_in_order() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11]);
        assert_eq!(j.next(), Next::Nothing);
        fill(&mut j, &[12]);
        assert_eq!(j.next(), Next::Play(p(10)));
        assert_eq!(j.next(), Next::Play(p(11)));
        assert_eq!(j.next(), Next::Play(p(12)));
    }

    #[test]
    fn late_packets_make_it_wait_longer_next_time() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        assert_eq!(j.next(), Next::Play(p(10)));
        assert_eq!(j.next(), Next::Play(p(11)));
        j.push(10, p(10));
        assert_eq!(j.take_late(), 1);
        assert_eq!(j.target, TARGET + 1);
    }

    #[test]
    fn out_of_order_packets_land_in_order() {
        let mut j = Jitter::default();
        fill(&mut j, &[12, 10, 11]);
        assert_eq!(j.next(), Next::Play(p(10)));
        assert_eq!(j.next(), Next::Play(p(11)));
    }

    #[test]
    fn a_missing_packet_is_recovered_from_the_next() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 12, 13]);
        assert_eq!(j.next(), Next::Play(p(10)));
        assert_eq!(j.next(), Next::Recover(p(12)));
        assert_eq!(j.next(), Next::Play(p(12)), "and then played itself");
    }

    #[test]
    fn a_gap_is_covered_and_then_they_have_stopped() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        for _ in 0..3 {
            assert!(matches!(j.next(), Next::Play(_)));
        }
        for _ in 0..MAX_CONCEALED {
            assert_eq!(j.next(), Next::Conceal);
        }
        assert_eq!(j.next(), Next::Nothing);
        assert!(j.is_empty());
    }

    #[test]
    fn a_late_packet_is_dropped() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        assert_eq!(j.next(), Next::Play(p(10)));
        assert_eq!(j.next(), Next::Play(p(11)));
        j.push(10, p(99));
        assert_eq!(j.next(), Next::Play(p(12)));
    }

    #[test]
    fn sequence_numbers_wrap_around() {
        let mut j = Jitter::default();
        fill(&mut j, &[65534, 65535, 0, 1]);
        assert_eq!(j.next(), Next::Play(p(254)));
        assert_eq!(j.next(), Next::Play(p(255)));
        assert_eq!(j.next(), Next::Play(p(0)));
        assert_eq!(j.next(), Next::Play(p(1)));
    }

    #[test]
    fn falling_behind_skips_ahead() {
        let mut j = Jitter::default();
        let seqs: Vec<u16> = (0..20).collect();
        fill(&mut j, &seqs);
        match j.next() {
            Next::Play(first) => assert!(usize::from(first[0]) >= 20 - TOO_MANY, "started at {}", first[0]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn coming_back_after_a_long_gap_resumes_at_once() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        for _ in 0..3 {
            assert!(matches!(j.next(), Next::Play(_)));
        }
        // Muted and unmuted: the sequence jumps well past what could be covered.
        fill(&mut j, &[200, 201, 202]);
        let mut concealed = 0;
        let first = loop {
            match j.next() {
                Next::Conceal => concealed += 1,
                other => break other,
            }
        };
        assert_eq!(concealed, MAX_CONCEALED, "no more than the cover");
        assert_eq!(first, Next::Play(p(200)), "and not the whole gap of 190 frames");
        assert!(j.take_restarted());
        assert!(!j.take_restarted(), "told once");
        assert_eq!(j.next(), Next::Play(p(201)));
    }

    #[test]
    fn a_new_burst_still_filling_waits_and_a_plain_stop_is_no_restart() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        for _ in 0..3 {
            j.next();
        }
        fill(&mut j, &[100]);
        for _ in 0..MAX_CONCEALED {
            assert_eq!(j.next(), Next::Conceal);
        }
        assert_eq!(j.next(), Next::Nothing, "one packet isn't enough to start from");
        fill(&mut j, &[101, 102]);
        assert_eq!(j.next(), Next::Play(p(100)));
    }

    #[test]
    fn stopping_for_good_is_not_a_restart() {
        let mut j = Jitter::default();
        fill(&mut j, &[10, 11, 12]);
        for _ in 0..3 {
            j.next();
        }
        for _ in 0..=MAX_CONCEALED {
            j.next();
        }
        assert!(j.is_empty());
        assert!(!j.take_restarted());
    }
}
