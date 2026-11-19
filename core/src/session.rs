//! One live connection to the peer: framing, heartbeat, failsafe and event delivery.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use iroh::EndpointId;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use n0_future::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, trace, warn};

use crate::input::{HeldInputs, MotionReceiver, MotionSender, Release};
use crate::protocol::{self, Control, Datagram, Motion, PROTOCOL_VERSION};
use crate::video::{self, RateController, VideoParams, VideoSender};

pub const HEARTBEAT_INTERVAL: Duration = Duration::from_millis(250);
/// No pong for this long: release everything and hand control back locally.
pub const STALL_AFTER: Duration = Duration::from_secs(1);
/// No pong for this long: drop the connection so the link reconnects.
pub const DEAD_AFTER: Duration = Duration::from_secs(5);
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Clipboard contents larger than this are not shared (send big things as files).
pub const MAX_CLIPBOARD_BYTES: usize = 32 * 1024 * 1024;
const CLIPBOARD_PRIORITY: i32 = -5;
/// Longest text typed in one go (it travels in a control frame).
pub const MAX_TEXT_BYTES: usize = 16 * 1024;

/// Everything the platform layer needs to react to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Connected { peer: EndpointId, name: String },
    /// The selected network path changed. `direct` is false when traffic goes via a relay.
    Path { direct: bool, rtt: Duration },
    Latency(Duration),
    /// The peer took control of this machine: start injecting.
    Entered,
    /// The peer released control. Held inputs were already released via events.
    Left,
    Move { dx: i64, dy: i64 },
    Scroll { dx: i64, dy: i64 },
    Key { code: u16, down: bool },
    Button { button: u8, down: bool },
    /// A HID consumer-page usage (media and system keys).
    Consumer { usage: u16, down: bool },
    /// Mouse wheel notches.
    Wheel { dx: i16, dy: i16 },
    /// Text to type (layout-independent).
    Text(String),
    /// A pinch or rotation; see [`Control::Gesture`].
    Gesture { kind: u8, phase: u8, value: i32 },
    /// The peer wants to see this screen: start capturing and send frames.
    VideoRequested(VideoParams),
    /// Stop capturing (the viewer closed, or the session ended).
    VideoStopped,
    /// Encode the next frame as a keyframe.
    KeyframeRequested,
    /// Encode at this bitrate from now on (adapted to the link while sharing).
    VideoBitrate(u32),
    /// The peer copied something; payload format is up to the platform apps.
    Clipboard(Vec<u8>),
    /// Progress of our [`Session::send_files`] transfer.
    FileSendProgress { transfer: u64, sent: u64, total: u64 },
    /// Our transfer finished: delivered, or failed with `error`.
    FileSendDone { transfer: u64, error: Option<String> },
    /// The peer is sending us files; `received`/`total` cover the whole transfer.
    /// `clipboard`: files copied on the peer, cached for pasting rather than downloaded.
    FileReceiveProgress { transfer: u64, name: String, received: u64, total: u64, clipboard: bool },
    FileReceived { transfer: u64, path: std::path::PathBuf, index: u32, count: u32, clipboard: bool },
    FileReceiveFailed { transfer: u64, name: String, error: String, clipboard: bool },
    /// An encoded frame from the peer's screen, in order, decodable.
    VideoFrame { payload: Vec<u8>, keyframe: bool },
    /// Move the pointer to this point on a display (0: main; fractions scaled to 0..=65535).
    PointerAbs { x: u16, y: u16, display: u32 },
    /// The peer's displays, while viewing its screen.
    DisplayList(Vec<u8>),
    /// While controlled: keep the pointer on this display and report edge exits
    /// (`protocol::NO_DISPLAY`: no confinement).
    Confine { display: u32 },
    /// While controlling: the peer's pointer was pushed past an edge of the display we
    /// confined it to. Typically: return control here.
    EdgeExit { edge: u8, along: u16 },
    /// Heartbeat lost: held inputs were released and, if this machine was controlling the
    /// peer, control is back to local.
    Stalled,
    Recovered,
    Disconnected { reason: String },
    /// An encoded audio packet of the peer's sound, while viewing it with audio.
    Audio(Vec<u8>),
    /// An encoded packet of the peer's microphone, while it controls this machine.
    Microphone(Vec<u8>),
    /// The peer's screen locked: lock this one too.
    LockRequested,
    /// How to wake the peer later (its MAC address).
    WakeAddress(Vec<u8>),
    /// The peer's user took their machine back: control and viewing ended there.
    TakenBack,
}

