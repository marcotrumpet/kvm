//! Wire protocol shared by every platform.
//!
//! Two channels run over one QUIC connection:
//! - unreliable datagrams carry [`Motion`] (pointer + scroll), where a newer state always
//!   supersedes an older one, so loss or reordering never needs a retransmit;
//! - one reliable, ordered bi-directional stream carries [`Control`] messages, which must
//!   never be lost (a dropped key-up means a stuck key).

use anyhow::{Context, Result, bail};
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

pub const ALPN: &[u8] = b"kvm/1";
pub const PROTOCOL_VERSION: u16 = 16;

/// Upper bound for a single control frame. Bulk data (clipboard) uses its own streams.
const MAX_FRAME_LEN: usize = 64 * 1024;

/// Cumulative pointer motion since the connection started.
///
/// Sending totals instead of deltas makes motion loss-tolerant: the receiver applies the
/// difference between the newest total it has seen and what it already injected.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Motion {
    pub seq: u64,
    pub x: i64,
    pub y: i64,
    pub scroll_x: i64,
    pub scroll_y: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Datagram {
    Motion(Motion),
    /// One encoded audio packet of the host's sound, while viewed with
    /// `VideoParams::audio` (format up to the apps, see docs). Lost packets are skipped.
    Audio(Vec<u8>),
    /// One encoded packet of the controller's microphone, while it controls the receiver
    /// (same format as `Audio`). The receiver offers it to its apps as a microphone.
    Microphone(Vec<u8>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Control {
    Hello { version: u16, name: String },
    Ping { nonce: u64 },
    Pong { nonce: u64 },
    /// The sender takes control of the receiver's input.
    Enter,
    /// The sender gives control back. The receiver releases anything still held.
    Leave,
    /// `code` is a USB HID keyboard usage (page 0x07). `motion` is the sender's motion
    /// total at the time of the event, so the pointer is caught up before the event lands.
    Key { code: u16, down: bool, motion: Motion },
    Button { button: u8, down: bool, motion: Motion },
    /// Media and system keys (volume, brightness, Mission Control, ...). `usage` is a USB
    /// HID consumer-page usage (page 0x0C).
    Consumer { usage: u16, down: bool },
    /// Fallback for when the path cannot carry datagrams.
    Motion(Motion),
    /// Mouse wheel notches (as opposed to trackpad scrolling, which is pixel motion).
    /// Carries the motion total like `Key`, so the wheel scrolls what the pointer is over.
    Wheel { dx: i16, dy: i16, motion: Motion },
    /// The sender wants to see the receiver's screen.
    VideoStart(crate::video::VideoParams),
    VideoStop,
    /// The viewer lost a frame and needs a keyframe to recover.
    VideoKeyframe,
    /// Move the pointer to a point on one of the receiver's displays (0: main), as
    /// fractions of its size scaled to 0..=65535 (used when clicking into the viewer).
    PointerAbs { x: u16, y: u16, display: u32 },
    /// The sender's displays and where they are; format up to the apps (see docs). Sent on
    /// connecting and whenever they change.
    DisplayList(Vec<u8>),
    /// While controlled: keep the pointer on this display (`NO_DISPLAY`: anywhere on the
    /// desktop) and report pushing past the edges in `edges` (`EDGE_*` bits) with
    /// `EdgeExit`. On the whole desktop only its outer edges count; `edges` 0 reports none.
    Confine { display: u32, edges: u8 },
    /// Controlled side to controller: the pointer was pushed past an edge (0 left, 1 right,
    /// 2 top, 3 bottom) of `display` (its id, as in the display list), `along` it at a
    /// fraction 0..=65535.
    EdgeExit { edge: u8, along: u16, display: u32 },
    /// Text to type as-is, whatever the receiver's keyboard layout (from a phone's
    /// on-screen keyboard, which has no key positions to send).
    Text(String),
    /// A trackpad gesture (from a phone's touch screen or a tablet's trackpad): `kind` is
    /// `GESTURE_MAGNIFY` (value: magnification change x 10000) or `GESTURE_ROTATE` (value:
    /// degrees counterclockwise x 100); `phase` is `PHASE_BEGAN`, `PHASE_CHANGED` or
    /// `PHASE_ENDED`. Values are increments, like macOS's own gesture events.
    Gesture { kind: u8, phase: u8, value: i32 },
    /// The sender's screen locked: lock this machine too (if its user wants that).
    Lock,
    /// The sender's Wake-on-LAN address (its Ethernet/Wi-Fi MAC, 6 bytes), to wake it later.
    WakeAddress(Vec<u8>),
    /// From the controlled side: its user took the machine back. Stop controlling and
    /// viewing it.
    TakeBack,
}

pub const GESTURE_MAGNIFY: u8 = 0;
pub const GESTURE_ROTATE: u8 = 1;
pub const PHASE_BEGAN: u8 = 1;
pub const PHASE_CHANGED: u8 = 2;
pub const PHASE_ENDED: u8 = 4;

pub fn encode_datagram(d: &Datagram) -> Vec<u8> {
    postcard::to_stdvec(d).expect("datagram serialization is infallible")
}

pub fn decode_datagram(bytes: &[u8]) -> Result<Datagram> {
    postcard::from_bytes(bytes).context("decoding datagram")
}

/// Encodes a control message as a length-prefixed frame (u32 little-endian + postcard).
pub fn encode_frame(c: &Control) -> Vec<u8> {
    encode_msg(c)
}

pub async fn read_frame(recv: &mut iroh::endpoint::RecvStream) -> Result<Control> {
    read_msg(recv).await
}

/// Length-prefixed framing (u32 little-endian + postcard) for any message type.
pub fn encode_msg<T: Serialize>(msg: &T) -> Vec<u8> {
    let body = postcard::to_stdvec(msg).expect("message serialization is infallible");
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_le_bytes());
    frame.extend_from_slice(&body);
    frame
}

pub async fn read_msg<T: DeserializeOwned>(recv: &mut iroh::endpoint::RecvStream) -> Result<T> {
    let mut len = [0u8; 4];
    recv.read_exact(&mut len).await.context("reading frame length")?;
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME_LEN {
        bail!("frame too large: {len} bytes");
    }
    let mut body = vec![0u8; len];
    recv.read_exact(&mut body).await.context("reading frame body")?;
    postcard::from_bytes(&body).context("decoding frame")
}

/// `Confine` value for "not confined".
pub const NO_DISPLAY: u32 = u32::MAX;

/// `Confine` edges, one bit per `EdgeExit` edge.
pub const EDGE_LEFT: u8 = 1 << 0;
pub const EDGE_RIGHT: u8 = 1 << 1;
pub const EDGE_TOP: u8 = 1 << 2;
pub const EDGE_BOTTOM: u8 = 1 << 3;
pub const EDGE_ALL: u8 = 0x0F;

/// First byte of every unidirectional stream: what it carries.
pub const STREAM_VIDEO: u8 = 0;
pub const STREAM_CLIPBOARD: u8 = 1;
pub const STREAM_FILE: u8 = 2;

/// Pairing runs on its own ALPN so an unpaired machine can be asked to pair without
/// being let into the input session.
pub const PAIR_ALPN: &[u8] = b"kvm-pair/1";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pair {
    Request { name: String },
    Reply { accepted: bool, name: String },
    /// Like `Request`, from a remote control (phone, tablet): it only ever controls, and is
    /// trusted in addition to the paired computer instead of replacing it.
    RequestRemote { name: String },
}

/// Six digits both machines show while pairing, derived from both ids (FNV-1a over the
/// sorted ids), so you can check you picked the right machine. Not a security measure:
/// the ids themselves are authenticated by the connection.
pub fn pairing_code(a: &[u8; 32], b: &[u8; 32]) -> String {
    let (lo, hi) = if a < b { (a, b) } else { (b, a) };
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in lo.iter().chain(hi.iter()) {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{:06}", hash % 1_000_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn control_frame_round_trip() {
        let msgs = [
            Control::Hello { version: PROTOCOL_VERSION, name: "mac".into() },
            Control::Key { code: 0x04, down: true, motion: Motion { seq: 9, x: -3, y: 7, ..Default::default() } },
            Control::Leave,
        ];
        for msg in msgs {
            let frame = encode_frame(&msg);
            let len = u32::from_le_bytes(frame[..4].try_into().unwrap()) as usize;
            assert_eq!(len, frame.len() - 4);
            let decoded: Control = postcard::from_bytes(&frame[4..]).unwrap();
            assert_eq!(decoded, msg);
        }
    }

    #[test]
    fn pairing_code_is_symmetric() {
        let a = [1u8; 32];
        let b = [2u8; 32];
        assert_eq!(pairing_code(&a, &b), pairing_code(&b, &a));
        assert_eq!(pairing_code(&a, &b).len(), 6);
        assert_ne!(pairing_code(&a, &b), pairing_code(&a, &[3u8; 32]));
    }

    #[test]
    fn motion_datagram_is_small() {
        let d = Datagram::Motion(Motion { seq: 100_000, x: 40_000, y: -12_000, scroll_x: 0, scroll_y: 300 });
        let bytes = encode_datagram(&d);
        assert!(bytes.len() <= 16, "motion datagram is {} bytes", bytes.len());
        assert_eq!(decode_datagram(&bytes).unwrap(), d);
    }
}
