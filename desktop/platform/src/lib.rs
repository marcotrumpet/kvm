//! The OS layer of the Windows and Linux (X11) app, kept free of the core so it can be
//! checked for every target from any machine. The app (`kvm-desktop`) implements
//! [`Delegate`] and drives [`Injector`], [`ui`] and [`clipboard`].

/// Input captured here while this machine controls the peer, in wire terms: USB HID
/// keyboard usages (page 0x07), consumer usages (page 0x0C), buttons 0 left, 1 right,
/// 2 middle, 3/4 back/forward.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Input {
    Motion { dx: i64, dy: i64 },
    /// Smooth scrolling (touchpads), in pixels; positive y scrolls up, positive x left.
    Scroll { dx: i64, dy: i64 },
    /// Mouse wheel notches, same directions.
    Wheel { dx: i16, dy: i16 },
    Key { hid: u16, down: bool },
    Button { button: u8, down: bool },
    Consumer { usage: u16, down: bool },
}

/// The switching shortcut: modifiers plus one key (a HID usage).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Hotkey {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub meta: bool,
    pub hid: u16,
}

impl Hotkey {
    /// Ctrl+Alt+K.
    pub const DEFAULT: Hotkey = Hotkey { ctrl: true, alt: true, shift: false, meta: false, hid: 0x0E };

    pub fn label(&self) -> String {
        let mut parts = Vec::new();
        for (on, name) in [(self.ctrl, "Ctrl"), (self.alt, "Alt"), (self.shift, "Shift"), (self.meta, "Win")] {
            if on {
                parts.push(name.to_string());
            }
        }
        parts.push(key_name(self.hid));
        parts.join("+")
    }
}

fn key_name(hid: u16) -> String {
    match hid {
        0x04..=0x1D => ((b'A' + (hid - 0x04) as u8) as char).to_string(),
        0x1E..=0x26 => ((b'1' + (hid - 0x1E) as u8) as char).to_string(),
        0x27 => "0".into(),
        0x3A..=0x45 => format!("F{}", hid - 0x39),
        0x2C => "Space".into(),
        _ => format!("key {hid:#04x}"),
    }
}

/// A tray menu entry. `id` comes back through [`Delegate::menu`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MenuEntry {
    Item { id: String, label: String, enabled: bool, checked: Option<bool> },
    Submenu { label: String, children: Vec<MenuEntry> },
    Separator,
}

impl MenuEntry {
    pub fn item(id: &str, label: impl Into<String>) -> Self {
        MenuEntry::Item { id: id.into(), label: label.into(), enabled: true, checked: None }
    }

    pub fn disabled(label: impl Into<String>) -> Self {
        MenuEntry::Item { id: String::new(), label: label.into(), enabled: false, checked: None }
    }

    pub fn check(id: &str, label: impl Into<String>, checked: bool) -> Self {
        MenuEntry::Item { id: id.into(), label: label.into(), enabled: true, checked: Some(checked) }
    }
}

/// The app, as the OS layer sees it. Called on the UI thread, except `input`, which comes
/// from the input hooks (keep it quick: the OS waits for it).
pub trait Delegate: Send + Sync + 'static {
    /// Captured input, while [`capture::set_remote`] is on.
    fn input(&self, input: Input);
    /// The switching shortcut was pressed (it never reaches either machine).
    fn hotkey(&self);
    fn menu(&self, id: &str);
    /// The user locked this computer's screen.
    fn screen_locked(&self) {}
}

pub mod video;

#[cfg(any(all(target_os = "linux", feature = "h264"), test))]
pub mod h264;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{Injector, capture, clipboard, dialog, screen, system_proxy, ui, updater, viewer};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "linux")]
pub use linux::{Injector, capture, clipboard, dialog, system_proxy, ui, updater};
#[cfg(all(target_os = "linux", feature = "h264"))]
pub use linux::screen;
#[cfg(all(target_os = "linux", not(feature = "h264")))]
pub use unsupported::screen;

/// Stand-ins: everything on macOS (it has its own app); the viewer on Linux (and screen
/// sharing without the h264 feature).
#[cfg(not(windows))]
mod unsupported;
#[cfg(target_os = "linux")]
pub use unsupported::viewer;
#[cfg(not(any(windows, target_os = "linux")))]
pub use unsupported::{Injector, capture, clipboard, dialog, screen, system_proxy, ui, updater, viewer};