pub(crate) type EventTx = mpsc::UnboundedSender<Event>;

#[derive(Default)]
struct RxState {
    motion: MotionReceiver,
    held: HeldInputs,
    controlled: bool,
}

impl RxState {
    fn catch_up(&mut self, m: Motion, events: &EventTx) {
        let Some(d) = self.motion.apply(m) else { return };
        if !self.controlled {
            return;
        }
        if d.has_pointer() {
            let _ = events.send(Event::Move { dx: d.dx, dy: d.dy });
        }
        if d.has_scroll() {
            let _ = events.send(Event::Scroll { dx: d.scroll_x, dy: d.scroll_y });
        }
    }

    /// Releases everything held and drops control, emitting the matching events.
    fn release(&mut self, events: &EventTx) {
        for r in self.held.release_all() {
            let _ = events.send(match r {
                Release::Key(code) => Event::Key { code, down: false },
                Release::Button(button) => Event::Button { button, down: false },
                Release::Consumer(usage) => Event::Consumer { usage, down: false },
            });
        }
        if std::mem::take(&mut self.controlled) {
            let _ = events.send(Event::Left);
        }
    }
}

struct Shared {
    conn: Connection,
    control_tx: mpsc::UnboundedSender<Control>,
    motion: Mutex<MotionSender>,
    controlling: AtomicBool,
    rx: Mutex<RxState>,
    pings: Mutex<HashMap<u64, Instant>>,
    started: Instant,
    /// Milliseconds since `started` at which the last pong arrived.
    last_pong_ms: AtomicU64,
    events: EventTx,
    /// The peer is viewing our screen.
    hosting: AtomicBool,
    /// We are viewing the peer's screen.
    viewing: AtomicBool,
    video: VideoSender,
    /// Adapts the bitrate while the peer views our screen.
    rate_task: Mutex<Option<JoinHandle<()>>>,
    peer_name: String,
}

impl Shared {
    fn send(&self, c: Control) {
        // A closed channel means the writer is gone and the session is shutting down.
        let _ = self.control_tx.send(c);
    }
}

/// Handle to a live session. Cheap to clone; all methods are non-blocking so they can be
/// called straight from an OS input callback.
#[derive(Clone)]
pub struct Session {
    shared: Arc<Shared>,
    supervisor: Arc<Mutex<Option<JoinHandle<()>>>>,
}

