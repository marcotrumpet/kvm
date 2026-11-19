//! Screen video transport. The platform app captures and encodes; this module moves the
//! encoded frames and decides the bitrate.
//!
//! Each frame travels on its own unidirectional QUIC stream at a priority below the
//! control stream, so video never delays input and a slow frame never blocks the next
//! one at the transport level. Frame payloads are opaque to the core.
//!
//! Stream layout: `STREAM_VIDEO: u8 | seq: u64 LE | flags: u8 | payload`.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use iroh::endpoint::Connection;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info, trace};

use crate::protocol::STREAM_VIDEO;

/// Frames sent but not yet acknowledged. On Wi-Fi a frame takes 20-40 ms to be
/// acknowledged, so at 60 fps 2-3 are in flight normally; beyond this the link is behind.
/// The app checks [`VideoSender::can_send`] before encoding and skips the frame instead,
/// which needs no keyframe (the encoder never saw it).
const MAX_IN_FLIGHT: usize = 6;
/// Below the control stream's priority.
const VIDEO_PRIORITY: i32 = -10;
pub(crate) const CONTROL_PRIORITY: i32 = 10;
pub(crate) const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// How long a later frame waits for a missing earlier one before the gap counts as loss.
const GAP_TIMEOUT: Duration = Duration::from_millis(40);
const KEYFRAME_REQUEST_INTERVAL: Duration = Duration::from_millis(300);
const FLAG_KEYFRAME: u8 = 1;

/// How often the bitrate is reconsidered.
pub(crate) const RATE_TICK: Duration = Duration::from_millis(500);
const MIN_KBPS: u32 = 1_000;
const START_KBPS: u32 = 12_000;
/// Extra delivery delay (over the best seen) that counts as queueing, at least. Wi-Fi
/// jitter alone reaches 20-30 ms, so this has to sit above it.
const QUEUEING_THRESHOLD_MS: f64 = 40.0;
/// Delay-based congestion must persist this many ticks: one jitter spike is not a queue.
const DELAY_TICKS: u32 = 2;
/// Skipped frames per tick (the app found the link busy) that count as congestion.
const BUSY_THRESHOLD: u32 = 3;
const DECREASE: f64 = 0.75;
/// Until the first congestion, climb fast to find the link's capacity; then gently.
const SLOW_START_INCREASE: f64 = 1.3;
const INCREASE: f64 = 1.06;
/// After backing off, wait this long before probing upwards again.
const HOLD_AFTER_DECREASE: Duration = Duration::from_secs(2);

/// What the viewer asks the host to stream. `bitrate_kbps` is the ceiling; the host
/// adapts below it. Asking again while streaming updates the stream in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoParams {
    pub max_width: u16,
    pub max_height: u16,
    pub fps: u8,
    pub bitrate_kbps: u32,
    /// What to give up first when bits run short: 0 balanced, 1 frame rate (keep text
    /// sharp), 2 resolution (keep motion smooth).
    pub prefer: u8,
    /// Which of the host's displays (an id from its display list); 0 is the main one.
    pub display: u32,
    /// Re-lay out the host's main display (a virtual display mirroring it) instead of only
    /// scaling the picture: 1 at `max_width` x `max_height`, 2 at that aspect ratio but the
    /// host's own number of pixels (so a zoomed-in viewer can ask for more); 0 off. Ignored
    /// for other displays. (A `u8` that was a `bool`: same encoding for 0 and 1.)
    pub fit_host: u8,
}

/// Measurements since the last rate tick.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct LinkStats {
    /// Frames refused or skipped because too many were in flight.
    pub refused: u32,
    /// Mean time from opening a (non-key) frame's stream until the peer had all of it.
    pub mean_delivery_ms: Option<f64>,
}

/// Host side: hands frames to QUIC streams, with backpressure and delivery measurement.
#[derive(Default)]
pub(crate) struct VideoSender {
    seq: AtomicU64,
    in_flight: Arc<AtomicUsize>,
    refused: AtomicU32,
    /// (sum of delivery ms, count) of non-key frames since the last tick.
    delivery: Arc<Mutex<(f64, u32)>>,
}

