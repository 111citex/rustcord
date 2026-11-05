//! Rustcord: how a finished frame gets from the main thread to the render thread. There's
//! one slot and the newest frame wins, so a render thread that falls behind skips frames
//! instead of showing old ones late, and the main thread never waits for it.
#![cfg_attr(target_family = "wasm", allow(dead_code))]

use parking_lot::{Condvar, Mutex};

/// Frames that wait to be filled again, so that their buffers don't get reallocated. One in
/// the slot and one being drawn is the most that are ever out at once.
const MAX_SPARES: usize = 2;

pub(crate) struct FrameMailbox<F> {
    state: Mutex<MailboxState<F>>,
    changed: Condvar,
}

struct MailboxState<F> {
    pending: Option<F>,
    spares: Vec<F>,
    /// The render thread has something to do with the frame it's got already.
    nudged: bool,
    closed: bool,
}

impl<F> FrameMailbox<F> {
    pub(crate) fn new() -> Self {
        Self {
            state: Mutex::new(MailboxState {
                pending: None,
                spares: Vec::new(),
                nudged: false,
                closed: false,
            }),
            changed: Condvar::new(),
        }
    }

    pub(crate) fn take_spare(&self) -> Option<F> {
        self.state.lock().spares.pop()
    }

    /// Gives a frame back once it's been drawn or dropped, for `take_spare`.
    pub(crate) fn recycle(&self, frame: F) {
        let mut state = self.state.lock();
        if state.spares.len() < MAX_SPARES {
            state.spares.push(frame);
        }
    }

    /// Leaves `frame` for the render thread. If one is still waiting there it won't be
    /// drawn: `replaced` runs with the old frame and the new one, so that what the new one
    /// still needs can be carried over, and the old one is recycled. Returns whether that
    /// happened.
    pub(crate) fn post(&self, mut frame: F, replaced: impl FnOnce(&mut F, &mut F)) -> bool {
        let mut state = self.state.lock();
        let had_pending = if let Some(mut older) = state.pending.take() {
            replaced(&mut older, &mut frame);
            if state.spares.len() < MAX_SPARES {
                state.spares.push(older);
            }
            true
        } else {
            false
        };
        state.pending = Some(frame);
        drop(state);
        self.changed.notify_one();
        had_pending
    }

    /// Waits for the next frame. `None` once closed, even with a frame still waiting.
    #[cfg(test)]
    pub(crate) fn wait(&self) -> Option<F> {
        let mut state = self.state.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(frame) = state.pending.take() {
                return Some(frame);
            }
            self.changed.wait(&mut state);
        }
    }

    /// Waits for a frame or a [`Self::nudge`]: `Some(None)` is the nudge, and `None` is closed.
    pub(crate) fn wait_or_nudge(&self) -> Option<Option<F>> {
        let mut state = self.state.lock();
        loop {
            if state.closed {
                return None;
            }
            if let Some(frame) = state.pending.take() {
                state.nudged = false;
                return Some(Some(frame));
            }
            if std::mem::take(&mut state.nudged) {
                return Some(None);
            }
            self.changed.wait(&mut state);
        }
    }

    /// Wakes [`Self::wait_or_nudge`] with no new frame. Stays set until it's been seen.
    pub(crate) fn nudge(&self) {
        self.state.lock().nudged = true;
        self.changed.notify_one();
    }

    pub(crate) fn is_closed(&self) -> bool {
        self.state.lock().closed
    }

    /// Takes back the waiting frame, if any, so it won't be drawn.
    pub(crate) fn take_pending(&self) -> Option<F> {
        self.state.lock().pending.take()
    }

    pub(crate) fn has_pending(&self) -> bool {
        self.state.lock().pending.is_some()
    }

    /// Makes `wait` return `None` from now on, which ends the render thread.
    pub(crate) fn close(&self) {
        self.state.lock().closed = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    #[test]
    fn newest_frame_wins_and_the_old_one_hands_over() {
        let mailbox = FrameMailbox::<Vec<u32>>::new();
        assert!(!mailbox.post(vec![1], |_, _| panic!("nothing to replace")));
        let replaced = mailbox.post(vec![2], |older, newer| {
            older.append(newer);
            std::mem::swap(older, newer);
        });
        assert!(replaced);
        assert_eq!(mailbox.wait(), Some(vec![1, 2]));
        assert!(!mailbox.has_pending());
    }

    #[test]
    fn replaced_frames_come_back_as_spares() {
        let mailbox = FrameMailbox::<u32>::new();
        mailbox.post(1, |_, _| {});
        mailbox.post(2, |_, _| {});
        assert_eq!(mailbox.take_spare(), Some(1));
        assert_eq!(mailbox.take_spare(), None);
    }

    #[test]
    fn spares_are_bounded() {
        let mailbox = FrameMailbox::<u32>::new();
        for frame in 0..10 {
            mailbox.recycle(frame);
        }
        let mut count = 0;
        while mailbox.take_spare().is_some() {
            count += 1;
        }
        assert_eq!(count, MAX_SPARES);
    }

    #[test]
    fn wait_blocks_until_a_frame_comes() {
        let mailbox = Arc::new(FrameMailbox::<u32>::new());
        let waiting = {
            let mailbox = mailbox.clone();
            std::thread::spawn(move || mailbox.wait())
        };
        std::thread::sleep(Duration::from_millis(20));
        mailbox.post(7, |_, _| {});
        assert_eq!(waiting.join().unwrap(), Some(7));
    }

    #[test]
    fn a_nudge_wakes_without_a_frame_and_stays_until_seen() {
        let mailbox = FrameMailbox::<u32>::new();
        mailbox.nudge();
        assert_eq!(mailbox.wait_or_nudge(), Some(None));

        let mailbox = Arc::new(mailbox);
        let waiting = {
            let mailbox = mailbox.clone();
            std::thread::spawn(move || mailbox.wait_or_nudge())
        };
        std::thread::sleep(Duration::from_millis(20));
        mailbox.nudge();
        assert_eq!(waiting.join().unwrap(), Some(None));

        // A frame answers a nudge too, so that it doesn't wake the thread a second time.
        mailbox.nudge();
        mailbox.post(1, |_, _| {});
        assert_eq!(mailbox.wait_or_nudge(), Some(Some(1)));
        mailbox.close();
        assert_eq!(mailbox.wait_or_nudge(), None);
        assert!(mailbox.is_closed());
    }

    #[test]
    fn close_wakes_a_waiting_thread_even_with_a_frame_left() {
        let mailbox = Arc::new(FrameMailbox::<u32>::new());
        let waiting = {
            let mailbox = mailbox.clone();
            std::thread::spawn(move || mailbox.wait())
        };
        std::thread::sleep(Duration::from_millis(20));
        mailbox.close();
        assert_eq!(waiting.join().unwrap(), None);

        mailbox.post(1, |_, _| {});
        assert_eq!(mailbox.wait(), None);
        assert_eq!(mailbox.take_pending(), Some(1));
    }
}
