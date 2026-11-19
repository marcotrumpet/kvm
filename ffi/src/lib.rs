//! C ABI over `kvm-core` for the platform apps (Swift on macOS, C# on Windows).
//! The matching header is `ffi/include/kvm.h`; keep the two in sync.

use std::ffi::{CStr, CString, c_char, c_void};
use std::collections::HashSet;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use kvm_core::{EndpointId, Event, Link, Node, NodeEvent, VideoParams};
use tokio::runtime::Runtime;
use tokio::task::JoinHandle;

pub const KVM_EV_CONNECTED: u32 = 0;
pub const KVM_EV_PATH: u32 = 1;
pub const KVM_EV_LATENCY: u32 = 2;
pub const KVM_EV_ENTERED: u32 = 3;
pub const KVM_EV_LEFT: u32 = 4;
pub const KVM_EV_MOVE: u32 = 5;
pub const KVM_EV_SCROLL: u32 = 6;
pub const KVM_EV_KEY: u32 = 7;
pub const KVM_EV_BUTTON: u32 = 8;
pub const KVM_EV_STALLED: u32 = 9;
pub const KVM_EV_RECOVERED: u32 = 10;
pub const KVM_EV_DISCONNECTED: u32 = 11;
pub const KVM_EV_CONSUMER: u32 = 12;
pub const KVM_EV_WHEEL: u32 = 13;
pub const KVM_EV_VIDEO_REQUESTED: u32 = 14;
pub const KVM_EV_VIDEO_STOPPED: u32 = 15;
pub const KVM_EV_KEYFRAME_REQUESTED: u32 = 16;
pub const KVM_EV_VIDEO_FRAME: u32 = 17;
pub const KVM_EV_POINTER_ABS: u32 = 18;
pub const KVM_EV_VIDEO_BITRATE: u32 = 19;
pub const KVM_EV_CLIPBOARD: u32 = 30;
pub const KVM_EV_DISPLAY_LIST: u32 = 36;
pub const KVM_EV_CONFINE: u32 = 37;
pub const KVM_EV_EDGE_EXIT: u32 = 38;
pub const KVM_EV_TEXT: u32 = 39;
pub const KVM_EV_GESTURE: u32 = 40;
pub const KVM_EV_FILE_SEND_PROGRESS: u32 = 31;
pub const KVM_EV_FILE_SEND_DONE: u32 = 32;
pub const KVM_EV_FILE_RECEIVE_PROGRESS: u32 = 33;
pub const KVM_EV_FILE_RECEIVED: u32 = 34;
pub const KVM_EV_FILE_RECEIVE_FAILED: u32 = 35;
pub const KVM_EV_DISCOVERED: u32 = 20;
pub const KVM_EV_EXPIRED: u32 = 21;
pub const KVM_EV_PAIR_REQUEST: u32 = 22;
pub const KVM_EV_PAIR_RESULT: u32 = 23;

/// See `kvm.h` for the meaning of each field per event kind.
#[repr(C)]
pub struct KvmEvent {
    pub kind: u32,
    pub a: i64,
    pub b: i64,
    pub flag: bool,
    pub text: *const c_char,
    /// Peer id (hex) for machine-level events, else null.
    pub id: *const c_char,
    /// Video frame payload (KVM_EV_VIDEO_FRAME), else null.
    pub data: *const u8,
    pub data_len: usize,
}

pub type KvmEventCallback = extern "C" fn(ctx: *mut c_void, event: *const KvmEvent);

pub struct KvmNode {
    rt: Arc<Runtime>,
    node: Node,
    /// A second handle on the same node (`kvm_node_remotes`): it owns only its link.
    secondary: bool,
    link: Mutex<Option<Link>>,
    dispatcher: Mutex<Option<JoinHandle<()>>>,
    watcher: Mutex<Option<JoinHandle<()>>>,
}

struct CallbackCtx(*mut c_void);
// The app guarantees `ctx` stays valid and usable from any thread until the peer is
// cleared or the node is freed.
unsafe impl Send for CallbackCtx {}

unsafe fn str_arg<'a>(p: *const c_char) -> Option<&'a str> {
    if p.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(p) }.to_str().ok()
}

fn write_c_str(s: &str, buf: *mut c_char, len: usize) {
    if buf.is_null() || len == 0 {
        return;
    }
    let n = s.len().min(len - 1);
    unsafe {
        std::ptr::copy_nonoverlapping(s.as_ptr(), buf as *mut u8, n);
        *buf.add(n) = 0;
    }
}