impl VideoSender {
    /// Returns false if the frame was refused because the link is behind.
    pub fn send(&self, conn: &Connection, payload: Vec<u8>, keyframe: bool) -> bool {
        if self.in_flight.fetch_add(1, Ordering::AcqRel) >= MAX_IN_FLIGHT {
            self.in_flight.fetch_sub(1, Ordering::AcqRel);
            self.refused.fetch_add(1, Ordering::Relaxed);
            return false;
        }
        let seq = self.seq.fetch_add(1, Ordering::Relaxed);
        let conn = conn.clone();
        let in_flight = self.in_flight.clone();
        let delivery = self.delivery.clone();
        tokio::spawn(async move {
            let started = Instant::now();
            match send_frame(&conn, seq, keyframe, &payload).await {
                // Keyframes are several times larger; they would read as congestion.
                Ok(()) if !keyframe => {
                    let mut d = delivery.lock().unwrap();
                    d.0 += started.elapsed().as_secs_f64() * 1000.0;
                    d.1 += 1;
                }
                Ok(()) => {}
                Err(err) => debug!("video frame {seq} not delivered: {err:#}"),
            }
            in_flight.fetch_sub(1, Ordering::AcqRel);
        });
        true
    }

    /// Whether a new frame would be accepted now. The app asks before encoding; a "no"
    /// counts towards congestion.
    pub fn can_send(&self) -> bool {
        let ok = self.in_flight.load(Ordering::Acquire) < MAX_IN_FLIGHT;
        if !ok {
            self.refused.fetch_add(1, Ordering::Relaxed);
        }
        ok
    }

    pub fn take_stats(&self) -> LinkStats {
        let refused = self.refused.swap(0, Ordering::Relaxed);
        let (sum, n) = std::mem::take(&mut *self.delivery.lock().unwrap());
        LinkStats { refused, mean_delivery_ms: (n > 0).then(|| sum / n as f64) }
    }
}

async fn send_frame(conn: &Connection, seq: u64, keyframe: bool, payload: &[u8]) -> anyhow::Result<()> {
    let mut send = conn.open_uni().await?;
    send.set_priority(VIDEO_PRIORITY).ok();
    let mut header = [0u8; 10];
    header[0] = STREAM_VIDEO;
    header[1..9].copy_from_slice(&seq.to_le_bytes());
    header[9] = if keyframe { FLAG_KEYFRAME } else { 0 };
    send.write_all(&header).await?;
    send.write_all(payload).await?;
    send.finish()?;
    // Resolves once the peer has everything, which is what in-flight should measure.
    let _ = send.stopped().await;
    Ok(())
}

/// Multiplicative-increase / multiplicative-decrease on the encoder bitrate. Congestion
/// shows up as frames waiting longer to be delivered than the best case (queues filling)
/// for more than a moment, or as frames skipped for having too many in flight.
pub(crate) struct RateController {
    kbps: f64,
    max_kbps: f64,
    /// Best mean delivery time seen: the path's baseline without queueing. Drifts up
    /// slowly so a path change (e.g. relay to direct and back) is re-learned.
    base_ms: Option<f64>,
    hold_until: Option<Instant>,
    delayed_ticks: u32,
    /// No congestion seen yet: probe upwards quickly.
    slow_start: bool,
}

impl RateController {
    pub fn new(max_kbps: u32) -> RateController {
        let max_kbps = max_kbps.max(MIN_KBPS) as f64;
        RateController {
            kbps: (START_KBPS as f64).min(max_kbps),
            max_kbps,
            base_ms: None,
            hold_until: None,
            delayed_ticks: 0,
            slow_start: true,
        }
    }

    pub fn kbps(&self) -> u32 {
        self.kbps as u32
    }