impl Session {
    /// Runs the handshake on a fresh connection and starts the session tasks.
    pub(crate) async fn establish(
        conn: Connection,
        dialer: bool,
        name: &str,
        events: EventTx,
    ) -> Result<Session> {
        let (send, recv, peer_name) = tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
            let hello = protocol::encode_frame(&Control::Hello {
                version: PROTOCOL_VERSION,
                name: name.to_string(),
            });
            // The stream only becomes visible to the peer once data is sent on it, so the
            // dialer speaks first.
            let (mut send, mut recv) = if dialer {
                let (mut send, recv) = conn.open_bi().await.context("opening control stream")?;
                send.write_all(&hello).await.context("sending hello")?;
                (send, recv)
            } else {
                conn.accept_bi().await.context("accepting control stream")?
            };
            let peer_name = match protocol::read_frame(&mut recv).await? {
                Control::Hello { version, name } if version == PROTOCOL_VERSION => name,
                Control::Hello { version, .. } => {
                    bail!("peer speaks protocol v{version}, we speak v{PROTOCOL_VERSION}")
                }
                other => bail!("expected hello, got {other:?}"),
            };
            if !dialer {
                send.write_all(&hello).await.context("sending hello")?;
            }
            Ok((send, recv, peer_name))
        })
        .await
        .context("handshake timed out")??;

        let (control_tx, control_rx) = mpsc::unbounded_channel();
        let shared = Arc::new(Shared {
            conn: conn.clone(),
            control_tx,
            motion: Mutex::default(),
            controlling: AtomicBool::new(false),
            rx: Mutex::default(),
            pings: Mutex::default(),
            started: Instant::now(),
            last_pong_ms: AtomicU64::new(0),
            events: events.clone(),
            hosting: AtomicBool::new(false),
            viewing: AtomicBool::new(false),
            video: VideoSender::default(),
            rate_task: Mutex::new(None),
            peer_name,
        });
        send.set_priority(video::CONTROL_PRIORITY).ok();

        let tasks = vec![
            tokio::spawn(write_loop(send, control_rx)),
            tokio::spawn(control_loop(shared.clone(), recv)),
            tokio::spawn(datagram_loop(shared.clone())),
            tokio::spawn(heartbeat_loop(shared.clone())),
            tokio::spawn(path_loop(shared.clone())),
            tokio::spawn(uni_loop(shared.clone())),
        ];
        let supervisor = tokio::spawn(supervise(shared.clone(), tasks));
        Ok(Session { shared, supervisor: Arc::new(Mutex::new(Some(supervisor))) })
    }

    pub fn peer(&self) -> EndpointId {
        self.shared.conn.remote_id()
    }

    /// Announces the session. Called by the link once the session is usable, so the app
    /// can act on [`Event::Connected`] straight away.
    pub(crate) fn announce(&self) {
        let _ = self.shared.events.send(Event::Connected {
            peer: self.peer(),
            name: self.shared.peer_name.clone(),
        });
    }

    pub fn is_controlling(&self) -> bool {
        self.shared.controlling.load(Ordering::Acquire)
    }

    /// Take control of the peer. Input sent before this is ignored.
    pub fn enter(&self) {
        self.shared.controlling.store(true, Ordering::Release);
        self.shared.send(Control::Enter);
    }

    pub fn leave(&self) {
        if self.shared.controlling.swap(false, Ordering::AcqRel) {
            self.shared.send(Control::Leave);
        }
    }

    pub fn send_motion(&self, dx: i64, dy: i64, scroll_x: i64, scroll_y: i64) {
        if !self.is_controlling() {
            return;
        }
        let m = self.shared.motion.lock().unwrap().add(dx, dy, scroll_x, scroll_y);
        let bytes = protocol::encode_datagram(&Datagram::Motion(m));
        if let Err(err) = self.shared.conn.send_datagram(bytes.into()) {
            trace!("datagram unavailable ({err}), sending motion reliably");
            self.shared.send(Control::Motion(m));
        }
    }

    pub fn send_key(&self, code: u16, down: bool) {
        if self.is_controlling() {
            let motion = self.shared.motion.lock().unwrap().current();
            self.shared.send(Control::Key { code, down, motion });
        }
    }

    pub fn send_button(&self, button: u8, down: bool) {
        if self.is_controlling() {
            let motion = self.shared.motion.lock().unwrap().current();
            self.shared.send(Control::Button { button, down, motion });
        }
    }

    pub fn send_consumer(&self, usage: u16, down: bool) {
        if self.is_controlling() {
            self.shared.send(Control::Consumer { usage, down });
        }
    }

    /// Asks the peer to stream its screen; frames arrive as [`Event::VideoFrame`].
    pub fn request_video(&self, params: VideoParams) {
        self.shared.viewing.store(true, Ordering::Release);
        self.shared.send(Control::VideoStart(params));
    }

    pub fn stop_video(&self) {
        if self.shared.viewing.swap(false, Ordering::AcqRel) {
            self.shared.send(Control::VideoStop);
        }
    }

    /// Whether the link can take another frame now. Ask before encoding: skipping a frame
    /// then needs no keyframe, refusing an encoded one does.
    pub fn video_can_send(&self) -> bool {
        self.shared.hosting.load(Ordering::Acquire) && self.shared.video.can_send()
    }

    /// Sends an encoded frame of our screen while the peer is viewing it. Returns false if
    /// it was refused (not viewing, or the link is behind): encode the next one as a keyframe.
    pub fn send_video_frame(&self, payload: Vec<u8>, keyframe: bool) -> bool {
        self.shared.hosting.load(Ordering::Acquire)
            && self.shared.video.send(&self.shared.conn, payload, keyframe)
    }

    /// Sends our clipboard contents to the peer, on their own stream.
    pub fn send_clipboard(&self, payload: Vec<u8>) {
        if payload.len() > MAX_CLIPBOARD_BYTES {
            warn!("clipboard too large to share ({} bytes)", payload.len());
            return;
        }
        let conn = self.shared.conn.clone();
        tokio::spawn(async move {
            let result: Result<()> = async {
                let mut send = conn.open_uni().await?;
                send.set_priority(CLIPBOARD_PRIORITY).ok();
                send.write_all(&[protocol::STREAM_CLIPBOARD]).await?;
                send.write_all(&payload).await?;
                send.finish()?;
                Ok(())
            }
            .await;
            if let Err(err) = result {
                warn!("sending clipboard: {err:#}");
            }
        });
    }

    /// Sends files and folders to the peer's download folder, or with `clipboard` to its
    /// clipboard cache (files copied here, to paste there). Returns the transfer id that
    /// the progress and completion events carry.
    pub fn send_files(&self, paths: Vec<std::path::PathBuf>, clipboard: bool) -> u64 {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let transfer = NEXT.fetch_add(1, Ordering::Relaxed) ^ (std::process::id() as u64) << 32;
        tokio::spawn(crate::files::send(self.shared.conn.clone(), transfer, paths, clipboard, self.shared.events.clone()));
        transfer
    }

    /// Moves the peer's pointer to a point on one of its displays, while controlling it.
    pub fn pointer_abs(&self, x: u16, y: u16, display: u32) {
        if self.is_controlling() {
            self.shared.send(Control::PointerAbs { x, y, display });
        }
    }

    /// While controlling: confines the peer's pointer to one of its displays and asks for
    /// edge exits (`protocol::NO_DISPLAY` to lift it).
    pub fn confine(&self, display: u32) {
        if self.is_controlling() {
            self.shared.send(Control::Confine { display });
        }
    }

    /// While controlled and confined: the pointer was pushed past an edge.
    pub fn edge_exit(&self, edge: u8, along: u16) {
        if self.shared.rx.lock().unwrap().controlled {
            self.shared.send(Control::EdgeExit { edge, along });
        }
    }

    /// Tells the viewer which displays this machine has (while it views our screen).
    pub fn send_display_list(&self, list: Vec<u8>) {
        if self.shared.hosting.load(Ordering::Acquire) {
            self.shared.send(Control::DisplayList(list));
        }
    }

    pub fn send_wheel(&self, dx: i16, dy: i16) {
        if self.is_controlling() {
            let motion = self.shared.motion.lock().unwrap().current();
            self.shared.send(Control::Wheel { dx, dy, motion });
        }
    }

    /// Types `text` on the peer, while controlling it.
    pub fn send_text(&self, text: String) {
        if text.len() > MAX_TEXT_BYTES {
            warn!("text too long to type ({} bytes)", text.len());
        } else if self.is_controlling() && !text.is_empty() {
            self.shared.send(Control::Text(text));
        }
    }

    /// A pinch or rotation on the peer, while controlling it; see [`Control::Gesture`].
    pub fn send_gesture(&self, kind: u8, phase: u8, value: i32) {
        if self.is_controlling() {
            self.shared.send(Control::Gesture { kind, phase, value });
        }
    }

    /// Sends one encoded audio packet while the peer views our screen. Dropped when the
    /// path carries no datagrams: late sound is worse than none.
    pub fn send_audio(&self, payload: Vec<u8>) {
        if self.shared.hosting.load(Ordering::Acquire) {
            let bytes = protocol::encode_datagram(&Datagram::Audio(payload));
            if let Err(err) = self.shared.conn.send_datagram(bytes.into()) {
                trace!("audio packet dropped: {err}");
            }
        }
    }

    /// Sends one encoded packet of our microphone while we control the peer. Dropped like
    /// `send_audio` when the path carries no datagrams.
    pub fn send_microphone(&self, payload: Vec<u8>) {
        if self.shared.controlling.load(Ordering::Acquire) {
            let bytes = protocol::encode_datagram(&Datagram::Microphone(payload));
            if let Err(err) = self.shared.conn.send_datagram(bytes.into()) {
                trace!("microphone packet dropped: {err}");
            }
        }
    }

    /// Our screen locked: asks the peer to lock too.
    pub fn send_lock(&self) {
        self.shared.send(Control::Lock);
    }

    /// Tells the peer how to wake this machine (see [`Control::WakeAddress`]).
    pub fn send_wake_address(&self, mac: Vec<u8>) {
        self.shared.send(Control::WakeAddress(mac));
    }

    /// The user here takes the machine back from the peer: releases its input, stops
    /// sharing the screen, and tells the peer to stop. False if it wasn't using it.
    pub fn take_back(&self) -> bool {
        let events = &self.shared.events;
        let controlled = {
            let mut rx = self.shared.rx.lock().unwrap();
            let controlled = rx.controlled;
            if controlled {
                rx.release(events); // lifts held input, reports `Left`
            }
            controlled
        };
        let hosting = self.shared.hosting.swap(false, Ordering::AcqRel);
        if !controlled && !hosting {
            return false;
        }
        if hosting {
            stop_rate_control(&self.shared);
            let _ = events.send(Event::VideoStopped);
        }
        self.shared.send(Control::TakeBack);
        true
    }

    /// Sends an extra ping; the answer arrives as [`Event::Latency`].
    pub fn ping(&self) {
        send_ping(&self.shared);
    }

    /// Closes the connection and waits until its final events (releases,
    /// [`Event::Disconnected`]) have been emitted.
    pub async fn shutdown(&self, reason: &str) {
        self.shared.conn.close(0u32.into(), reason.as_bytes());
        self.finished().await;
    }

    /// Resolves once the session has ended for any reason.
    pub async fn finished(&self) {
        let handle = self.supervisor.lock().unwrap().take();
        if let Some(handle) = handle {
            let _ = handle.await;
        } else {
            // Another caller is awaiting the supervisor; wait for the connection instead.
            self.shared.conn.closed().await;
        }
    }
}