/// Sends logs to `path` (appending). Only the first call has an effect.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_init_logging(path: *const c_char) {
    static ONCE: OnceLock<()> = OnceLock::new();
    let Some(path) = (unsafe { str_arg(path) }) else { return };
    ONCE.get_or_init(|| {
        let Ok(file) = std::fs::OpenOptions::new().create(true).append(true).open(path) else {
            return;
        };
        let filter = tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "warn,kvm_core=info,app=info".into());
        let _ = tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(Mutex::new(file))
            .try_init();
    });
}

/// Loads (or creates) the identity in `state_dir` and binds the endpoint.
/// Returns null on failure and writes the reason into `err`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_start(
    state_dir: *const c_char,
    name: *const c_char,
    err: *mut c_char,
    err_len: usize,
) -> *mut KvmNode {
    let (Some(state_dir), Some(name)) = (unsafe { str_arg(state_dir) }, unsafe { str_arg(name) })
    else {
        write_c_str("state_dir and name must be valid UTF-8", err, err_len);
        return std::ptr::null_mut();
    };
    let rt = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("kvm-core")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            write_c_str(&format!("starting runtime: {e}"), err, err_len);
            return std::ptr::null_mut();
        }
    };
    match rt.block_on(Node::bind(state_dir.as_ref(), name)) {
        Ok(node) => Box::into_raw(Box::new(KvmNode {
            rt: Arc::new(rt),
            node,
            secondary: false,
            link: Mutex::new(None),
            dispatcher: Mutex::new(None),
            watcher: Mutex::new(None),
        })),
        Err(e) => {
            write_c_str(&format!("{e:#}"), err, err_len);
            std::ptr::null_mut()
        }
    }
}

/// Writes this machine's id (64 hex chars + NUL) into `buf`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_id(node: *const KvmNode, buf: *mut c_char, len: usize) {
    let node = unsafe { &*node };
    write_c_str(&node.node.id().to_string(), buf, len);
}

/// Starts (or replaces) the link to `peer`. Events are delivered to `callback` in order,
/// on a background thread. Returns false if `peer` is not a valid id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_set_peer(
    node: *const KvmNode,
    peer: *const c_char,
    callback: KvmEventCallback,
    ctx: *mut c_void,
) -> bool {
    let Some(peer) = (unsafe { peer_arg(peer) }) else { return false };
    let node = unsafe { &*node };
    start_link(node, callback, ctx, |n| n.link(peer));
    true
}

/// Like `kvm_node_set_peer`, but this side always dials: a remote control (phone, tablet)
/// connecting to a computer that trusts it through `kvm_node_set_remotes`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_dial_peer(
    node: *const KvmNode,
    peer: *const c_char,
    callback: KvmEventCallback,
    ctx: *mut c_void,
) -> bool {
    let Some(peer) = (unsafe { peer_arg(peer) }) else { return false };
    let node = unsafe { &*node };
    start_link(node, callback, ctx, |n| n.dial(peer));
    true
}

/// A second handle on `node` for the link to remote controls (`kvm_node_set_remotes`). Every
/// per-link function works on it; free it with `kvm_node_free` before freeing `node`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_remotes(node: *const KvmNode) -> *mut KvmNode {
    let node = unsafe { &*node };
    Box::into_raw(Box::new(KvmNode {
        rt: node.rt.clone(),
        node: node.node.clone(),
        secondary: true,
        link: Mutex::new(None),
        dispatcher: Mutex::new(None),
        watcher: Mutex::new(None),
    }))
}

/// Accepts sessions from these remote controls (ids, `count` of them), one at a time, next
/// to the paired peer's link; replaces the previous set. `count` 0 just stops. Call it on a
/// `kvm_node_remotes` handle. False if an id is invalid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_set_remotes(
    node: *const KvmNode,
    ids: *const *const c_char,
    count: usize,
    callback: KvmEventCallback,
    ctx: *mut c_void,
) -> bool {
    let mut allowed = HashSet::new();
    for i in 0..count {
        let Some(id) = (unsafe { peer_arg(*ids.add(i)) }) else { return false };
        allowed.insert(id);
    }
    let node = unsafe { &*node };
    if allowed.is_empty() {
        unsafe { kvm_node_clear_peer(node) };
    } else {
        start_link(node, callback, ctx, |n| n.accept_remotes(allowed));
    }
    true
}

