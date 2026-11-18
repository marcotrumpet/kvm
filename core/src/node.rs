//! Identity, endpoint setup, local discovery, pairing, and the self-healing link to the
//! paired peer.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use iroh::address_lookup::UserData;
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointId, SecretKey};
use iroh_mdns_address_lookup::{DiscoveryEvent, MdnsAddressLookup};
use n0_future::StreamExt;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::protocol::{self, ALPN, PAIR_ALPN, Pair};
use crate::session::{Event, Session};

const IDENTITY_FILE: &str = "identity.key";
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const RETRY_MIN: Duration = Duration::from_millis(250);
const RETRY_MAX: Duration = Duration::from_secs(5);
/// mDNS service name: only KVM machines discover each other.
const MDNS_SERVICE: &str = "kvm";
/// User data published with our addresses. In pairing mode it is this prefix plus the
/// machine name; otherwise just `HIDDEN_USER_DATA`, so nearby machines don't list us.
const USER_DATA_PREFIX: &str = "kvm1:";
const HIDDEN_USER_DATA: &str = "kvm1";
/// How long the other machine's user has to accept a pairing request.
const PAIR_DECISION_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_PENDING_PAIRS: usize = 3;

/// Machine-level events, independent of the paired session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeEvent {
    /// A KVM machine was seen (or updated) on the local network.
    Discovered { id: EndpointId, name: String },
    Expired { id: EndpointId },
    /// Someone asks to pair with this machine; answer with [`Node::respond_pairing`].
    PairRequest { id: EndpointId, name: String, code: String },
    /// The answer to our [`Node::request_pairing`].
    PairResult { id: EndpointId, name: String, accepted: bool, error: Option<String> },
}

/// This machine on the network: a persistent identity plus an iroh endpoint that is
/// reachable on the LAN (mDNS) and over the internet (hole punching, relay fallback).
#[derive(Clone)]
pub struct Node {
    endpoint: Endpoint,
    name: String,
    inner: Arc<Inner>,
}

struct Inner {
    /// Where incoming session connections from the paired peer go (accepting side only).
    session_conns: Mutex<Option<(EndpointId, mpsc::UnboundedSender<Connection>)>>,
    /// Pairing requests waiting for the local user's decision.
    pending_pairs: Mutex<HashMap<EndpointId, oneshot::Sender<bool>>>,
    events: mpsc::UnboundedSender<NodeEvent>,
    events_rx: Mutex<Option<mpsc::UnboundedReceiver<NodeEvent>>>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    /// Pairing mode: listed by nearby machines and accepting pairing requests.
    discoverable: std::sync::atomic::AtomicBool,
}

impl Node {
    pub async fn bind(state_dir: &Path, name: impl Into<String>) -> Result<Node> {
        let name = name.into();
        let secret_key = load_or_create_identity(state_dir)?;
        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .alpns(vec![ALPN.to_vec(), PAIR_ALPN.to_vec()])
            .user_data_for_address_lookup(user_data(None))
            .bind()
            .await
            .context("binding endpoint")?;
        let mdns = MdnsAddressLookup::builder()
            .service_name(MDNS_SERVICE)
            .build(endpoint.id())
            .context("starting mDNS")?;
        endpoint.address_lookup().context("address lookup unavailable")?.add(mdns.clone());

        let (events, events_rx) = mpsc::unbounded_channel();
        let node = Node {
            endpoint,
            name,
            inner: Arc::new(Inner {
                session_conns: Mutex::new(None),
                pending_pairs: Mutex::default(),
                events,
                events_rx: Mutex::new(Some(events_rx)),
                tasks: Mutex::default(),
                discoverable: std::sync::atomic::AtomicBool::new(false),
            }),
        };
        let tasks = vec![
            tokio::spawn(accept_loop(node.clone())),
            tokio::spawn(discovery_loop(node.clone(), mdns)),
        ];
        *node.inner.tasks.lock().unwrap() = tasks;
        Ok(node)
    }

