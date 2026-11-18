//! Platform-independent input state: motion accumulation and held-input tracking.

use std::collections::BTreeSet;

use crate::protocol::Motion;

/// Sender side: turns per-event deltas into cumulative [`Motion`] totals.
#[derive(Debug, Default)]
pub struct MotionSender {
    state: Motion,
}

impl MotionSender {
    pub fn add(&mut self, dx: i64, dy: i64, scroll_x: i64, scroll_y: i64) -> Motion {
        let s = &mut self.state;
        s.seq += 1;
        s.x += dx;
        s.y += dy;
        s.scroll_x += scroll_x;
        s.scroll_y += scroll_y;
        *s
    }

    pub fn current(&self) -> Motion {
        self.state
    }
}

/// The part of a motion update that still has to be injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MotionDelta {
    pub dx: i64,
    pub dy: i64,
    pub scroll_x: i64,
    pub scroll_y: i64,
}

impl MotionDelta {
    pub fn has_pointer(&self) -> bool {
        self.dx != 0 || self.dy != 0
    }

    pub fn has_scroll(&self) -> bool {
        self.scroll_x != 0 || self.scroll_y != 0
    }
}

/// Receiver side: applies only motion newer than what was already injected.
#[derive(Debug, Default)]
pub struct MotionReceiver {
    applied: Motion,
}

impl MotionReceiver {
    /// Returns the delta to inject, or `None` if `m` is stale or a duplicate.
    pub fn apply(&mut self, m: Motion) -> Option<MotionDelta> {
        if m.seq <= self.applied.seq {
            return None;
        }
        let a = self.applied;
        self.applied = m;
        Some(MotionDelta {
            dx: m.x - a.x,
            dy: m.y - a.y,
            scroll_x: m.scroll_x - a.scroll_x,
            scroll_y: m.scroll_y - a.scroll_y,
        })
    }
}

/// Keys and buttons currently held down on the receiver, so they can all be released
/// when control leaves or the link stalls.
#[derive(Debug, Default)]
pub struct HeldInputs {
    keys: BTreeSet<u16>,
    buttons: BTreeSet<u8>,
    consumer: BTreeSet<u16>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Release {
    Key(u16),
    Button(u8),
    Consumer(u16),
}

impl HeldInputs {
    /// Records a key event and returns whether it should be injected. Releases of keys
    /// that were never pressed here (e.g. modifiers of the switch hotkey) are dropped.
    pub fn key(&mut self, code: u16, down: bool) -> bool {
        if down {
            self.keys.insert(code);
            true
        } else {
            self.keys.remove(&code)
        }
    }

    /// Same contract as [`HeldInputs::key`].
    pub fn button(&mut self, button: u8, down: bool) -> bool {
        if down {
            self.buttons.insert(button);
            true
        } else {
            self.buttons.remove(&button)
        }
    }

    /// Same contract as [`HeldInputs::key`].
    pub fn consumer(&mut self, usage: u16, down: bool) -> bool {
        if down {
            self.consumer.insert(usage);
            true
        } else {
            self.consumer.remove(&usage)
        }
    }

    pub fn release_all(&mut self) -> Vec<Release> {
        let releases = self
            .buttons
            .iter()
            .map(|&b| Release::Button(b))
            .chain(self.consumer.iter().map(|&u| Release::Consumer(u)))
            .chain(self.keys.iter().map(|&k| Release::Key(k)))
            .collect();
        self.keys.clear();
        self.buttons.clear();
        self.consumer.clear();
        releases
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lost_motion_is_caught_up_by_the_next_update() {
        let mut tx = MotionSender::default();
        let mut rx = MotionReceiver::default();
        let a = tx.add(5, 0, 0, 0);
        let _lost = tx.add(3, 1, 0, 0);
        let c = tx.add(2, 1, 0, 2);
        assert_eq!(rx.apply(a).unwrap().dx, 5);
        let d = rx.apply(c).unwrap();
        assert_eq!((d.dx, d.dy, d.scroll_y), (5, 2, 2));
    }

    #[test]
    fn stale_and_duplicate_motion_is_ignored() {
        let mut tx = MotionSender::default();
        let mut rx = MotionReceiver::default();
        let a = tx.add(1, 1, 0, 0);
        let b = tx.add(1, 1, 0, 0);
        assert!(rx.apply(b).is_some());
        assert!(rx.apply(a).is_none());
        assert!(rx.apply(b).is_none());
    }

    #[test]
    fn release_all_releases_buttons_then_keys_once() {
        let mut held = HeldInputs::default();
        held.key(0xE1, true); // left shift
        held.key(0x04, true);
        assert!(held.key(0x04, false));
        assert!(!held.key(0x05, false), "release of a key never pressed is dropped");
        held.button(0, true);
        held.consumer(0xE9, true); // volume up
        assert_eq!(
            held.release_all(),
            vec![Release::Button(0), Release::Consumer(0xE9), Release::Key(0xE1)]
        );
        assert!(held.release_all().is_empty());
    }
}