    /// Returns the new bitrate when it changed meaningfully.
    pub fn tick(&mut self, stats: LinkStats, now: Instant) -> Option<u32> {
        let before = self.kbps;
        let mut congested = stats.refused >= BUSY_THRESHOLD;
        if let Some(mean) = stats.mean_delivery_ms {
            let base = self.base_ms.map_or(mean, |b| b.min(mean) * 1.002);
            self.base_ms = Some(base);
            let threshold = QUEUEING_THRESHOLD_MS.max(base * 0.5);
            if mean - base > threshold {
                self.delayed_ticks += 1;
            } else {
                self.delayed_ticks = 0;
            }
            congested |= self.delayed_ticks >= DELAY_TICKS;
        }
        if congested {
            self.kbps = (self.kbps * DECREASE).max(MIN_KBPS as f64);
            self.hold_until = Some(now + HOLD_AFTER_DECREASE);
            self.delayed_ticks = 0;
            self.slow_start = false;
        } else if self.hold_until.is_none_or(|t| now >= t) && stats.mean_delivery_ms.is_some() {
            // Only probe upwards while frames are actually flowing.
            let factor = if self.slow_start { SLOW_START_INCREASE } else { INCREASE };
            self.kbps = (self.kbps * factor).min(self.max_kbps);
        }
        let changed = (self.kbps - before).abs() / before > 0.04;
        if changed {
            info!(
                "video bitrate {} -> {} kbit/s (busy {}, delivery {:?} ms, base {:?} ms)",
                before as u32, self.kbps as u32, stats.refused, stats.mean_delivery_ms.map(|m| m.round()),
                self.base_ms.map(|b| b.round())
            );
        }
        changed.then(|| self.kbps as u32)
    }
}

pub(crate) struct Frame {
    pub payload: Vec<u8>,
    pub keyframe: bool,
}

/// Parses a video stream's contents after the kind byte.
pub(crate) fn parse_frame(bytes: Vec<u8>) -> Option<(u64, Frame)> {
    if bytes.len() < 9 {
        return None;
    }
    let seq = u64::from_le_bytes(bytes[..8].try_into().unwrap());
    let keyframe = bytes[8] & FLAG_KEYFRAME != 0;
    Some((seq, Frame { payload: bytes[9..].to_vec(), keyframe }))
}

/// Viewer side: delivers frames in order, decodable from the first keyframe on. A lost
/// frame means waiting for (and requesting) the next keyframe.
pub(crate) async fn receive_loop(
    mut frames: mpsc::UnboundedReceiver<(u64, Frame)>,
    deliver: impl Fn(Frame) + Send + 'static,
    request_keyframe: impl Fn() + Send + 'static,
) {
    let mut orderer = Orderer::default();
    let mut last_request: Option<Instant> = None;
    loop {
        let deadline = orderer.gap_deadline();
        tokio::select! {
            frame = frames.recv() => match frame {
                Some((seq, frame)) => orderer.insert(seq, frame, Instant::now()),
                None => break,
            },
            _ = tokio::time::sleep_until(deadline.unwrap_or_else(Instant::now)), if deadline.is_some() => {}
        }
        for frame in orderer.ready(Instant::now()) {
            deliver(frame);
        }
        if orderer.needs_keyframe() && last_request.is_none_or(|t| t.elapsed() >= KEYFRAME_REQUEST_INTERVAL) {
            trace!("requesting keyframe");
            last_request = Some(Instant::now());
            request_keyframe();
        }
    }
}

#[derive(Default)]
struct Orderer {
    /// Next sequence number to deliver, once synced.
    next: u64,
    synced: bool,
    pending: BTreeMap<u64, (Frame, Instant)>,
}

impl Orderer {
    fn insert(&mut self, seq: u64, frame: Frame, now: Instant) {
        if self.synced && seq < self.next {
            return; // arrived after we gave up on it
        }
        self.pending.insert(seq, (frame, now));
    }

    fn needs_keyframe(&self) -> bool {
        !self.synced
    }

    /// When the oldest out-of-order frame stops waiting for the gap before it.
    fn gap_deadline(&self) -> Option<Instant> {
        if !self.synced {
            return None;
        }
        self.pending.values().map(|(_, at)| *at + GAP_TIMEOUT).min()
    }