    pub fn id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Pairing mode on or off (off by default). While on, nearby machines list this one by
    /// name and pairing requests reach the user; while off, requests are declined.
    pub fn set_discoverable(&self, on: bool) {
        use std::sync::atomic::Ordering;
        if self.inner.discoverable.swap(on, Ordering::AcqRel) != on {
            info!("pairing mode {}", if on { "on" } else { "off" });
            self.endpoint
                .set_user_data_for_address_lookup(Some(user_data(on.then_some(self.name.as_str()))));
        }
    }

    /// Machine-level events (discovery, pairing). Can be taken once.
    pub fn events(&self) -> Option<mpsc::UnboundedReceiver<NodeEvent>> {
        self.inner.events_rx.lock().unwrap().take()
    }

    /// Keeps a session with `peer` alive for as long as the returned [`Link`] exists.
    ///
    /// Exactly one side dials: the one with the smaller id. The other accepts, and a new
    /// incoming connection from the peer replaces the current one, so a restarted peer
    /// reconnects immediately instead of waiting for the old session to time out.
    pub fn link(&self, peer: EndpointId) -> (Link, mpsc::UnboundedReceiver<Event>) {
        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let current = Arc::new(Mutex::new(None));
        let dialer = self.id().as_bytes() < peer.as_bytes();
        let incoming = if dialer {
            *self.inner.session_conns.lock().unwrap() = None;
            None
        } else {
            let (tx, rx) = mpsc::unbounded_channel();
            *self.inner.session_conns.lock().unwrap() = Some((peer, tx));
            Some(rx)
        };
        let task = tokio::spawn(maintain(self.clone(), peer, incoming, current.clone(), events_tx));
        (Link { current, task, peer, node: self.inner.clone() }, events_rx)
    }

    /// Asks `peer` to pair. The outcome arrives as [`NodeEvent::PairResult`].
    pub fn request_pairing(&self, peer: EndpointId) {
        let node = self.clone();
        tokio::spawn(async move {
            let result = node.pair_with(peer).await;
            let event = match result {
                Ok((accepted, name)) => NodeEvent::PairResult { id: peer, name, accepted, error: None },
                Err(err) => NodeEvent::PairResult {
                    id: peer,
                    name: String::new(),
                    accepted: false,
                    error: Some(format!("{err:#}")),
                },
            };
            let _ = node.inner.events.send(event);
        });
    }

    /// Answers a [`NodeEvent::PairRequest`]. Returns false if it is no longer pending.
    pub fn respond_pairing(&self, peer: EndpointId, accept: bool) -> bool {
        let pending = self.inner.pending_pairs.lock().unwrap().remove(&peer);
        pending.is_some_and(|tx| tx.send(accept).is_ok())
    }

    pub async fn close(&self) {
        for task in self.inner.tasks.lock().unwrap().drain(..) {
            task.abort();
        }
        self.endpoint.close().await;
    }

