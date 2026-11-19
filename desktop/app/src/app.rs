//! The app: the link to the paired computer, switching control, replaying the peer's input,
//! clipboard sharing, pairing and the tray menu. Mirrors the Mac app's Controller, and its
//! RemoteHost for iPhones and iPads, which control this computer on a link of their own.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

use kvm_core::{EndpointId, Event, Link, Node, NodeEvent};
use kvm_platform::{Delegate, Injector, Input, MenuEntry, capture, dialog, ui, updater};
use tokio::runtime::Handle;
use tracing::{info, warn};

use crate::clipboard;
use crate::config::Config;

/// How long "Allow Pairing" stays on.
const PAIRING_WINDOW: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Local,
    /// This machine's input goes to the peer.
    Controlling,
    /// The peer's input is replayed here.
    Controlled,
}

pub struct App {
    /// Itself, for callbacks that only get `&self` but start background work.
    me: Weak<App>,
    rt: Handle,
    node: Node,
    config_dir: PathBuf,
    config: Mutex<Config>,
    link: Mutex<Option<Arc<Link>>>,
    /// Sessions from paired remote controls, while there are any.
    remotes: Mutex<Option<Arc<Link>>>,
    state: Mutex<State>,
    injector: Injector,
    /// Clipboard text received from the peer, to set on the clipboard thread.
    clipboard_in: std::sync::mpsc::Sender<String>,
    clipboard_rx: Mutex<Option<std::sync::mpsc::Receiver<String>>>,
}

struct State {
    mode: Mode,
    connected: bool,
    stalled: bool,
    peer_name: Option<String>,
    latency_ms: Option<f64>,
    /// KVM machines seen on this network.
    discovered: BTreeMap<EndpointId, String>,
    pairing_until: Option<Instant>,
    /// Pairing requested by us and waiting for the other machine's answer.
    asking: Option<EndpointId>,
    /// A remote control (named) is controlling this computer.
    remote_controlling: Option<String>,
}

impl App {
    pub fn new(rt: Handle, node: Node, config_dir: PathBuf) -> Arc<App> {
        let config = Config::load(&config_dir);
        let (clipboard_in, clipboard_rx) = std::sync::mpsc::channel();
        Arc::new_cyclic(|me| App {
            me: me.clone(),
            rt,
            node,
            config_dir,
            state: Mutex::new(State {
                mode: Mode::Local,
                connected: false,
                stalled: false,
                peer_name: config.peer_name.clone(),
                latency_ms: None,
                discovered: BTreeMap::new(),
                pairing_until: None,
                asking: None,
                remote_controlling: None,
            }),
            config: Mutex::new(config),
            link: Mutex::new(None),
            remotes: Mutex::new(None),
            injector: Injector::default(),
            clipboard_in,
            clipboard_rx: Mutex::new(Some(clipboard_rx)),
        })
    }

    pub fn hotkey(&self) -> kvm_platform::Hotkey {
        self.config.lock().unwrap().hotkey.into()
    }