fn start_link(
    node: &KvmNode,
    callback: KvmEventCallback,
    ctx: *mut c_void,
    make: impl FnOnce(&Node) -> (Link, tokio::sync::mpsc::UnboundedReceiver<Event>),
) {
    unsafe { kvm_node_clear_peer(node) };
    let _guard = node.rt.enter();
    let (link, mut events) = make(&node.node);
    let ctx = CallbackCtx(ctx);
    let dispatcher = node.rt.spawn(async move {
        let ctx = ctx;
        while let Some(event) = events.recv().await {
            dispatch(&event, callback, ctx.0);
        }
    });
    *node.link.lock().unwrap() = Some(link);
    *node.dispatcher.lock().unwrap() = Some(dispatcher);
}

/// Drops the link. No callbacks run after this returns.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_clear_peer(node: *const KvmNode) {
    let node = unsafe { &*node };
    drop(node.link.lock().unwrap().take());
    if let Some(dispatcher) = node.dispatcher.lock().unwrap().take() {
        dispatcher.abort();
        let _ = node.rt.block_on(dispatcher);
    }
}

fn dispatch(event: &Event, callback: KvmEventCallback, ctx: *mut c_void) {
    let mut text = None;
    let mut data: &[u8] = &[];
    let mut id = None;
    let (kind, a, b, flag) = match event {
        Event::Connected { name, peer } => {
            text = CString::new(name.as_str()).ok();
            id = CString::new(peer.to_string()).ok();
            (KVM_EV_CONNECTED, 0, 0, false)
        }
        Event::Path { direct, rtt } => (KVM_EV_PATH, rtt.as_micros() as i64, 0, *direct),
        Event::Latency(rtt) => (KVM_EV_LATENCY, rtt.as_micros() as i64, 0, false),
        Event::Entered => (KVM_EV_ENTERED, 0, 0, false),
        Event::Left => (KVM_EV_LEFT, 0, 0, false),
        Event::Move { dx, dy } => (KVM_EV_MOVE, *dx, *dy, false),
        Event::Scroll { dx, dy } => (KVM_EV_SCROLL, *dx, *dy, false),
        Event::Key { code, down } => (KVM_EV_KEY, *code as i64, 0, *down),
        Event::Button { button, down } => (KVM_EV_BUTTON, *button as i64, 0, *down),
        Event::Consumer { usage, down } => (KVM_EV_CONSUMER, *usage as i64, 0, *down),
        Event::Wheel { dx, dy } => (KVM_EV_WHEEL, *dx as i64, *dy as i64, false),
        Event::Gesture { kind, phase, value } => {
            (KVM_EV_GESTURE, ((*kind as i64) << 8) | *phase as i64, *value as i64, false)
        }
        Event::Text(t) => {
            text = CString::new(t.as_str()).ok();
            (KVM_EV_TEXT, 0, 0, false)
        }
        Event::VideoRequested(p) => (
            KVM_EV_VIDEO_REQUESTED,
            ((p.display as i64) << 32) | ((p.max_width as i64) << 16) | p.max_height as i64,
            ((p.fit_host as i64) << 48) | ((p.prefer as i64) << 40) | ((p.fps as i64) << 32) | p.bitrate_kbps as i64,
            false,
        ),
        Event::VideoStopped => (KVM_EV_VIDEO_STOPPED, 0, 0, false),
        Event::KeyframeRequested => (KVM_EV_KEYFRAME_REQUESTED, 0, 0, false),
        Event::VideoFrame { payload, keyframe } => {
            data = payload;
            (KVM_EV_VIDEO_FRAME, 0, 0, *keyframe)
        }
        Event::PointerAbs { x, y, display } => {
            (KVM_EV_POINTER_ABS, ((*display as i64) << 16) | *x as i64, *y as i64, false)
        }
        Event::Confine { display } => (KVM_EV_CONFINE, *display as i64, 0, false),
        Event::EdgeExit { edge, along } => (KVM_EV_EDGE_EXIT, *edge as i64, *along as i64, false),
        Event::DisplayList(list) => {
            data = list;
            (KVM_EV_DISPLAY_LIST, 0, 0, false)
        }
        Event::VideoBitrate(kbps) => (KVM_EV_VIDEO_BITRATE, *kbps as i64, 0, false),
        Event::Clipboard(payload) => {
            data = payload;
            (KVM_EV_CLIPBOARD, 0, 0, false)
        }
        Event::FileSendProgress { transfer, sent, total } => {
            id = CString::new(transfer.to_string()).ok();
            (KVM_EV_FILE_SEND_PROGRESS, *sent as i64, *total as i64, false)
        }
        Event::FileSendDone { transfer, error } => {
            id = CString::new(transfer.to_string()).ok();
            text = error.as_ref().and_then(|e| CString::new(e.as_str()).ok());
            (KVM_EV_FILE_SEND_DONE, 0, 0, error.is_none())
        }
        Event::FileReceiveProgress { transfer, name, received, total, clipboard } => {
            id = CString::new(transfer.to_string()).ok();
            text = CString::new(name.as_str()).ok();
            (KVM_EV_FILE_RECEIVE_PROGRESS, *received as i64, *total as i64, *clipboard)
        }
        Event::FileReceived { transfer, path, index, count, clipboard } => {
            id = CString::new(transfer.to_string()).ok();
            text = CString::new(path.to_string_lossy().as_ref()).ok();
            (KVM_EV_FILE_RECEIVED, *index as i64, *count as i64, *clipboard)
        }
        Event::FileReceiveFailed { transfer, name, error, clipboard } => {
            id = CString::new(transfer.to_string()).ok();
            text = CString::new(format!("{name}: {error}")).ok();
            (KVM_EV_FILE_RECEIVE_FAILED, 0, 0, *clipboard)
        }
        Event::Stalled => (KVM_EV_STALLED, 0, 0, false),
        Event::Recovered => (KVM_EV_RECOVERED, 0, 0, false),
        Event::Disconnected { reason } => {
            text = CString::new(reason.as_str()).ok();
            (KVM_EV_DISCONNECTED, 0, 0, false)
        }
    };
    let ev = KvmEvent {
        kind,
        a,
        b,
        flag,
        text: text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
        id: id.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
        data: if data.is_empty() { std::ptr::null() } else { data.as_ptr() },
        data_len: data.len(),
    };
    callback(ctx, &ev);
}

