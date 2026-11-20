//! Decides which captured frame the consumer gets and when.
//!
//! gliff asks for a frame when it wants one; PipeWire pushes frames whenever
//! the screen changes. The pacer sits between them over a small ring of
//! buffers: a new frame replaces an older one nobody asked for yet, a request
//! takes the newest frame or waits for one, and the last frame sent can be
//! sent again (the first frame after a quiet spell, or a keyframe request on a
//! screen that is not changing).

#[derive(Debug)]
pub(crate) struct Pacer {
    /// How many consumers hold each slot. A held slot is never overwritten.
    held: Vec<u32>,
    /// A filled slot waiting for a request.
    pending: Option<usize>,
    /// The slot sent most recently; its pixels are intact until reused.
    last: Option<usize>,
    wanted: bool,
}

impl Pacer {
    pub(crate) fn new(slots: usize) -> Self {
        Self {
            held: vec![0; slots],
            pending: None,
            last: None,
            wanted: false,
        }
    }

    /// Start over with `slots` new slots (the format changed). A request made
    /// before this still stands: the consumer is waiting for a frame.
    pub(crate) fn reset(&mut self, slots: usize) {
        *self = Self {
            wanted: self.wanted,
            ..Self::new(slots)
        };
    }

    /// One more slot (a dmabuf pool grows one buffer at a time).
    pub(crate) fn add_slot(&mut self) {
        self.held.push(0);
    }

    /// No consumer holds `slot`, and it is neither the unsent frame nor the
    /// one kept for a re-send: its buffer can go back to the producer.
    pub(crate) fn is_free(&self, slot: usize) -> bool {
        self.held.get(slot) == Some(&0) && self.pending != Some(slot) && self.last != Some(slot)
    }

    /// A slot to write the next frame into, or `None` when the consumer holds
    /// every slot (the frame is dropped). The newest frame wins: with nothing
    /// else free, the slot of an unsent frame is overwritten (that frame is
    /// stale now), and only after that the one kept for a re-send.
    pub(crate) fn acquire(&mut self) -> Option<usize> {
        let unheld = |i: &usize| self.held[*i] == 0;
        let all = 0..self.held.len();
        let slot = all
            .clone()
            .filter(unheld)
            .find(|&i| self.pending != Some(i) && self.last != Some(i))
            .or_else(|| {
                all.clone()
                    .filter(unheld)
                    .find(|&i| self.pending == Some(i))
            })
            .or_else(|| all.filter(unheld).find(|&i| self.last == Some(i)))?;
        if self.pending == Some(slot) {
            self.pending = None;
        }
        if self.last == Some(slot) {
            self.last = None;
        }
        Some(slot)
    }

    /// `slot` now holds a new frame. Returns the slot to send if the consumer
    /// is waiting for one.
    pub(crate) fn filled(&mut self, slot: usize) -> Option<usize> {
        self.pending = Some(slot);
        if self.wanted {
            self.send_pending()
        } else {
            None
        }
    }

    /// The consumer wants the next frame.
    pub(crate) fn request(&mut self) -> Option<usize> {
        self.wanted = true;
        self.send_pending()
    }

    /// Send a frame now: the newest unsent one, else the last one again.
    /// With neither, the next frame is sent as soon as it arrives.
    pub(crate) fn recapture(&mut self) -> Option<usize> {
        if let Some(s) = self.send_pending() {
            return Some(s);
        }
        match self.last {
            Some(s) => {
                self.held[s] += 1;
                self.wanted = false;
                Some(s)
            }
            None => {
                self.wanted = true;
                None
            }
        }
    }

    /// The consumer is done with `slot`.
    pub(crate) fn release(&mut self, slot: usize) {
        if let Some(h) = self.held.get_mut(slot) {
            *h = h.saturating_sub(1);
        }
    }