    /// Connects to the paired computer and starts the background work.
    pub fn start(self: &Arc<Self>) {
        if let Some(peer) = self.peer_id() {
            self.connect(peer);
        }
        self.listen_remotes();
        let app = self.clone();
        if let Some(mut events) = self.node.events() {
            self.rt.spawn(async move {
                while let Some(event) = events.recv().await {
                    app.node_event(event);
                }
            });
        }
        let app = self.clone();
        std::thread::Builder::new()
            .name("clipboard".into())
            .spawn(move || app.clipboard_loop())
            .expect("clipboard thread");
        let app = self.clone();
        self.rt.spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            loop {
                tick.tick().await;
                app.second();
            }
        });
        self.refresh();
    }

    fn peer_id(&self) -> Option<EndpointId> {
        self.config.lock().unwrap().peer_id.as_deref().and_then(|s| s.parse().ok())
    }

    fn connect(self: &Arc<Self>, peer: EndpointId) {
        let _rt = self.rt.enter();
        let (link, mut events) = self.node.link(peer);
        let link = Arc::new(link);
        *self.link.lock().unwrap() = Some(link);
        let app = self.clone();
        self.rt.spawn(async move {
            while let Some(event) = events.recv().await {
                app.event(event);
            }
        });
        info!("linking with {peer}");
    }

    /// Accepts the paired remote controls (replacing the previous listener), or stops.
    fn listen_remotes(self: &Arc<Self>) {
        let allowed: std::collections::HashSet<EndpointId> =
            self.config.lock().unwrap().remotes.keys().filter_map(|id| id.parse().ok()).collect();
        if allowed.is_empty() {
            *self.remotes.lock().unwrap() = None;
            self.release_remote();
            return;
        }
        let _rt = self.rt.enter();
        let (link, mut events) = self.node.accept_remotes(allowed);
        *self.remotes.lock().unwrap() = Some(Arc::new(link));
        let app = self.clone();
        self.rt.spawn(async move {
            while let Some(event) = events.recv().await {
                app.remote_event(event);
            }
        });
    }

    fn release_remote(&self) {
        let was = self.state.lock().unwrap().remote_controlling.take();
        if was.is_some() {
            self.injector.end();
            self.refresh();
        }
    }

    /// A remote control's session: it only ever controls this computer.
    fn remote_event(&self, event: Event) {
        let controlling = || self.state.lock().unwrap().remote_controlling.is_some();
        match event {
            Event::Connected { peer, name } => {
                info!("remote control {name} connected");
                let mut config = self.config.lock().unwrap();
                let id = peer.to_string();
                if config.remotes.get(&id).is_some_and(|n| *n != name) && !name.is_empty() {
                    config.remotes.insert(id, name);
                    config.save(&self.config_dir);
                }
            }
            Event::Entered => {
                let name = self.remote_name();
                info!("{name} took control");
                self.return_to_local(true);
                self.state.lock().unwrap().remote_controlling = Some(name);
                self.injector.begin();
                self.refresh();
            }
            Event::Left | Event::Stalled | Event::Disconnected { .. } | Event::TakenBack => self.release_remote(),
            Event::Move { dx, dy } if controlling() => self.injector.motion(dx, dy),
            Event::Scroll { dx, dy } if controlling() => self.injector.scroll(dx, dy),
            Event::Wheel { dx, dy } if controlling() => self.injector.wheel(dx, dy),
            Event::Key { code, down } if controlling() => self.injector.key(self.swapped(code), down),
            Event::Button { button, down } if controlling() => self.injector.button(button, down),
            Event::Consumer { usage, down } if controlling() => self.injector.consumer(usage, down),
            Event::Text(text) if controlling() => self.injector.text(&text),
            Event::PointerAbs { x, y, .. } if controlling() => self.injector.warp(x, y),
            Event::Clipboard(payload) => {
                if let Some(text) = clipboard::decode_text(&payload) {
                    let _ = self.clipboard_in.send(text);
                }
            }
            _ => {} // no screen to show yet
        }
    }

    /// The connected remote control's name.
    fn remote_name(&self) -> String {
        let peer = self.remotes.lock().unwrap().as_ref().and_then(|l| l.session()).map(|s| s.peer().to_string());
        let config = self.config.lock().unwrap();
        peer.and_then(|id| config.remotes.get(&id).cloned()).unwrap_or_else(|| "A remote control".into())
    }

    fn link(&self) -> Option<Arc<Link>> {
        self.link.lock().unwrap().clone()
    }

    fn mode(&self) -> Mode {
        self.state.lock().unwrap().mode
    }

    // MARK: Control

    fn toggle(&self) {
        match self.mode() {
            Mode::Local => self.control_peer(),
            Mode::Controlling => self.return_to_local(true),
            Mode::Controlled => {}
        }
    }

    fn control_peer(&self) {
        let Some(link) = self.link() else { return };
        if !link.is_connected() {
            warn!("can't control the peer: not connected");
            return;
        }
        {
            let mut s = self.state.lock().unwrap();
            if s.mode != Mode::Local {
                return;
            }
            s.mode = Mode::Controlling;
        }
        link.enter();
        capture::set_remote(true);
        info!("controlling the peer");
        self.refresh();
    }

    fn return_to_local(&self, notify_peer: bool) {
        {
            let mut s = self.state.lock().unwrap();
            if s.mode != Mode::Controlling {
                return;
            }
            s.mode = Mode::Local;
        }
        capture::set_remote(false);
        if notify_peer && let Some(link) = self.link() {
            link.leave();
        }
        info!("back to this computer");
        self.refresh();
    }

    fn release_control(&self) {
        let was = {
            let mut s = self.state.lock().unwrap();
            let was = s.mode == Mode::Controlled;
            if was {
                s.mode = Mode::Local;
            }
            was
        };
        if was {
            self.injector.end();
            self.refresh();
        }
    }

    /// The Mac's Command keys act as Ctrl here and the other way round, if wanted.
    fn swapped(&self, hid: u16) -> u16 {
        if !self.config.lock().unwrap().swap_ctrl_cmd {
            return hid;
        }
        match hid {
            0xE0 => 0xE3,
            0xE3 => 0xE0,
            0xE4 => 0xE7,
            0xE7 => 0xE4,
            other => other,
        }
    }

    // MARK: Core events

    fn event(&self, event: Event) {
        let controlled = || self.mode() == Mode::Controlled;
        match event {
            Event::Connected { name, .. } => {
                info!("connected to {name}");
                {
                    let mut s = self.state.lock().unwrap();
                    s.connected = true;
                    s.stalled = false;
                    s.peer_name = Some(name.clone());
                }
                let mut config = self.config.lock().unwrap();
                if config.peer_name.as_deref() != Some(&name) {
                    config.peer_name = Some(name);
                    config.save(&self.config_dir);
                }
                drop(config);
                self.refresh();
            }
            Event::Disconnected { reason } => {
                info!("disconnected: {reason}");
                self.return_to_local(false);
                self.release_control();
                {
                    let mut s = self.state.lock().unwrap();
                    s.connected = false;
                    s.latency_ms = None;
                }
                self.refresh();
            }
            Event::Stalled => {
                self.return_to_local(false);
                self.release_control();
                self.state.lock().unwrap().stalled = true;
                self.refresh();
            }
            Event::Recovered => {
                self.state.lock().unwrap().stalled = false;
                self.refresh();
            }
            Event::Latency(rtt) => {
                let ms = rtt.as_secs_f64() * 1000.0;
                let mut s = self.state.lock().unwrap();
                s.latency_ms = Some(s.latency_ms.map_or(ms, |e| e * 0.9 + ms * 0.1));
            }
            Event::Entered => {
                info!("the peer took control");
                self.return_to_local(true);
                self.state.lock().unwrap().mode = Mode::Controlled;
                self.injector.begin();
                self.refresh();
            }
            Event::Left => {
                info!("the peer released control");
                self.release_control();
            }
            Event::TakenBack => {
                info!("the peer took their computer back");
                self.return_to_local(false);
            }
            Event::Move { dx, dy } if controlled() => self.injector.motion(dx, dy),
            Event::Scroll { dx, dy } if controlled() => self.injector.scroll(dx, dy),
            Event::Wheel { dx, dy } if controlled() => self.injector.wheel(dx, dy),
            Event::Key { code, down } if controlled() => self.injector.key(self.swapped(code), down),
            Event::Button { button, down } if controlled() => self.injector.button(button, down),
            Event::Consumer { usage, down } if controlled() => self.injector.consumer(usage, down),
            Event::Text(text) if controlled() => self.injector.text(&text),
            Event::PointerAbs { x, y, .. } if controlled() => self.injector.warp(x, y),
            Event::Clipboard(payload) => {
                if self.config.lock().unwrap().share_clipboard
                    && let Some(text) = clipboard::decode_text(&payload)
                {
                    let _ = self.clipboard_in.send(text);
                }
            }
            Event::LockRequested => self.injector.lock_screen(),
            Event::VideoRequested(_) => warn!("the peer asked to view this screen: not supported here yet"),
            _ => {}
        }
    }

    fn node_event(self: &Arc<Self>, event: NodeEvent) {
        match event {
            NodeEvent::Discovered { id, name } => {
                self.state.lock().unwrap().discovered.insert(id, name);
                self.refresh();
            }
            NodeEvent::Expired { id } => {
                self.state.lock().unwrap().discovered.remove(&id);
                self.refresh();
            }
            NodeEvent::PairRequest { id, name, code, remote } => {
                if remote {
                    let app = self.clone();
                    std::thread::spawn(move || {
                        let text = format!(
                            "{name} wants to control this computer as a remote (mouse, keyboard).\n\n\
                             Check that it shows this code: {}\n\nAllow it?",
                            spaced(&code)
                        );
                        let accepted = dialog::ask("KVM", &text);
                        let _rt = app.rt.enter();
                        if app.node.respond_pairing(id, accepted) && accepted {
                            info!("remote control {name} paired");
                            let mut config = app.config.lock().unwrap();
                            config.remotes.insert(id.to_string(), name);
                            config.save(&app.config_dir);
                            drop(config);
                            app.set_pairing_mode(false);
                            app.listen_remotes();
                            app.refresh();
                        }
                    });
                    return;
                }
                let app = self.clone();
                std::thread::spawn(move || {
                    let text = format!(
                        "{name} wants to pair with this computer, to share mouse, keyboard and clipboard.\n\n\
                         Check that it shows this code: {}\n\nAllow it?",
                        spaced(&code)
                    );
                    let accepted = dialog::ask("KVM", &text);
                    let _rt = app.rt.enter();
                    if app.node.respond_pairing(id, accepted) && accepted {
                        app.paired(id, name);
                    }
                });
            }
            NodeEvent::PairResult { id, name, accepted, error } => {
                self.state.lock().unwrap().asking = None;
                if accepted {
                    self.paired(id, name);
                } else {
                    let text = match error {
                        Some(err) => format!("Couldn't pair: {err}"),
                        None => format!("{name} declined. Open KVM Settings on it and try again."),
                    };
                    std::thread::spawn(move || dialog::tell("KVM", &text));
                }
                self.refresh();
            }
        }
    }

    fn paired(self: &Arc<Self>, id: EndpointId, name: String) {
        info!("paired with {name} ({id})");
        {
            let mut config = self.config.lock().unwrap();
            config.peer_id = Some(id.to_string());
            config.peer_name = Some(name.clone());
            config.save(&self.config_dir);
        }
        self.state.lock().unwrap().peer_name = Some(name);
        self.set_pairing_mode(false);
        self.connect(id);
        self.refresh();
    }

    fn unpair(&self) {
        self.return_to_local(true);
        self.release_control();
        *self.link.lock().unwrap() = None; // dropping the link closes the session
        {
            let mut config = self.config.lock().unwrap();
            config.peer_id = None;
            config.peer_name = None;
            config.save(&self.config_dir);
        }
        let mut s = self.state.lock().unwrap();
        s.connected = false;
        s.peer_name = None;
        drop(s);
        self.refresh();
    }

    fn set_pairing_mode(&self, on: bool) {
        let _rt = self.rt.enter();
        self.node.set_discoverable(on);
        self.state.lock().unwrap().pairing_until = on.then(|| Instant::now() + PAIRING_WINDOW);
        self.refresh();
    }

    /// Once a second: pairing mode times out; the tooltip shows the latency.
    fn second(&self) {
        let expired = self.state.lock().unwrap().pairing_until.is_some_and(|t| Instant::now() >= t);
        if expired {
            self.set_pairing_mode(false);
        }
        ui::set_tooltip(&self.status());
    }

    // MARK: Clipboard

    /// Sends what is copied here while connected (polled: Windows' change notifications need
    /// a window), and sets what arrives from the peer.
    fn clipboard_loop(&self) {
        let Some(incoming) = self.clipboard_rx.lock().unwrap().take() else { return };
        let mut board = match kvm_platform::clipboard::Clipboard::new() {
            Ok(board) => board,
            Err(err) => {
                warn!("no clipboard: {err}");
                return;
            }
        };
        let mut last = board.text();
        loop {
            while let Ok(text) = incoming.try_recv() {
                board.set_text(&text);
                last = Some(text); // not sent back
            }
            std::thread::sleep(Duration::from_millis(300));
            let now = board.text();
            if now == last {
                continue;
            }
            last = now.clone();
            let share = self.config.lock().unwrap().share_clipboard;
            let connected = self.state.lock().unwrap().connected;
            if let (true, true, Some(text), Some(link)) = (share, connected, now, self.link()) {
                if !text.is_empty() {
                    link.send_clipboard(clipboard::encode_text(&text));
                }
            }
        }
    }

    // MARK: Tray

    fn status(&self) -> String {
        if self.peer_id().is_none() {
            return "KVM: not paired".into();
        }
        let s = self.state.lock().unwrap();
        if let Some(remote) = &s.remote_controlling {
            return format!("KVM: {remote} is controlling this computer");
        }
        let peer = s.peer_name.clone().unwrap_or_else(|| "the other computer".into());
        match (s.connected, s.stalled, s.mode) {
            (false, _, _) => format!("KVM: connecting to {peer}…"),
            (true, true, _) => format!("KVM: connection to {peer} lost, waiting…"),
            (true, false, Mode::Controlling) => format!("KVM: controlling {peer}"),
            (true, false, Mode::Controlled) => format!("KVM: {peer} is controlling this computer"),
            (true, false, Mode::Local) => match s.latency_ms {
                Some(ms) => format!("KVM: connected to {peer} ({ms:.0} ms)"),
                None => format!("KVM: connected to {peer}"),
            },
        }
    }

    /// Rebuilds the tray menu from the current state.
    fn refresh(&self) {
        let config = self.config.lock().unwrap().clone();
        let status = self.status();
        let hotkey = kvm_platform::Hotkey::from(config.hotkey).label();
        let s = self.state.lock().unwrap();
        let peer = s.peer_name.clone().unwrap_or_else(|| "the other computer".into());
        let mut menu = vec![MenuEntry::disabled(status.trim_start_matches("KVM: ").to_string())];
        if config.peer_id.is_some() {
            menu.push(match s.mode {
                Mode::Controlling => MenuEntry::item("control", format!("Return to This Computer\t{hotkey}")),
                Mode::Controlled => MenuEntry::disabled(format!("Controlled by {peer}")),
                Mode::Local => MenuEntry::Item {
                    id: "control".into(),
                    label: format!("Control {peer}\t{hotkey}"),
                    enabled: s.connected && !s.stalled,
                    checked: None,
                },
            });
        }
        menu.push(MenuEntry::Separator);

        let mut machines: Vec<MenuEntry> = s
            .discovered
            .iter()
            .filter(|(id, _)| Some(id.to_string()) != config.peer_id)
            .map(|(id, name)| {
                let asking = s.asking == Some(*id);
                MenuEntry::Item {
                    id: format!("pair:{id}"),
                    label: if asking { format!("{name} (waiting for it to allow…)") } else { name.clone() },
                    enabled: s.asking.is_none(),
                    checked: None,
                }
            })
            .collect();
        if machines.is_empty() {
            machines.push(MenuEntry::disabled("None found: open KVM Settings on the Mac"));
        }
        menu.push(MenuEntry::Submenu { label: "Pair With".into(), children: machines });
        // Where mDNS is blocked (guest and office networks): pair by ID, through the clipboard.
        menu.push(MenuEntry::Submenu {
            label: "Pair Over the Internet".into(),
            children: vec![
                MenuEntry::item("copy-id", "Copy This Computer's ID"),
                MenuEntry::Item {
                    id: "pair-clipboard".into(),
                    label: "Pair With the ID on the Clipboard".into(),
                    enabled: s.asking.is_none(),
                    checked: None,
                },
            ],
        });
        menu.push(MenuEntry::check("pairing", "Allow Pairing (2 minutes)", s.pairing_until.is_some()));
        if config.peer_id.is_some() {
            menu.push(MenuEntry::item("unpair", format!("Unpair {peer}")));
        }
        let mut remotes: Vec<MenuEntry> = config
            .remotes
            .iter()
            .map(|(id, name)| MenuEntry::item(&format!("remove-remote:{id}"), format!("Remove {name}")))
            .collect();
        if remotes.is_empty() {
            remotes.push(MenuEntry::disabled("None: Allow Pairing here, then on the iPhone or iPad"));
            remotes.push(MenuEntry::disabled("tap + > Enter Computer ID (Copy This Computer's ID)"));
        }
        menu.push(MenuEntry::Submenu { label: "iPhone and iPad".into(), children: remotes });
        drop(s);
        menu.push(MenuEntry::Separator);
        menu.push(MenuEntry::check("clipboard", "Share Clipboard", config.share_clipboard));
        menu.push(MenuEntry::check("swap", "Mac's Command Key Is Ctrl", config.swap_ctrl_cmd));
        if updater::available() {
            menu.push(MenuEntry::item("update", "Check for Updates…"));
        }
        menu.push(MenuEntry::item("log", "Show Log"));
        menu.push(MenuEntry::Separator);
        menu.push(MenuEntry::item("quit", "Quit KVM"));
        ui::set_menu(menu);
        ui::set_tooltip(&status);
    }

    pub fn shutdown(&self) {
        self.return_to_local(true);
        self.release_control();
        self.release_remote();
        updater::stop();
    }
}