/// Delivers machine-level events (discovery, pairing) to `callback`, in order, on a
/// background thread, until the node is freed. Can be called once.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_watch(
    node: *const KvmNode,
    callback: KvmEventCallback,
    ctx: *mut c_void,
) -> bool {
    let node = unsafe { &*node };
    let Some(mut events) = node.node.events() else { return false };
    let ctx = CallbackCtx(ctx);
    let watcher = node.rt.spawn(async move {
        let ctx = ctx;
        while let Some(event) = events.recv().await {
            dispatch_node(&event, callback, ctx.0);
        }
    });
    *node.watcher.lock().unwrap() = Some(watcher);
    true
}

fn dispatch_node(event: &NodeEvent, callback: KvmEventCallback, ctx: *mut c_void) {
    let (kind, id, text, a, flag) = match event {
        NodeEvent::Discovered { id, name } => (KVM_EV_DISCOVERED, id, Some(name.clone()), 0, false),
        NodeEvent::Expired { id } => (KVM_EV_EXPIRED, id, None, 0, false),
        NodeEvent::PairRequest { id, name, code, remote } => {
            (KVM_EV_PAIR_REQUEST, id, Some(name.clone()), code.parse().unwrap_or(0), *remote)
        }
        NodeEvent::PairResult { id, name, accepted, error } => {
            let failed = error.is_some() as i64;
            (KVM_EV_PAIR_RESULT, id, Some(error.clone().unwrap_or_else(|| name.clone())), failed, *accepted)
        }
    };
    let id = CString::new(id.to_string()).ok();
    let text = text.and_then(|t| CString::new(t).ok());
    let ev = KvmEvent {
        kind,
        a,
        b: 0,
        flag,
        text: text.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
        id: id.as_ref().map_or(std::ptr::null(), |t| t.as_ptr()),
        data: std::ptr::null(),
        data_len: 0,
    };
    callback(ctx, &ev);
}

unsafe fn peer_arg(peer: *const c_char) -> Option<EndpointId> {
    unsafe { str_arg(peer) }.and_then(|p| p.trim().parse().ok())
}

/// Pairing mode: while on, nearby machines list this one and pairing requests reach the
/// user; while off (the default) requests are declined automatically.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_set_discoverable(node: *const KvmNode, on: bool) {
    let node = unsafe { &*node };
    let _guard = node.rt.enter();
    node.node.set_discoverable(on);
}