    async fn pair_with(&self, peer: EndpointId) -> Result<(bool, String)> {
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, self.endpoint.connect(peer, PAIR_ALPN))
            .await
            .context("connecting timed out")?
            .context("connecting")?;
        let (mut send, mut recv) = conn.open_bi().await.context("opening pairing stream")?;
        send.write_all(&protocol::encode_msg(&Pair::Request { name: self.name.clone() }))
            .await
            .context("sending pairing request")?;
        let reply = tokio::time::timeout(PAIR_DECISION_TIMEOUT + Duration::from_secs(10), protocol::read_msg(&mut recv))
            .await
            .context("no answer in time")?
            .context("reading answer")?;
        conn.close(0u32.into(), b"done");
        match reply {
            Pair::Reply { accepted, name } => Ok((accepted, name)),
            other => bail!("unexpected pairing message {other:?}"),
        }
    }

    /// Hands an incoming session connection to the link, if it is from the paired peer.
    fn route_session(&self, conn: Connection) {
        let remote = conn.remote_id();
        let routed = match self.inner.session_conns.lock().unwrap().as_ref() {
            Some((peer, tx)) if *peer == remote => tx.send(conn.clone()).is_ok(),
            _ => false,
        };
        if !routed {
            warn!("refusing session from unpaired {}", remote.fmt_short());
            conn.close(2u32.into(), b"not paired");
        }
    }

    async fn handle_pair_request(&self, conn: Connection) {
        let peer = conn.remote_id();
        if let Err(err) = self.answer_pair_request(&conn).await {
            warn!("pairing request from {}: {err:#}", peer.fmt_short());
        }
        self.inner.pending_pairs.lock().unwrap().remove(&peer);
        conn.close(0u32.into(), b"done");
    }

    async fn answer_pair_request(&self, conn: &Connection) -> Result<()> {
        let peer = conn.remote_id();
        let (mut send, mut recv) = tokio::time::timeout(CONNECT_TIMEOUT, conn.accept_bi())
            .await
            .context("no request")??;
        let Pair::Request { name } = tokio::time::timeout(CONNECT_TIMEOUT, protocol::read_msg(&mut recv))
            .await
            .context("no request")??
        else {
            bail!("expected a pairing request");
        };
        if !self.inner.discoverable.load(std::sync::atomic::Ordering::Acquire) {
            // Not in pairing mode: decline without bothering the user.
            send.write_all(&protocol::encode_msg(&Pair::Reply { accepted: false, name: self.name.clone() }))
                .await
                .ok();
            send.finish().ok();
            let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
            info!("declined pairing request from {} (not in pairing mode)", peer.fmt_short());
            return Ok(());
        }
        let (tx, rx) = oneshot::channel();
        {
            let mut pending = self.inner.pending_pairs.lock().unwrap();
            if pending.contains_key(&peer) || pending.len() >= MAX_PENDING_PAIRS {
                bail!("too many pending requests");
            }
            pending.insert(peer, tx);
        }
        let code = protocol::pairing_code(self.id().as_bytes(), peer.as_bytes());
        let _ = self.inner.events.send(NodeEvent::PairRequest { id: peer, name, code });

        let accepted = tokio::select! {
            decision = tokio::time::timeout(PAIR_DECISION_TIMEOUT, rx) => {
                matches!(decision, Ok(Ok(true)))
            }
            _ = conn.closed() => bail!("requester went away"),
        };
        send.write_all(&protocol::encode_msg(&Pair::Reply { accepted, name: self.name.clone() }))
            .await
            .context("sending answer")?;
        send.finish().ok();
        // Give the answer time to arrive before closing.
        let _ = tokio::time::timeout(Duration::from_secs(3), conn.closed()).await;
        info!("pairing with {} {}", peer.fmt_short(), if accepted { "accepted" } else { "declined" });
        Ok(())
    }
}

/// With a name: discoverable. Without: present (so the paired peer finds our addresses)
/// but not listed by anyone.
fn user_data(name: Option<&str>) -> UserData {
    let hidden = || UserData::try_from(HIDDEN_USER_DATA.to_string()).unwrap();
    let Some(name) = name else { return hidden() };
    let mut s = format!("{USER_DATA_PREFIX}{name}");
    while s.len() > UserData::MAX_LENGTH {
        s.pop();
    }
    UserData::try_from(s).unwrap_or_else(|_| hidden())
}