impl Delegate for App {
    fn input(&self, input: Input) {
        if self.mode() != Mode::Controlling {
            return;
        }
        let Some(link) = self.link() else { return };
        match input {
            Input::Motion { dx, dy } => link.send_motion(dx, dy, 0, 0),
            Input::Scroll { dx, dy } => link.send_motion(0, 0, dx, dy),
            Input::Wheel { dx, dy } => link.send_wheel(dx, dy),
            Input::Key { hid, down } => link.send_key(self.swapped(hid), down),
            Input::Button { button, down } => link.send_button(button, down),
            Input::Consumer { usage, down } => link.send_consumer(usage, down),
        }
    }

    fn hotkey(&self) {
        self.toggle();
    }

    fn menu(&self, id: &str) {
        match id {
            "control" => self.toggle(),
            "pairing" => {
                let on = self.state.lock().unwrap().pairing_until.is_none();
                self.set_pairing_mode(on);
            }
            "unpair" => self.unpair(),
            "clipboard" | "swap" => {
                let mut config = self.config.lock().unwrap();
                if id == "clipboard" {
                    config.share_clipboard = !config.share_clipboard;
                } else {
                    config.swap_ctrl_cmd = !config.swap_ctrl_cmd;
                }
                config.save(&self.config_dir);
                drop(config);
                self.refresh();
            }
            "update" => updater::check_now(),
            "copy-id" => {
                let _ = self.clipboard_in.send(self.node.id().to_string());
            }
            "pair-clipboard" => {
                let text = kvm_platform::clipboard::Clipboard::new().ok().and_then(|mut c| c.text()).unwrap_or_default();
                let id = text.trim().to_lowercase();
                match id.parse::<EndpointId>() {
                    Ok(peer) if id.len() == 64 && peer != self.node.id() => {
                        let _rt = self.rt.enter();
                        self.state.lock().unwrap().asking = Some(peer);
                        self.node.request_pairing(peer, false);
                        self.refresh();
                        std::thread::spawn(|| {
                            dialog::tell("KVM", "Asked the other computer to pair. Allow it there (its KVM Settings must be open) and check that both show the same code.")
                        });
                    }
                    _ => {
                        std::thread::spawn(|| {
                            dialog::tell("KVM", "Copy the other computer's ID first: on a Mac, KVM Settings > Pair over the internet; here, Copy This Computer's ID.")
                        });
                    }
                }
            }
            "log" => open(&crate::config::log_dir().join("kvm.log")),
            "quit" => ui::quit(),
            _ if id.starts_with("remove-remote:") => {
                let removed = id.trim_start_matches("remove-remote:");
                let mut config = self.config.lock().unwrap();
                config.remotes.remove(removed);
                config.save(&self.config_dir);
                drop(config);
                if let Some(app) = self.me.upgrade() {
                    app.listen_remotes(); // drops a session it had
                }
                self.refresh();
            }
            _ => {
                if let Some(peer) = id.strip_prefix("pair:").and_then(|s| s.parse::<EndpointId>().ok()) {
                    let _rt = self.rt.enter();
                    self.state.lock().unwrap().asking = Some(peer);
                    self.node.request_pairing(peer, false);
                    self.refresh();
                }
            }
        }
    }
}

fn spaced(code: &str) -> String {
    if code.len() == 6 { format!("{} {}", &code[..3], &code[3..]) } else { code.to_string() }
}

/// Opens a file with its default app.
fn open(path: &std::path::Path) {
    let result = if cfg!(windows) {
        std::process::Command::new("explorer").arg(path).spawn()
    } else {
        std::process::Command::new("xdg-open").arg(path).spawn()
    };
    if let Err(err) = result {
        warn!("opening {}: {err}", path.display());
    }
}