/// Asks `peer` to pair; the answer arrives as KVM_EV_PAIR_RESULT. False if `peer` is invalid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_pair_request(node: *const KvmNode, peer: *const c_char) -> bool {
    unsafe { pair_request(node, peer, false) }
}

/// Like `kvm_pair_request`, as a remote control: the computer trusts this device next to
/// its paired computer rather than instead of it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_pair_request_remote(node: *const KvmNode, peer: *const c_char) -> bool {
    unsafe { pair_request(node, peer, true) }
}

unsafe fn pair_request(node: *const KvmNode, peer: *const c_char, remote: bool) -> bool {
    let node = unsafe { &*node };
    let Some(peer) = (unsafe { peer_arg(peer) }) else { return false };
    let _guard = node.rt.enter();
    node.node.request_pairing(peer, remote);
    true
}

/// Writes the 6-digit code both machines show while pairing with `peer` (len >= 7).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_pairing_code(node: *const KvmNode, peer: *const c_char, buf: *mut c_char, len: usize) -> bool {
    let node = unsafe { &*node };
    let Some(peer) = (unsafe { peer_arg(peer) }) else { return false };
    let code = kvm_core::protocol::pairing_code(node.node.id().as_bytes(), peer.as_bytes());
    write_c_str(&code, buf, len);
    true
}

/// Answers a KVM_EV_PAIR_REQUEST. False if the request is no longer pending.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_pair_respond(node: *const KvmNode, peer: *const c_char, accept: bool) -> bool {
    let node = unsafe { &*node };
    unsafe { peer_arg(peer) }.is_some_and(|peer| node.node.respond_pairing(peer, accept))
}

