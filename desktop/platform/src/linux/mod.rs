//! X11: input through the core protocol and XTest, the tray as a StatusNotifierItem (ksni).
//! Wayland sessions aren't supported yet (no global input capture or injection there
//! without libei and portals).

mod keymap;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    ClientMessageEvent, ConnectionExt as _, CreateWindowAux, EventMask, GrabMode, ModMask, Window, WindowClass,
};
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::{Delegate, Hotkey, Input, MenuEntry};

struct X {
    conn: RustConnection,
    root: Window,
    /// Ours, unmapped: wakes the event loop to quit.
    window: Window,
    width: u16,
    height: u16,
}

static X: OnceLock<X> = OnceLock::new();
static REMOTE: AtomicBool = AtomicBool::new(false);
static QUIT: AtomicBool = AtomicBool::new(false);
static HOTKEY: Mutex<Hotkey> = Mutex::new(Hotkey::DEFAULT);

fn x() -> Option<&'static X> {
    if let Some(x) = X.get() {
        return Some(x);
    }
    let (conn, screen) = RustConnection::connect(None).ok()?;
    let s = &conn.setup().roots[screen];
    let (root, width, height) = (s.root, s.width_in_pixels, s.height_in_pixels);
    let window = conn.generate_id().ok()?;
    conn.create_window(0, window, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &CreateWindowAux::new()).ok()?;
    conn.flush().ok()?;
    let _ = X.set(X { conn, root, window, width, height });
    X.get()
}

const KEY_PRESS: u8 = 2;
const KEY_RELEASE: u8 = 3;
const BUTTON_PRESS: u8 = 4;
const BUTTON_RELEASE: u8 = 5;
const MOTION_NOTIFY: u8 = 6;

fn center(x: &X) -> (i16, i16) {
    ((x.width / 2) as i16, (x.height / 2) as i16)
}

/// The modifiers as X reports them in a key event's state.
fn hotkey_mask(h: &Hotkey) -> u16 {
    let mut m = 0u16;
    if h.shift { m |= u16::from(ModMask::SHIFT) }
    if h.ctrl { m |= u16::from(ModMask::CONTROL) }
    if h.alt { m |= u16::from(ModMask::M1) }
    if h.meta { m |= u16::from(ModMask::M4) }
    m
}

const RELEVANT: u16 = 1 | 4 | 8 | 64; // Shift, Control, Mod1 (Alt), Mod4 (Super)

pub mod capture {
    use super::*;

    pub fn set_hotkey(hotkey: Hotkey) {
        *HOTKEY.lock().unwrap() = hotkey;
    }

    /// Grabs keyboard and pointer for the peer (or lets go). The pointer is parked in the
    /// middle of the screen meanwhile; motion is how far it moved from there.
    pub fn set_remote(on: bool) {
        let Some(x) = x() else { return };
        REMOTE.store(on, Ordering::Release);
        let c = &x.conn;
        if on {
            let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION;
            let _ = c.grab_keyboard(false, x.root, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC);
            let _ = c.grab_pointer(false, x.root, mask, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME);
            let (cx, cy) = center(x);
            let _ = c.warp_pointer(x11rb::NONE, x.root, 0, 0, 0, 0, cx, cy);
        } else {
            let _ = c.ungrab_keyboard(x11rb::CURRENT_TIME);
            let _ = c.ungrab_pointer(x11rb::CURRENT_TIME);
        }
        let _ = c.flush();
    }

    pub fn is_remote() -> bool {
        REMOTE.load(Ordering::Acquire)
    }
}

/// Replays the peer's input with XTest: absolute pointer positions, keys by keycode.
#[derive(Default)]
pub struct Injector {
    pos: Mutex<(f64, f64)>,
}

impl Injector {
    fn fake(&self, kind: u8, detail: u8, x_: i16, y_: i16) {
        if let Some(x) = x() {
            let _ = x.conn.xtest_fake_input(kind, detail, x11rb::CURRENT_TIME, x.root, x_, y_, 0);
            let _ = x.conn.flush();
        }
    }