    fn send_pending(&mut self) -> Option<usize> {
        let slot = self.pending.take()?;
        self.held[slot] += 1;
        self.last = Some(slot);
        self.wanted = false;
        Some(slot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_slot_is_free_once_it_is_neither_held_pending_nor_last() {
        let mut p = Pacer::new(0);
        p.add_slot();
        p.add_slot();
        // Frame in slot 0 arrives, is sent, then slot 1 arrives and replaces
        // it as the newest.
        assert_eq!(p.filled(0), None);
        assert!(!p.is_free(0), "unsent");
        assert_eq!(p.request(), Some(0));
        assert!(!p.is_free(0), "held and last");
        p.release(0);
        assert!(!p.is_free(0), "still the one kept for a re-send");
        assert_eq!(p.filled(1), None);
        assert_eq!(p.request(), Some(1));
        p.release(1);
        assert!(p.is_free(0), "a newer frame was sent, so 0 can go back");
        assert!(!p.is_free(1));
    }

    #[test]
    fn an_unsent_frame_replaced_by_a_newer_one_is_free() {
        let mut p = Pacer::new(0);
        p.add_slot();
        p.add_slot();
        assert_eq!(p.filled(0), None);
        assert_eq!(p.filled(1), None);
        assert!(p.is_free(0), "never sent, and no longer the newest");
        assert!(!p.is_free(1));
    }

    #[test]
    fn a_frame_waits_for_a_request() {
        let mut p = Pacer::new(3);
        let a = p.acquire().unwrap();
        assert_eq!(p.filled(a), None);
        assert_eq!(p.request(), Some(a));
        // Nothing new: the next request waits.
        assert_eq!(p.request(), None);
        let b = p.acquire().unwrap();
        assert_eq!(p.filled(b), Some(b));
    }

    #[test]
    fn a_request_made_before_the_format_is_known_survives_it() {
        let mut p = Pacer::new(0);
        assert_eq!(p.request(), None);
        p.reset(3);
        let a = p.acquire().unwrap();
        assert_eq!(p.filled(a), Some(a), "the early request is still waiting");
    }

    #[test]
    fn a_full_ring_still_takes_the_newest_frame_over_a_stale_pending_one() {
        // The consumer holds 3 frames; the 4th slot holds an unsent frame.
        let mut p = Pacer::new(4);
        let mut held = Vec::new();
        for _ in 0..3 {
            p.request();
            let s = p.acquire().unwrap();
            assert_eq!(p.filled(s), Some(s));
            held.push(s);
        }
        let stale = p.acquire().unwrap();
        assert_eq!(p.filled(stale), None);
        // A newer frame arrives: it must replace the stale one, not be dropped.
        let newer = p.acquire().expect("the stale pending slot is reused");
        assert_eq!(newer, stale);
        p.filled(newer);
        assert_eq!(
            p.request(),
            Some(newer),
            "the consumer gets the newest frame"
        );
    }

    #[test]
    fn a_newer_frame_replaces_an_unsent_one() {
        let mut p = Pacer::new(3);
        let a = p.acquire().unwrap();
        p.filled(a);
        let b = p.acquire().unwrap();
        assert_ne!(a, b, "the unsent frame is not overwritten while pending");
        p.filled(b);
        // `a` is free again: the pending frame is now `b`.
        assert_eq!(p.request(), Some(b));
        let again = p.acquire().unwrap();
        assert_ne!(again, b, "b is held by the consumer");
    }

    #[test]
    fn held_slots_are_not_reused_and_a_full_ring_drops() {
        let mut p = Pacer::new(2);
        p.request();
        let a = p.acquire().unwrap();
        assert_eq!(p.filled(a), Some(a));
        p.request();
        let b = p.acquire().unwrap();
        assert_eq!(p.filled(b), Some(b));
        assert_eq!(p.acquire(), None);
        p.release(a);
        assert_eq!(p.acquire(), Some(a));
    }

    #[test]
    fn the_last_frame_can_be_sent_again_until_its_slot_is_reused() {
        let mut p = Pacer::new(3);
        assert_eq!(
            p.recapture(),
            None,
            "nothing to send yet; the next frame goes out"
        );
        let a = p.acquire().unwrap();
        assert_eq!(p.filled(a), Some(a));
        p.release(a);
        assert_eq!(p.recapture(), Some(a));
        // Sent twice: held twice, so both releases are needed.
        p.release(a);
        assert_eq!(p.acquire().map(|s| s == a), Some(false));
        p.release(a);
        // Free slots other than the last come first.
        let next = p.acquire().unwrap();
        assert_ne!(next, a);
    }

    #[test]
    fn reusing_the_last_slot_ends_the_resend() {
        let mut p = Pacer::new(1);
        p.request();
        let a = p.acquire().unwrap();
        p.filled(a);
        p.release(a);
        let again = p.acquire().unwrap();
        assert_eq!(again, a);
        // The slot is being overwritten, so it can no longer be re-sent.
        assert_eq!(p.recapture(), None);
    }

    #[test]
    fn an_unsent_frame_beats_a_resend() {
        let mut p = Pacer::new(3);
        let a = p.acquire().unwrap();
        p.filled(a);
        p.request();
        p.release(a);
        let b = p.acquire().unwrap();
        p.filled(b);
        assert_eq!(p.recapture(), Some(b));
    }
}