fn with_link(node: *const KvmNode, f: impl FnOnce(&Link)) {
    let node = unsafe { &*node };
    if let Some(link) = node.link.lock().unwrap().as_ref() {
        f(link);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_enter(node: *const KvmNode) {
    with_link(node, Link::enter)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_leave(node: *const KvmNode) {
    with_link(node, Link::leave)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_is_connected(node: *const KvmNode) -> bool {
    let mut connected = false;
    with_link(node, |l| connected = l.is_connected());
    connected
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_is_controlling(node: *const KvmNode) -> bool {
    let mut controlling = false;
    with_link(node, |l| controlling = l.is_controlling());
    controlling
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_motion(
    node: *const KvmNode,
    dx: i64,
    dy: i64,
    scroll_x: i64,
    scroll_y: i64,
) {
    with_link(node, |l| l.send_motion(dx, dy, scroll_x, scroll_y))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_key(node: *const KvmNode, code: u16, down: bool) {
    with_link(node, |l| l.send_key(code, down))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_button(node: *const KvmNode, button: u8, down: bool) {
    with_link(node, |l| l.send_button(button, down))
}

/// Asks the peer to stream its main display; frames arrive as KVM_EV_VIDEO_FRAME.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_video_request(
    node: *const KvmNode,
    max_width: u16,
    max_height: u16,
    fps: u8,
    bitrate_kbps: u32,
    prefer: u8,
    display: u32,
    fit_host: u8,
) {
    with_link(node, |l| {
        l.request_video(VideoParams { max_width, max_height, fps, bitrate_kbps, prefer, display, fit_host })
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_video_stop(node: *const KvmNode) {
    with_link(node, Link::stop_video)
}

/// Whether the link can take another frame now. Ask before encoding and skip the frame on
/// false: unlike a refused encoded frame, a skipped one needs no keyframe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_video_can_send(node: *const KvmNode) -> bool {
    let node = unsafe { &*node };
    let mut ok = false;
    if let Some(link) = node.link.lock().unwrap().as_ref() {
        ok = link.video_can_send();
    }
    ok
}

/// Sends an encoded frame of this screen while the peer views it. Copies `data`.
/// False if refused (not viewed, or the link is behind): make the next frame a keyframe.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_video_send_frame(node: *const KvmNode, data: *const u8, len: usize, keyframe: bool) -> bool {
    if data.is_null() || len == 0 {
        return false;
    }
    let payload = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let node = unsafe { &*node };
    let _guard = node.rt.enter();
    let mut sent = false;
    if let Some(link) = node.link.lock().unwrap().as_ref() {
        sent = link.send_video_frame(payload, keyframe);
    }
    sent
}

/// Shares clipboard contents (opaque to the core; see docs/protocol.md). Copies `data`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_clipboard(node: *const KvmNode, data: *const u8, len: usize) {
    if data.is_null() {
        return;
    }
    let payload = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    let node = unsafe { &*node };
    let _guard = node.rt.enter();
    with_link(node, |l| l.send_clipboard(payload))
}

/// Sends files and folders to the peer. Returns the transfer id carried by the
/// KVM_EV_FILE_SEND_* events, or 0 when not connected.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_files(node: *const KvmNode, paths: *const *const c_char, count: usize, clipboard: bool) -> u64 {
    if paths.is_null() || count == 0 {
        return 0;
    }
    let paths: Vec<std::path::PathBuf> = (0..count)
        .filter_map(|i| unsafe { str_arg(*paths.add(i)) }.map(std::path::PathBuf::from))
        .collect();
    let node = unsafe { &*node };
    let _guard = node.rt.enter();
    let mut transfer = 0;
    with_link(node, |l| transfer = l.send_files(paths, clipboard).unwrap_or(0));
    transfer
}

/// Where files copied on the peer are cached for pasting; one subfolder per transfer id.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_set_clipboard_dir(path: *const c_char) {
    if let Some(path) = unsafe { str_arg(path) } {
        kvm_core::set_clipboard_dir(path.into());
    }
}

/// Where received files are saved (default ~/Downloads).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_set_download_dir(path: *const c_char) {
    if let Some(path) = unsafe { str_arg(path) } {
        kvm_core::set_download_dir(path.into());
    }
}

/// While controlling: moves the peer's pointer to (x, y) on one of its displays (0: main),
/// as fractions of its size scaled to 0..=65535.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_pointer_abs(node: *const KvmNode, x: u16, y: u16, display: u32) {
    with_link(node, |l| l.pointer_abs(x, y, display))
}

/// While controlling: keeps the peer's pointer on one of its displays and asks it to report
/// edge exits (KVM_EV_EDGE_EXIT); 0xFFFFFFFF lifts the confinement.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_confine(node: *const KvmNode, display: u32) {
    with_link(node, |l| l.confine(display))
}

/// While controlled and confined: tells the controller the pointer was pushed past an edge.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_edge_exit(node: *const KvmNode, edge: u8, along: u16) {
    with_link(node, |l| l.edge_exit(edge, along))
}

/// While the peer views this screen: tells it which displays there are (JSON, see docs).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_display_list(node: *const KvmNode, data: *const u8, len: usize) {
    if data.is_null() {
        return;
    }
    let list = unsafe { std::slice::from_raw_parts(data, len) }.to_vec();
    with_link(node, |l| l.send_display_list(list))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_wheel(node: *const KvmNode, dx: i16, dy: i16) {
    with_link(node, |l| l.send_wheel(dx, dy))
}

/// While controlling: types `text` (UTF-8) on the peer, whatever its keyboard layout.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_text(node: *const KvmNode, text: *const c_char) {
    let Some(text) = (unsafe { str_arg(text) }) else { return };
    with_link(node, |l| l.send_text(text.to_string()))
}

/// While controlling: a pinch or rotation on the peer (kind 0 magnify, 1 rotate; phase 1
/// began, 2 changed, 4 ended; value: magnification x 10000 or degrees x 100, increments).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_gesture(node: *const KvmNode, kind: u8, phase: u8, value: i32) {
    with_link(node, |l| l.send_gesture(kind, phase, value))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_send_consumer(node: *const KvmNode, usage: u16, down: bool) {
    with_link(node, |l| l.send_consumer(usage, down))
}

/// Writes a line from the platform app into the shared log. `level`: 0 info, 1 warn, 2 error.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_log(level: u32, message: *const c_char) {
    let Some(message) = (unsafe { str_arg(message) }) else { return };
    match level {
        0 => tracing::info!(target: "app", "{message}"),
        1 => tracing::warn!(target: "app", "{message}"),
        _ => tracing::error!(target: "app", "{message}"),
    }
}

/// Stops everything and frees the node.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn kvm_node_free(node: *mut KvmNode) {
    if node.is_null() {
        return;
    }
    unsafe { kvm_node_clear_peer(node) };
    let node = unsafe { Box::from_raw(node) };
    if node.secondary {
        return;
    }
    if let Some(watcher) = node.watcher.lock().unwrap().take() {
        watcher.abort();
    }
    let KvmNode { rt, node, .. } = *node;
    rt.block_on(node.close());
    if let Ok(rt) = Arc::try_unwrap(rt) {
        rt.shutdown_timeout(Duration::from_secs(1));
    }
}
