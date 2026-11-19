//! Replays the peer's input with SendInput, tagged so our own hooks ignore it. The pointer
//! moves by absolute positions on the virtual desktop (all monitors), so Windows' pointer
//! acceleration isn't applied on top of the peer's; keys go as scancodes.

use std::collections::HashSet;
use std::sync::Mutex;

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Shutdown::LockWorkStation;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBD_EVENT_FLAGS, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, KEYEVENTF_UNICODE, MOUSE_EVENT_FLAGS, MOUSEEVENTF_ABSOLUTE,
    MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN, MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP,
    MOUSEEVENTF_MOVE, MOUSEEVENTF_RIGHTDOWN, MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_VIRTUALDESK, MOUSEEVENTF_WHEEL,
    MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT, SendInput, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetSystemMetrics, SM_CXSCREEN, SM_CXVIRTUALSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use super::{MARKER, keymap};

#[derive(Default)]
pub struct Injector {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    x: f64,
    y: f64,
    keys: HashSet<u16>,
    buttons: HashSet<u8>,
}

struct Desktop {
    left: f64,
    top: f64,
    width: f64,
    height: f64,
}

fn desktop() -> Desktop {
    unsafe {
        Desktop {
            left: GetSystemMetrics(SM_XVIRTUALSCREEN) as f64,
            top: GetSystemMetrics(SM_YVIRTUALSCREEN) as f64,
            width: GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1) as f64,
            height: GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1) as f64,
        }
    }
}

fn mouse(dx: i32, dy: i32, data: i32, flags: MOUSE_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT { dx, dy, mouseData: data as _, dwFlags: flags, time: 0, dwExtraInfo: MARKER },
        },
    }
}

fn keyboard(vk: u16, scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT { wVk: VIRTUAL_KEY(vk), wScan: scan, dwFlags: flags, time: 0, dwExtraInfo: MARKER },
        },
    }
}

fn send(inputs: &[INPUT]) {
    unsafe { SendInput(inputs, std::mem::size_of::<INPUT>() as i32) };
}

impl Injector {
    /// Control begins: start from where the pointer is.
    pub fn begin(&self) {
        let mut p = POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        let mut s = self.state.lock().unwrap();
        s.x = p.x as f64;
        s.y = p.y as f64;
    }

    /// Control ends: release anything still held (the core already does; a second line of
    /// defense so nothing stays stuck).
    pub fn end(&self) {
        let (keys, buttons) = {
            let mut s = self.state.lock().unwrap();
            (std::mem::take(&mut s.keys), std::mem::take(&mut s.buttons))
        };
        for hid in keys {
            self.key(hid, false);
        }
        for b in buttons {
            self.button(b, false);
        }
    }

    pub fn motion(&self, dx: i64, dy: i64) {
        let d = desktop();
        let (x, y) = {
            let mut s = self.state.lock().unwrap();
            s.x = (s.x + dx as f64).clamp(d.left, d.left + d.width - 1.0);
            s.y = (s.y + dy as f64).clamp(d.top, d.top + d.height - 1.0);
            (s.x, s.y)
        };
        self.move_to(&d, x, y);
    }

    fn move_to(&self, d: &Desktop, x: f64, y: f64) {
        let nx = ((x - d.left) * 65535.0 / (d.width - 1.0).max(1.0)).round() as i32;
        let ny = ((y - d.top) * 65535.0 / (d.height - 1.0).max(1.0)).round() as i32;
        send(&[mouse(nx, ny, 0, MOUSEEVENTF_MOVE | MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK)]);
    }

    /// To a point on a display (an id from the display list; 0 the main one), as fractions
    /// 0..=65535: where the viewer was clicked.
    pub fn warp(&self, fx: u16, fy: u16, display: u32) {
        let (left, top, w, h) = match super::screen::display(display) {
            Some(d) => (d.left as f64, d.top as f64, d.width as f64, d.height as f64),
            None => unsafe { (0.0, 0.0, GetSystemMetrics(SM_CXSCREEN) as f64, GetSystemMetrics(SM_CYSCREEN) as f64) },
        };
        let (x, y) = (left + fx as f64 / 65535.0 * (w - 1.0), top + fy as f64 / 65535.0 * (h - 1.0));
        {
            let mut s = self.state.lock().unwrap();
            s.x = x;
            s.y = y;
        }
        self.move_to(&desktop(), x, y);
    }

    pub fn button(&self, button: u8, down: bool) {
        let (flags, data) = match (button, down) {
            (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
            (0, false) => (MOUSEEVENTF_LEFTUP, 0),
            (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
            (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
            (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
            (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
            (3 | 4, true) => (MOUSEEVENTF_XDOWN, button as i32 - 2),
            (3 | 4, false) => (MOUSEEVENTF_XUP, button as i32 - 2),
            _ => return,
        };
        {
            let mut s = self.state.lock().unwrap();
            if down { s.buttons.insert(button) } else { s.buttons.remove(&button) };
        }
        send(&[mouse(0, 0, data, flags)]);
    }

    /// Wheel notches; up and left positive (Windows' horizontal axis is the other way).
    pub fn wheel(&self, dx: i16, dy: i16) {
        if dy != 0 {
            send(&[mouse(0, 0, dy as i32 * 120, MOUSEEVENTF_WHEEL)]);
        }
        if dx != 0 {
            send(&[mouse(0, 0, -(dx as i32) * 120, MOUSEEVENTF_HWHEEL)]);
        }
    }

    /// Smooth scrolling in pixels, as fine-grained wheel deltas (12 per pixel).
    pub fn scroll(&self, dx: i64, dy: i64) {
        if dy != 0 {
            send(&[mouse(0, 0, (dy * 12).clamp(-12000, 12000) as i32, MOUSEEVENTF_WHEEL)]);
        }
        if dx != 0 {
            send(&[mouse(0, 0, (-dx * 12).clamp(-12000, 12000) as i32, MOUSEEVENTF_HWHEEL)]);
        }
    }

    pub fn key(&self, hid: u16, down: bool) {
        let up = if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP };
        if let Some(vk) = keymap::keyboard_volume_vk(hid) {
            send(&[keyboard(vk, 0, up)]);
            return;
        }
        let Some((scan, extended)) = keymap::scancode(hid) else { return };
        {
            let mut s = self.state.lock().unwrap();
            if down { s.keys.insert(hid) } else { s.keys.remove(&hid) };
        }
        let mut flags = KEYEVENTF_SCANCODE | up;
        if extended {
            flags |= KEYEVENTF_EXTENDEDKEY;
        }
        send(&[keyboard(0, scan, flags)]);
    }

    pub fn consumer(&self, usage: u16, down: bool) {
        if let Some(vk) = keymap::media_vk(usage) {
            send(&[keyboard(vk, 0, if down { KEYBD_EVENT_FLAGS(0) } else { KEYEVENTF_KEYUP })]);
        }
    }

    /// Types text as-is, whatever the keyboard layout.
    pub fn text(&self, text: &str) {
        let mut inputs = Vec::new();
        for unit in text.encode_utf16() {
            inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE));
            inputs.push(keyboard(0, unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
        if !inputs.is_empty() {
            send(&inputs);
        }
    }

    pub fn lock_screen(&self) {
        let _ = unsafe { LockWorkStation() };
    }
}