    pub fn begin(&self) {
        let Some(x) = x() else { return };
        if let Ok(Ok(p)) = x.conn.query_pointer(x.root).map(|c| c.reply()) {
            *self.pos.lock().unwrap() = (p.root_x as f64, p.root_y as f64);
        }
    }

    pub fn end(&self) {}

    pub fn motion(&self, dx: i64, dy: i64) {
        let Some(x) = x() else { return };
        let (px, py) = {
            let mut p = self.pos.lock().unwrap();
            p.0 = (p.0 + dx as f64).clamp(0.0, x.width as f64 - 1.0);
            p.1 = (p.1 + dy as f64).clamp(0.0, x.height as f64 - 1.0);
            *p
        };
        self.fake(MOTION_NOTIFY, 0, px as i16, py as i16);
    }

    pub fn warp(&self, fx: u16, fy: u16) {
        let Some(x) = x() else { return };
        let (px, py) = (fx as f64 / 65535.0 * (x.width as f64 - 1.0), fy as f64 / 65535.0 * (x.height as f64 - 1.0));
        *self.pos.lock().unwrap() = (px, py);
        self.fake(MOTION_NOTIFY, 0, px as i16, py as i16);
    }

    pub fn button(&self, button: u8, down: bool) {
        // Wire: 0 left, 1 right, 2 middle, 3 back, 4 forward. X: 1 left, 2 middle, 3 right, 8, 9.
        let b = match button {
            0 => 1,
            1 => 3,
            2 => 2,
            3 => 8,
            4 => 9,
            _ => return,
        };
        self.fake(if down { BUTTON_PRESS } else { BUTTON_RELEASE }, b, 0, 0);
    }

    fn click(&self, b: u8, times: u32) {
        for _ in 0..times.min(50) {
            self.fake(BUTTON_PRESS, b, 0, 0);
            self.fake(BUTTON_RELEASE, b, 0, 0);
        }
    }

    /// Notches; up and left positive. X: 4 up, 5 down, 6 left, 7 right.
    pub fn wheel(&self, dx: i16, dy: i16) {
        self.click(if dy > 0 { 4 } else { 5 }, dy.unsigned_abs() as u32);
        self.click(if dx > 0 { 6 } else { 7 }, dx.unsigned_abs() as u32);
    }

    /// Pixels, as notches (about ten pixels each); remainders carry over.
    pub fn scroll(&self, dx: i64, dy: i64) {
        static REST: Mutex<(i64, i64)> = Mutex::new((0, 0));
        let (nx, ny) = {
            let mut r = REST.lock().unwrap();
            r.0 += dx;
            r.1 += dy;
            let n = (r.0 / 10, r.1 / 10);
            r.0 -= n.0 * 10;
            r.1 -= n.1 * 10;
            n
        };
        self.wheel(nx as i16, ny as i16);
    }

    pub fn key(&self, hid: u16, down: bool) {
        if let Some(code) = keymap::evdev(hid) {
            self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, code + 8, 0, 0);
        }
    }

    pub fn consumer(&self, usage: u16, down: bool) {
        if let Some(code) = keymap::media_evdev(usage) {
            self.fake(if down { KEY_PRESS } else { KEY_RELEASE }, code + 8, 0, 0);
        }
    }

    /// Typing arbitrary text needs a keymap per character; not done yet.
    pub fn text(&self, _text: &str) {
        tracing::debug!("typing text isn't supported on X11 yet");
    }

    pub fn lock_screen(&self) {
        let _ = std::process::Command::new("loginctl").arg("lock-session").spawn();
    }
}

pub mod ui {
    use super::*;
    use ksni::blocking::TrayMethods;

    static TRAY: Mutex<Option<ksni::blocking::Handle<Tray>>> = Mutex::new(None);

    struct Tray {
        delegate: Arc<dyn Delegate>,
        menu: Vec<MenuEntry>,
        tooltip: String,
    }