fn send_ping(shared: &Shared) {
    static NONCE: AtomicU64 = AtomicU64::new(0);
    let nonce = NONCE.fetch_add(1, Ordering::Relaxed);
    let now = Instant::now();
    let mut pings = shared.pings.lock().unwrap();
    pings.retain(|_, sent| now.duration_since(*sent) < DEAD_AFTER);
    pings.insert(nonce, now);
    drop(pings);
    shared.send(Control::Ping { nonce });
}

async fn write_loop(mut send: SendStream, mut rx: mpsc::UnboundedReceiver<Control>) {
    let mut buf = Vec::new();
    while let Some(first) = rx.recv().await {
        // Batch whatever is already queued into one write.
        buf.clear();
        buf.extend(protocol::encode_frame(&first));
        while let Ok(next) = rx.try_recv() {
            buf.extend(protocol::encode_frame(&next));
        }
        if let Err(err) = send.write_all(&buf).await {
            debug!("control stream write failed: {err}");
            return;
        }
    }
}

async fn control_loop(shared: Arc<Shared>, mut recv: RecvStream) {
    loop {
        let msg = match protocol::read_frame(&mut recv).await {
            Ok(msg) => msg,
            Err(err) => {
                debug!("control stream ended: {err:#}");
                return;
            }
        };
        let events = &shared.events;
        match msg {
            Control::Ping { nonce } => shared.send(Control::Pong { nonce }),
            Control::Pong { nonce } => {
                let sent = shared.pings.lock().unwrap().remove(&nonce);
                if let Some(sent) = sent {
                    let ms = shared.started.elapsed().as_millis() as u64;
                    shared.last_pong_ms.store(ms, Ordering::Release);
                    let _ = events.send(Event::Latency(sent.elapsed()));
                }
            }
            Control::Enter => {
                let mut rx = shared.rx.lock().unwrap();
                if !rx.controlled {
                    rx.controlled = true;
                    let _ = events.send(Event::Entered);
                }
            }
            Control::Leave => shared.rx.lock().unwrap().release(events),
            Control::Motion(m) => shared.rx.lock().unwrap().catch_up(m, events),
            Control::Key { code, down, motion } => {
                let mut rx = shared.rx.lock().unwrap();
                rx.catch_up(motion, events);
                if rx.controlled && rx.held.key(code, down) {
                    let _ = events.send(Event::Key { code, down });
                }
            }
            Control::Button { button, down, motion } => {
                let mut rx = shared.rx.lock().unwrap();
                rx.catch_up(motion, events);
                if rx.controlled && rx.held.button(button, down) {
                    let _ = events.send(Event::Button { button, down });
                }
            }
            Control::Consumer { usage, down } => {
                let mut rx = shared.rx.lock().unwrap();
                if rx.controlled && rx.held.consumer(usage, down) {
                    let _ = events.send(Event::Consumer { usage, down });
                }
            }
            Control::VideoStart(params) => {
                shared.hosting.store(true, Ordering::Release);
                let _ = events.send(Event::VideoRequested(params));
                start_rate_control(&shared, params.bitrate_kbps);
            }
            Control::VideoStop => {
                if shared.hosting.swap(false, Ordering::AcqRel) {
                    stop_rate_control(&shared);
                    let _ = events.send(Event::VideoStopped);
                }
            }
            Control::VideoKeyframe => {
                let _ = events.send(Event::KeyframeRequested);
            }
            Control::PointerAbs { x, y, display } => {
                if shared.rx.lock().unwrap().controlled {
                    let _ = events.send(Event::PointerAbs { x, y, display });
                }
            }
            Control::Confine { display } => {
                if shared.rx.lock().unwrap().controlled {
                    let _ = events.send(Event::Confine { display });
                }
            }
            Control::EdgeExit { edge, along } => {
                if shared.controlling.load(Ordering::Acquire) {
                    let _ = events.send(Event::EdgeExit { edge, along });
                }
            }
            Control::DisplayList(list) => {
                if shared.viewing.load(Ordering::Acquire) {
                    let _ = events.send(Event::DisplayList(list));
                }
            }
            Control::Wheel { dx, dy, motion } => {
                let mut rx = shared.rx.lock().unwrap();
                rx.catch_up(motion, events);
                if rx.controlled {
                    let _ = events.send(Event::Wheel { dx, dy });
                }
            }
            Control::Text(text) => {
                if shared.rx.lock().unwrap().controlled {
                    let _ = events.send(Event::Text(text));
                }
            }
            Control::Gesture { kind, phase, value } => {
                if shared.rx.lock().unwrap().controlled {
                    let _ = events.send(Event::Gesture { kind, phase, value });
                }
            }
            Control::Lock => {
                let _ = events.send(Event::LockRequested);
            }
            Control::WakeAddress(mac) => {
                if mac.len() == 6 {
                    let _ = events.send(Event::WakeAddress(mac));
                }
            }
            Control::TakeBack => {
                let was_controlling = shared.controlling.swap(false, Ordering::AcqRel);
                let was_viewing = shared.viewing.swap(false, Ordering::AcqRel);
                if was_controlling || was_viewing {
                    let _ = events.send(Event::TakenBack);
                }
            }
            Control::Hello { .. } => warn!("unexpected hello mid-session"),
        }
    }
}