async fn accept_loop(node: Node) {
    while let Some(incoming) = node.endpoint.accept().await {
        let node = node.clone();
        tokio::spawn(async move {
            let mut accepting = match incoming.accept() {
                Ok(accepting) => accepting,
                Err(err) => return warn!("incoming connection failed: {err:#}"),
            };
            let alpn = match accepting.alpn().await {
                Ok(alpn) => alpn,
                Err(err) => return warn!("incoming connection failed: {err:#}"),
            };
            let conn = match accepting.await {
                Ok(conn) => conn,
                Err(err) => return warn!("incoming connection failed: {err:#}"),
            };
            if alpn == ALPN {
                node.route_session(conn);
            } else if alpn == PAIR_ALPN {
                node.handle_pair_request(conn).await;
            }
        });
    }
}

/// Lists nearby machines that are in pairing mode; a machine leaving pairing mode (or the
/// network) is reported as expired.
async fn discovery_loop(node: Node, mdns: MdnsAddressLookup) {
    let mut listed = std::collections::HashSet::new();
    let mut events = mdns.subscribe().await;
    while let Some(event) = events.next().await {
        let event = match event {
            DiscoveryEvent::Discovered { endpoint_info, .. } => {
                let id = endpoint_info.endpoint_id;
                if id == node.id() {
                    continue;
                }
                let name = endpoint_info
                    .data
                    .user_data()
                    .and_then(|d| d.as_ref().strip_prefix(USER_DATA_PREFIX).map(str::to_string));
                match name {
                    Some(name) => {
                        listed.insert(id);
                        NodeEvent::Discovered { id, name }
                    }
                    None if listed.remove(&id) => NodeEvent::Expired { id },
                    None => continue,
                }
            }
            DiscoveryEvent::Expired { endpoint_id } => {
                if !listed.remove(&endpoint_id) {
                    continue;
                }
                NodeEvent::Expired { id: endpoint_id }
            }
            _ => continue,
        };
        let _ = node.inner.events.send(event);
    }
}

async fn maintain(
    node: Node,
    peer: EndpointId,
    mut incoming: Option<mpsc::UnboundedReceiver<Connection>>,
    current: Arc<Mutex<Option<Session>>>,
    events: mpsc::UnboundedSender<Event>,
) {
    let dialer = incoming.is_none();
    let mut backoff = RETRY_MIN;
    loop {
        let conn = match incoming.as_mut() {
            None => match tokio::time::timeout(CONNECT_TIMEOUT, node.endpoint.connect(peer, ALPN)).await {
                Ok(Ok(conn)) => conn,
                Ok(Err(err)) => {
                    warn!("connecting to {}: {err:#}", peer.fmt_short());
                    tokio::time::sleep(backoff).await;
                    backoff = (backoff * 2).min(RETRY_MAX);
                    continue;
                }
                Err(_) => {
                    warn!("connecting to {} timed out", peer.fmt_short());
                    continue;
                }
            },
            Some(rx) => match rx.recv().await {
                Some(conn) => conn,
                None => return,
            },
        };

        // Replace any previous session only after it has emitted its final events.
        let previous = current.lock().unwrap().take();
        if let Some(previous) = previous {
            previous.shutdown("replaced by new connection").await;
        }

        let session = match Session::establish(conn, dialer, &node.name, events.clone()).await {
            Ok(session) => session,
            Err(err) => {
                warn!("handshake with {} failed: {err:#}", peer.fmt_short());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(RETRY_MAX);
                continue;
            }
        };
        info!("session with {} established", peer.fmt_short());
        backoff = RETRY_MIN;
        *current.lock().unwrap() = Some(session.clone());
        session.announce();

        if dialer {
            session.finished().await;
            current.lock().unwrap().take();
            tokio::time::sleep(RETRY_MIN).await;
        }
        // The accepting side goes straight back to waiting; a dead session is cleaned up
        // by its own heartbeat, a live one is replaced by the next incoming connection.
    }
}

/// Handle to the self-healing session with the paired peer. Input methods are dropped
/// silently while disconnected.
pub struct Link {
    current: Arc<Mutex<Option<Session>>>,
    task: JoinHandle<()>,
    peer: EndpointId,
    node: Arc<Inner>,
}