    impl ksni::Tray for Tray {
        fn id(&self) -> String {
            "kvm".into()
        }

        fn title(&self) -> String {
            "KVM".into()
        }

        fn icon_pixmap(&self) -> Vec<ksni::Icon> {
            vec![icon()]
        }

        fn tool_tip(&self) -> ksni::ToolTip {
            ksni::ToolTip { title: self.tooltip.clone(), ..Default::default() }
        }

        fn menu(&self) -> Vec<ksni::MenuItem<Self>> {
            self.menu.iter().map(build).collect()
        }
    }

    fn build(entry: &MenuEntry) -> ksni::MenuItem<Tray> {
        use ksni::menu::{CheckmarkItem, StandardItem, SubMenu};
        match entry {
            MenuEntry::Item { id, label, enabled, checked } => {
                let id = id.clone();
                let label = label.split('\t').next().unwrap_or_default().to_string();
                let activate = Box::new(move |t: &mut Tray| t.delegate.menu(&id));
                match checked {
                    Some(checked) => {
                        CheckmarkItem { label, enabled: *enabled, checked: *checked, activate, ..Default::default() }.into()
                    }
                    None => StandardItem { label, enabled: *enabled, activate, ..Default::default() }.into(),
                }
            }
            MenuEntry::Submenu { label, children } => SubMenu {
                label: label.clone(),
                enabled: !children.is_empty(),
                submenu: children.iter().map(build).collect(),
                ..Default::default()
            }
            .into(),
            MenuEntry::Separator => ksni::MenuItem::Separator,
        }
    }

    pub fn set_menu(entries: Vec<MenuEntry>) {
        if let Some(handle) = TRAY.lock().unwrap().as_ref() {
            handle.update(|t| t.menu = entries);
        }
    }

    pub fn set_tooltip(text: &str) {
        if let Some(handle) = TRAY.lock().unwrap().as_ref() {
            let text = text.to_string();
            handle.update(|t| t.tooltip = text);
        }
    }

    pub fn quit() {
        QUIT.store(true, Ordering::Release);
        if let Some(x) = X.get() {
            let event = ClientMessageEvent::new(32, x.window, x11rb::NONE, [0u32; 5]);
            let _ = x.conn.send_event(false, x.window, EventMask::NO_EVENT, event);
            let _ = x.conn.flush();
        }
    }

    /// Runs the X event loop on this thread until [`quit`].
    pub fn run(delegate: Arc<dyn Delegate>, hotkey: Hotkey) -> anyhow::Result<()> {
        let x = x().ok_or_else(|| anyhow::anyhow!("no X11 display (Wayland sessions aren't supported yet)"))?;
        capture::set_hotkey(hotkey);
        grab_hotkey(x, &hotkey);
        let tray = Tray { delegate: delegate.clone(), menu: vec![], tooltip: "KVM".into() };
        match tray.spawn() {
            Ok(handle) => *TRAY.lock().unwrap() = Some(handle),
            Err(err) => tracing::warn!("no tray (is a StatusNotifierItem host running?): {err}"),
        }
        while !QUIT.load(Ordering::Acquire) {
            let event = x.conn.wait_for_event()?;
            handle(x, &delegate, event);
        }
        if let Some(handle) = TRAY.lock().unwrap().take() {
            handle.shutdown().wait();
        }
        Ok(())
    }

    /// A passive grab of the shortcut, also with Caps Lock and Num Lock on.
    fn grab_hotkey(x: &X, hotkey: &Hotkey) {
        let Some(code) = keymap::evdev(hotkey.hid) else { return };
        let mask = hotkey_mask(hotkey);
        for extra in [0u16, 2, 16, 18] {
            let modifiers = ModMask::from(mask | extra);
            let _ = x.conn.grab_key(true, x.root, modifiers, code + 8, GrabMode::ASYNC, GrabMode::ASYNC);
        }
        let _ = x.conn.flush();
    }