    fn ready(&mut self, now: Instant) -> Vec<Frame> {
        let mut out = Vec::new();
        loop {
            if !self.synced {
                // Resume at the newest keyframe; everything before it is useless.
                let Some(&seq) = self.pending.iter().rev().find(|(_, (f, _))| f.keyframe).map(|(s, _)| s) else {
                    self.pending.clear();
                    return out;
                };
                self.pending = self.pending.split_off(&seq);
                self.next = seq;
                self.synced = true;
            }
            if let Some((frame, _)) = self.pending.remove(&self.next) {
                self.next += 1;
                out.push(frame);
                continue;
            }
            let gap_expired = self.pending.values().any(|(_, at)| now.duration_since(*at) >= GAP_TIMEOUT);
            if gap_expired {
                debug!("video frame {} lost, waiting for a keyframe", self.next);
                self.synced = false;
                continue;
            }
            return out;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(keyframe: bool, tag: u8) -> Frame {
        Frame { payload: vec![tag], keyframe }
    }

    fn tags(frames: Vec<Frame>) -> Vec<u8> {
        frames.into_iter().map(|f| f.payload[0]).collect()
    }

    #[test]
    fn delivers_in_order_from_the_first_keyframe() {
        let mut o = Orderer::default();
        let t = Instant::now();
        o.insert(0, frame(false, 0), t); // not decodable: before any keyframe
        assert!(o.ready(t).is_empty());
        o.insert(2, frame(false, 2), t);
        o.insert(1, frame(true, 1), t);
        assert_eq!(tags(o.ready(t)), vec![1, 2]);
        o.insert(4, frame(false, 4), t);
        o.insert(3, frame(false, 3), t);
        assert_eq!(tags(o.ready(t)), vec![3, 4]);
    }

    #[test]
    fn lost_frame_waits_for_the_next_keyframe() {
        let mut o = Orderer::default();
        let t = Instant::now();
        o.insert(0, frame(true, 0), t);
        assert_eq!(tags(o.ready(t)), vec![0]);
        o.insert(2, frame(false, 2), t); // 1 is missing
        assert!(o.ready(t).is_empty(), "waits for the gap");
        assert!(o.ready(t + GAP_TIMEOUT).is_empty(), "gives up on frame 1");
        assert!(o.needs_keyframe());
        o.insert(1, frame(false, 1), t + GAP_TIMEOUT); // too late
        o.insert(3, frame(true, 3), t + GAP_TIMEOUT);
        o.insert(4, frame(false, 4), t + GAP_TIMEOUT);
        assert_eq!(tags(o.ready(t + GAP_TIMEOUT)), vec![3, 4]);
        assert!(!o.needs_keyframe());
    }

    fn flowing(ms: f64) -> LinkStats {
        LinkStats { refused: 0, mean_delivery_ms: Some(ms) }
    }

    #[test]
    fn rate_reaches_the_ceiling_fast_then_backs_off_on_sustained_queueing() {
        let mut rc = RateController::new(40_000);
        let mut t = Instant::now();
        assert_eq!(rc.kbps(), START_KBPS);
        // Healthy Wi-Fi (30 ms delivery): slow start reaches the ceiling within ~2.5 s.
        for _ in 0..5 {
            rc.tick(flowing(30.0), t);
            t += RATE_TICK;
        }
        assert_eq!(rc.kbps(), 40_000);
        // One jitter spike is not congestion.
        assert!(rc.tick(flowing(250.0), t).is_none());
        t += RATE_TICK;
        assert!(rc.tick(flowing(30.0), t).is_none());
        t += RATE_TICK;
        // Sustained queueing is.
        assert!(rc.tick(flowing(90.0), t).is_none());
        t += RATE_TICK;
        assert!(rc.tick(flowing(90.0), t).is_some());
        assert_eq!(rc.kbps(), 30_000);
        // Holds, then creeps up gently (slow start is over).
        t += RATE_TICK;
        assert!(rc.tick(flowing(30.0), t).is_none());
        t += HOLD_AFTER_DECREASE;
        assert!(rc.tick(flowing(30.0), t).is_some());
        assert!(rc.kbps() > 30_000 && rc.kbps() < 33_000);
    }

    #[test]
    fn busy_link_counts_as_congestion_and_rate_has_a_floor() {
        let mut rc = RateController::new(30_000);
        let t = Instant::now();
        // An occasional skipped frame is fine.
        assert!(rc.tick(LinkStats { refused: 1, mean_delivery_ms: None }, t).is_none());
        for _ in 0..20 {
            rc.tick(LinkStats { refused: 5, mean_delivery_ms: None }, t);
        }
        assert_eq!(rc.kbps(), MIN_KBPS);
        // No frames flowing (idle screen): no probing.
        assert!(rc.tick(LinkStats::default(), t + Duration::from_secs(10)).is_none());
    }
}