async fn datagram_loop(shared: Arc<Shared>) {
    while let Ok(bytes) = shared.conn.read_datagram().await {
        match protocol::decode_datagram(&bytes) {
            Ok(Datagram::Motion(m)) => shared.rx.lock().unwrap().catch_up(m, &shared.events),
            Ok(Datagram::Audio(payload)) => {
                if shared.viewing.load(Ordering::Acquire) {
                    let _ = shared.events.send(Event::Audio(payload));
                }
            }
            Ok(Datagram::Microphone(payload)) => {
                if shared.rx.lock().unwrap().controlled {
                    let _ = shared.events.send(Event::Microphone(payload));
                }
            }
            Err(err) => warn!("bad datagram: {err:#}"),
        }
    }
}

async fn heartbeat_loop(shared: Arc<Shared>) {
    let mut tick = tokio::time::interval(HEARTBEAT_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut stalled = false;
    loop {
        tick.tick().await;
        send_ping(&shared);
        let now_ms = shared.started.elapsed().as_millis() as u64;
        let silent = Duration::from_millis(now_ms - shared.last_pong_ms.load(Ordering::Acquire));
        if silent >= DEAD_AFTER {
            warn!("no heartbeat for {silent:?}, closing connection");
            shared.conn.close(1u32.into(), b"heartbeat timeout");
            return;
        }
        if silent >= STALL_AFTER && !stalled {
            stalled = true;
            warn!("no heartbeat for {silent:?}, releasing input");
            shared.controlling.store(false, Ordering::Release);
            shared.rx.lock().unwrap().release(&shared.events);
            let _ = shared.events.send(Event::Stalled);
        } else if silent < STALL_AFTER && stalled {
            stalled = false;
            let _ = shared.events.send(Event::Recovered);
        }
    }
}

async fn path_loop(shared: Arc<Shared>) {
    let conn = shared.conn.clone();
    let mut paths = conn.paths_stream();
    let mut last_direct = None;
    while let Some(list) = paths.next().await {
        let Some(path) = list.iter().find(|p| p.is_selected()) else { continue };
        let direct = path.is_ip();
        if last_direct != Some(direct) {
            last_direct = Some(direct);
            let _ = shared.events.send(Event::Path { direct, rtt: path.rtt() });
        }
    }
}

fn start_rate_control(shared: &Arc<Shared>, max_kbps: u32) {
    let mut rc = RateController::new(max_kbps);
    let _ = shared.events.send(Event::VideoBitrate(rc.kbps()));
    shared.video.take_stats(); // drop measurements from an earlier viewing
    let task = tokio::spawn({
        let shared = shared.clone();
        async move {
            let mut tick = tokio::time::interval(video::RATE_TICK);
            tick.tick().await;
            loop {
                tick.tick().await;
                if let Some(kbps) = rc.tick(shared.video.take_stats(), tokio::time::Instant::now()) {
                    let _ = shared.events.send(Event::VideoBitrate(kbps));
                }
            }
        }
    });
    if let Some(old) = shared.rate_task.lock().unwrap().replace(task) {
        old.abort();
    }
}

fn stop_rate_control(shared: &Shared) {
    if let Some(task) = shared.rate_task.lock().unwrap().take() {
        task.abort();
    }
}

/// Accepts the peer's unidirectional streams and routes them by their first byte.
async fn uni_loop(shared: Arc<Shared>) {
    let (frames_tx, frames_rx) = mpsc::unbounded_channel();
    let deliver = {
        let shared = shared.clone();
        move |frame: video::Frame| {
            if shared.viewing.load(Ordering::Acquire) {
                let _ = shared.events.send(Event::VideoFrame { payload: frame.payload, keyframe: frame.keyframe });
            }
        }
    };
    let request_keyframe = {
        let shared = shared.clone();
        move || {
            if shared.viewing.load(Ordering::Acquire) {
                shared.send(Control::VideoKeyframe);
            }
        }
    };
    let orderer = tokio::spawn(video::receive_loop(frames_rx, deliver, request_keyframe));

    while let Ok(mut recv) = shared.conn.accept_uni().await {
        let shared = shared.clone();
        let frames_tx = frames_tx.clone();
        tokio::spawn(async move {
            let mut kind = [0u8; 1];
            if recv.read_exact(&mut kind).await.is_err() {
                return;
            }
            match kind[0] {
                protocol::STREAM_VIDEO => {
                    let Ok(bytes) = recv.read_to_end(video::MAX_FRAME_BYTES).await else { return };
                    if let Some(frame) = video::parse_frame(bytes) {
                        let _ = frames_tx.send(frame);
                    }
                }
                protocol::STREAM_CLIPBOARD => match recv.read_to_end(MAX_CLIPBOARD_BYTES).await {
                    Ok(bytes) => {
                        let _ = shared.events.send(Event::Clipboard(bytes));
                    }
                    Err(err) => warn!("clipboard from peer not received: {err}"),
                },
                protocol::STREAM_FILE => crate::files::receive(recv, shared.events.clone()).await,
                other => warn!("unknown stream kind {other}"),
            }
        });
    }
    orderer.abort();
}

/// Waits for the connection to end, stops the session tasks and emits the final events.
async fn supervise(shared: Arc<Shared>, tasks: Vec<JoinHandle<()>>) {
    let reason = shared.conn.closed().await.to_string();
    for task in &tasks {
        task.abort();
    }
    for task in tasks {
        let _ = task.await;
    }
    shared.controlling.store(false, Ordering::Release);
    shared.rx.lock().unwrap().release(&shared.events);
    if shared.hosting.swap(false, Ordering::AcqRel) {
        stop_rate_control(&shared);
        let _ = shared.events.send(Event::VideoStopped);
    }
    let _ = shared.events.send(Event::Disconnected { reason });
}