    fn handle(x: &X, delegate: &Arc<dyn Delegate>, event: Event) {
        let remote = capture::is_remote();
        let hotkey = *HOTKEY.lock().unwrap();
        let send = |input: Input| delegate.input(input);
        match event {
            Event::KeyPress(e) | Event::KeyRelease(e) => {
                let down = matches!(event, Event::KeyPress(_));
                let code = e.detail.wrapping_sub(8);
                let hid = keymap::hid(code);
                let is_hotkey = hid == Some(hotkey.hid) && (u16::from(e.state) & RELEVANT) == hotkey_mask(&hotkey);
                if is_hotkey {
                    if down {
                        delegate.hotkey();
                    }
                    return;
                }
                if !remote {
                    return;
                }
                if let Some(usage) = keymap::media_usage(code) {
                    send(Input::Consumer { usage, down });
                } else if let Some(hid) = hid {
                    send(Input::Key { hid, down });
                }
            }
            Event::ButtonPress(e) | Event::ButtonRelease(e) if remote => {
                let down = matches!(event, Event::ButtonPress(_));
                let input = match (e.detail, down) {
                    (1, _) => Input::Button { button: 0, down },
                    (2, _) => Input::Button { button: 2, down },
                    (3, _) => Input::Button { button: 1, down },
                    (8, _) => Input::Button { button: 3, down },
                    (9, _) => Input::Button { button: 4, down },
                    (4, true) => Input::Wheel { dx: 0, dy: 1 },
                    (5, true) => Input::Wheel { dx: 0, dy: -1 },
                    (6, true) => Input::Wheel { dx: 1, dy: 0 },
                    (7, true) => Input::Wheel { dx: -1, dy: 0 },
                    _ => return,
                };
                send(input);
            }
            Event::MotionNotify(e) if remote => {
                let (cx, cy) = center(x);
                let (dx, dy) = ((e.root_x - cx) as i64, (e.root_y - cy) as i64);
                if dx != 0 || dy != 0 {
                    send(Input::Motion { dx, dy });
                    let _ = x.conn.warp_pointer(x11rb::NONE, x.root, 0, 0, 0, 0, cx, cy);
                    let _ = x.conn.flush();
                }
            }
            _ => {}
        }
    }

    /// The app icon's look at 32x32, as ARGB32 in network byte order.
    fn icon() -> ksni::Icon {
        const N: i32 = 32;
        let mut data = Vec::with_capacity((N * N * 4) as usize);
        for py in 0..N {
            for px in 0..N {
                let (fx, fy) = (px as f32 + 0.5, py as f32 + 0.5);
                let inside = |l: f32, t: f32, r: f32, b: f32| fx >= l && fx <= r && fy >= t && fy <= b;
                let frame = |l: f32, t: f32, r: f32, b: f32| inside(l, t, r, b) && !inside(l + 2.0, t + 2.0, r - 2.0, b - 2.0);
                let corner = (fx.min(N as f32 - fx).min(7.0) - 7.0).powi(2) + (fy.min(N as f32 - fy).min(7.0) - 7.0).powi(2);
                if corner > 49.0 {
                    data.extend_from_slice(&[0, 0, 0, 0]);
                    continue;
                }
                let t = (fx + fy) / (2.0 * N as f32);
                let (r, g, b) = (255.0 - 71.0 * t, 181.0 - 122.0 * t, 71.0 + 143.0 * t);
                let front = frame(13.0, 12.0, 28.0, 23.0) || inside(19.5, 23.0, 21.5, 26.0) || inside(16.0, 25.5, 25.0, 27.5);
                let back = !inside(11.0, 10.0, 30.0, 25.0) && frame(4.0, 5.0, 19.0, 16.0);
                let px = if front || back { [255, 255, 255, 255] } else { [255, r as u8, g as u8, b as u8] };
                data.extend_from_slice(&px);
            }
        }
        ksni::Icon { width: N, height: N, data }
    }
}