impl Link {
    pub fn session(&self) -> Option<Session> {
        self.current.lock().unwrap().clone()
    }

    fn with(&self, f: impl FnOnce(&Session)) {
        if let Some(s) = self.current.lock().unwrap().as_ref() {
            f(s);
        }
    }

    pub fn is_connected(&self) -> bool {
        self.current.lock().unwrap().is_some()
    }

    pub fn is_controlling(&self) -> bool {
        self.current.lock().unwrap().as_ref().is_some_and(Session::is_controlling)
    }

    pub fn enter(&self) {
        self.with(Session::enter)
    }

    pub fn leave(&self) {
        self.with(Session::leave)
    }

    pub fn send_motion(&self, dx: i64, dy: i64, scroll_x: i64, scroll_y: i64) {
        self.with(|s| s.send_motion(dx, dy, scroll_x, scroll_y))
    }

    pub fn send_key(&self, code: u16, down: bool) {
        self.with(|s| s.send_key(code, down))
    }

    pub fn send_button(&self, button: u8, down: bool) {
        self.with(|s| s.send_button(button, down))
    }

    pub fn send_consumer(&self, usage: u16, down: bool) {
        self.with(|s| s.send_consumer(usage, down))
    }

    pub fn send_wheel(&self, dx: i16, dy: i16) {
        self.with(|s| s.send_wheel(dx, dy))
    }

    pub fn request_video(&self, params: crate::video::VideoParams) {
        self.with(|s| s.request_video(params))
    }

    pub fn stop_video(&self) {
        self.with(Session::stop_video)
    }

    pub fn video_can_send(&self) -> bool {
        let session = self.current.lock().unwrap().clone();
        session.is_some_and(|s| s.video_can_send())
    }

    /// False if there is no session or the frame was refused; see [`Session::send_video_frame`].
    pub fn send_video_frame(&self, payload: Vec<u8>, keyframe: bool) -> bool {
        let session = self.current.lock().unwrap().clone();
        session.is_some_and(|s| s.send_video_frame(payload, keyframe))
    }

    pub fn pointer_abs(&self, x: u16, y: u16, display: u32) {
        self.with(|s| s.pointer_abs(x, y, display))
    }

    pub fn confine(&self, display: u32) {
        self.with(|s| s.confine(display))
    }

    pub fn edge_exit(&self, edge: u8, along: u16) {
        self.with(|s| s.edge_exit(edge, along))
    }

    pub fn send_display_list(&self, list: Vec<u8>) {
        self.with(|s| s.send_display_list(list))
    }

    pub fn send_clipboard(&self, payload: Vec<u8>) {
        self.with(|s| s.send_clipboard(payload))
    }

    /// None while disconnected.
    pub fn send_files(&self, paths: Vec<std::path::PathBuf>, clipboard: bool) -> Option<u64> {
        let session = self.current.lock().unwrap().clone();
        session.map(|s| s.send_files(paths, clipboard))
    }

    pub fn ping(&self) {
        self.with(Session::ping)
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.task.abort();
        let mut conns = self.node.session_conns.lock().unwrap();
        if conns.as_ref().is_some_and(|(peer, _)| *peer == self.peer) {
            *conns = None;
        }
    }
}

fn load_or_create_identity(state_dir: &Path) -> Result<SecretKey> {
    let path = state_dir.join(IDENTITY_FILE);
    if let Ok(bytes) = std::fs::read(&path) {
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("{} is corrupt", path.display()))?;
        return Ok(SecretKey::from_bytes(&bytes));
    }
    std::fs::create_dir_all(state_dir)
        .with_context(|| format!("creating {}", state_dir.display()))?;
    let key = SecretKey::generate();
    write_private(&path, &key.to_bytes())?;
    Ok(key)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut opts, 0o600);
    let mut file = opts.open(path).with_context(|| format!("creating {}", path.display()))?;
    file.write_all(bytes)?;
    Ok(())
}