/// GNOME's manual proxy setting (Settings > Network > Proxy), as a URL for HTTPS_PROXY.
/// Other desktops usually export HTTPS_PROXY themselves.
pub fn system_proxy() -> Option<String> {
    let get = |schema: &str, key: &str| -> Option<String> {
        let out = std::process::Command::new("gsettings").args(["get", schema, key]).output().ok()?;
        Some(String::from_utf8_lossy(&out.stdout).trim().trim_matches('\'').to_string())
    };
    if get("org.gnome.system.proxy", "mode")? != "manual" {
        return None;
    }
    let host = get("org.gnome.system.proxy.https", "host").filter(|h| !h.is_empty())?;
    let port = get("org.gnome.system.proxy.https", "port").and_then(|p| p.parse::<u16>().ok()).unwrap_or(8080);
    Some(format!("http://{host}:{port}"))
}

/// Text on the clipboard. Use from one thread (arboard serves pastes from its own).
pub mod clipboard {
    pub struct Clipboard(arboard::Clipboard);

    impl Clipboard {
        pub fn new() -> anyhow::Result<Self> {
            Ok(Clipboard(arboard::Clipboard::new()?))
        }

        pub fn text(&mut self) -> Option<String> {
            self.0.get_text().ok()
        }

        pub fn set_text(&mut self, text: &str) {
            if let Err(err) = self.0.set_text(text) {
                tracing::warn!("setting the clipboard: {err}");
            }
        }
    }
}

/// Questions and notices through zenity or kdialog, whichever is installed. Blocking.
pub mod dialog {
    use std::process::Command;

    pub fn ask(title: &str, text: &str) -> bool {
        let zenity = Command::new("zenity").args(["--question", "--title", title, "--text", text]).status();
        match zenity {
            Ok(status) => status.success(),
            Err(_) => Command::new("kdialog")
                .args(["--title", title, "--yesno", text])
                .status()
                .is_ok_and(|s| s.success()),
        }
    }

    pub fn tell(title: &str, text: &str) {
        if Command::new("zenity").args(["--info", "--title", title, "--text", text]).status().is_err() {
            let _ = Command::new("notify-send").args([title, text]).status();
        }
    }
}

/// Linux has no installer to update yet: a daily check of the release feed that says when a
/// newer version is out and opens the download page.
pub mod updater {
    use std::process::Command;
    use std::sync::OnceLock;
    use std::time::Duration;

    static FEED: OnceLock<(String, String)> = OnceLock::new();

    pub fn start(feed_url: &str, _public_key: &str, version: &str, _on_quit: extern "C" fn()) -> bool {
        let _ = FEED.set((feed_url.to_string(), version.to_string()));
        std::thread::spawn(|| {
            loop {
                check(false);
                std::thread::sleep(Duration::from_secs(24 * 3600));
            }
        });
        true
    }

    pub fn available() -> bool {
        FEED.get().is_some()
    }

    pub fn check_now() {
        std::thread::spawn(|| check(true));
    }

    pub fn stop() {}

    fn check(tell_if_current: bool) {
        let Some((feed, current)) = FEED.get() else { return };
        let Ok(out) = Command::new("curl").args(["-fsSL", "--max-time", "20", feed]).output() else { return };
        let xml = String::from_utf8_lossy(&out.stdout);
        let Some(latest) = xml.split("<sparkle:version>").nth(1).and_then(|s| s.split('<').next()) else { return };
        if newer(latest, current) {
            let text = format!("KVM {latest} is available (you have {current}). Open the download page?");
            if super::dialog::ask("KVM", &text) {
                let _ = Command::new("xdg-open").arg("https://github.com/marcotrumpet/kvm/releases/latest").spawn();
            }
        } else if tell_if_current {
            super::dialog::tell("KVM", &format!("KVM {current} is the latest version."));
        }
    }

    fn newer(a: &str, b: &str) -> bool {
        let parse = |v: &str| v.split('.').map(|p| p.parse::<u32>().unwrap_or(0)).collect::<Vec<_>>();
        parse(a) > parse(b)
    }
}
