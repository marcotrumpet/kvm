//! Low-level keyboard and mouse hooks, installed on the UI thread by [`super::ui::run`].
//! Always: the switching shortcut. While remote (controlling the peer): every key, button,
//! wheel and motion goes to the delegate and is kept from Windows. The pointer is parked
//! in the middle of the main screen meanwhile; motion is how far each event would have
//! moved it from there, and a cursorless window under it hides it ([`super::ui`]).

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use windows::Win32::Foundation::{LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetSystemMetrics, HHOOK, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, LLMHF_INJECTED,
    MSLLHOOKSTRUCT, SM_CXSCREEN, SM_CYSCREEN, SetCursorPos, SetWindowsHookExW, UnhookWindowsHookEx, WH_KEYBOARD_LL,
    WH_MOUSE_LL, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL,
    WM_MOUSEMOVE, WM_MOUSEWHEEL, WM_RBUTTONDOWN, WM_RBUTTONUP, WM_SYSKEYDOWN, WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use super::{MARKER, keymap};
use crate::{Delegate, Hotkey, Input};

static DELEGATE: OnceLock<Arc<dyn Delegate>> = OnceLock::new();
static REMOTE: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<State> = Mutex::new(State {
    hotkey: Hotkey::DEFAULT,
    modifiers: 0,
    seen_down: None,
    hotkey_key_down: false,
    center: POINT { x: 0, y: 0 },
});

struct State {
    hotkey: Hotkey,
    /// Modifier keys held right now: bit n for HID usage 0xE0 + n (key repeat sets it again).
    modifiers: u8,
    /// Keys Windows saw go down: their releases still reach Windows while remote, so
    /// nothing stays stuck there (e.g. the shortcut's own modifiers).
    seen_down: Option<HashSet<u16>>,
    /// The shortcut's key is down: its release is swallowed too.
    hotkey_key_down: bool,
    center: POINT,
}

/// Wheel delta for one notch.
const NOTCH: i32 = 120;
/// Wheel delta per pixel of smooth scrolling (a notch scrolls about ten pixels).
const PER_PIXEL: i32 = 12;

pub fn set_hotkey(hotkey: Hotkey) {
    STATE.lock().unwrap().hotkey = hotkey;
}

/// Starts or stops capturing for the peer.
pub fn set_remote(on: bool) {
    if on {
        let center = POINT { x: unsafe { GetSystemMetrics(SM_CXSCREEN) } / 2, y: unsafe { GetSystemMetrics(SM_CYSCREEN) } / 2 };
        STATE.lock().unwrap().center = center;
        super::ui::hide_cursor(Some(center));
        let _ = unsafe { SetCursorPos(center.x, center.y) }; // also makes Windows pick up the hidden cursor
    } else {
        super::ui::hide_cursor(None);
    }
    REMOTE.store(on, Ordering::Release);
}

pub fn is_remote() -> bool {
    REMOTE.load(Ordering::Acquire)
}

pub(super) struct Hooks(HHOOK, HHOOK);

pub(super) fn install(delegate: Arc<dyn Delegate>) -> anyhow::Result<Hooks> {
    let _ = DELEGATE.set(delegate);
    STATE.lock().unwrap().seen_down = Some(HashSet::new());
    let keyboard = unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0)? };
    let mouse = unsafe { SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)? };
    Ok(Hooks(keyboard, mouse))
}

impl Drop for Hooks {
    fn drop(&mut self) {
        unsafe {
            let _ = UnhookWindowsHookEx(self.0);
            let _ = UnhookWindowsHookEx(self.1);
        }
    }
}

fn send(input: Input) {
    if let Some(d) = DELEGATE.get() {
        d.input(input);
    }
}

const PASS: Option<LRESULT> = None;
const SWALLOW: Option<LRESULT> = Some(LRESULT(1));

unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let k = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
        let injected = k.flags.0 & LLKHF_INJECTED.0 != 0 || k.dwExtraInfo == MARKER;
        if !injected {
            let down = wparam.0 as u32 == WM_KEYDOWN || wparam.0 as u32 == WM_SYSKEYDOWN;
            let extended = k.flags.0 & LLKHF_EXTENDED.0 != 0;
            if let Some(result) = keyboard(k.scanCode as u16, extended, k.vkCode as u16, down) {
                return result;
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn keyboard(scan: u16, extended: bool, vk: u16, down: bool) -> Option<LRESULT> {
    let remote = is_remote();
    if let Some(usage) = keymap::media_usage(vk) {
        if !remote {
            return PASS;
        }
        send(Input::Consumer { usage, down });
        return SWALLOW;
    }
    let Some(hid) = keymap::hid(scan, extended, vk) else {
        return if remote { SWALLOW } else { PASS };
    };
    let mut state = STATE.lock().unwrap();
    if (0xE0..=0xE7).contains(&hid) {
        let bit = 1u8 << (hid - 0xE0);
        if down {
            state.modifiers |= bit;
        } else {
            state.modifiers &= !bit;
        }
    }

    let hotkey = state.hotkey;
    let modifiers = state.modifiers;
    let held = |left: u16| modifiers & (1 << (left - 0xE0) | 1 << (left + 4 - 0xE0)) != 0;
    if hid == hotkey.hid {
        let matches = held(0xE0) == hotkey.ctrl
            && held(0xE2) == hotkey.alt
            && held(0xE1) == hotkey.shift
            && held(0xE3) == hotkey.meta;
        if down && matches {
            state.hotkey_key_down = true;
            drop(state);
            if let Some(d) = DELEGATE.get() {
                d.hotkey();
            }
            return SWALLOW;
        }
        if !down && std::mem::take(&mut state.hotkey_key_down) {
            return SWALLOW;
        }
    }

    let seen = state.seen_down.get_or_insert_with(HashSet::new);
    if remote {
        if !down && seen.remove(&hid) {
            return PASS; // Windows saw it go down
        }
        drop(state);
        send(Input::Key { hid, down });
        return SWALLOW;
    }
    if down {
        seen.insert(hid);
    } else {
        seen.remove(&hid);
    }
    PASS
}

unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 && is_remote() {
        let m = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
        let injected = m.flags & LLMHF_INJECTED != 0 || m.dwExtraInfo == MARKER;
        if !injected {
            mouse(wparam.0 as u32, m);
            return LRESULT(1);
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

fn mouse(message: u32, m: &MSLLHOOKSTRUCT) {
    let wheel = (m.mouseData >> 16) as i16 as i32;
    let input = match message {
        WM_MOUSEMOVE => {
            let center = STATE.lock().unwrap().center;
            let (dx, dy) = ((m.pt.x - center.x) as i64, (m.pt.y - center.y) as i64);
            if dx == 0 && dy == 0 {
                return;
            }
            Input::Motion { dx, dy }
        }
        WM_LBUTTONDOWN | WM_LBUTTONUP => Input::Button { button: 0, down: message == WM_LBUTTONDOWN },
        WM_RBUTTONDOWN | WM_RBUTTONUP => Input::Button { button: 1, down: message == WM_RBUTTONDOWN },
        WM_MBUTTONDOWN | WM_MBUTTONUP => Input::Button { button: 2, down: message == WM_MBUTTONDOWN },
        WM_XBUTTONDOWN | WM_XBUTTONUP => {
            let button = if (m.mouseData >> 16) & 0xFFFF == 1 { 3 } else { 4 };
            Input::Button { button, down: message == WM_XBUTTONDOWN }
        }
        // Up is positive on both sides; horizontally Windows' right is the wire's negative.
        WM_MOUSEWHEEL if wheel % NOTCH == 0 => Input::Wheel { dx: 0, dy: (wheel / NOTCH) as i16 },
        WM_MOUSEWHEEL => Input::Scroll { dx: 0, dy: (wheel / PER_PIXEL) as i64 },
        WM_MOUSEHWHEEL if wheel % NOTCH == 0 => Input::Wheel { dx: (-wheel / NOTCH) as i16, dy: 0 },
        WM_MOUSEHWHEEL => Input::Scroll { dx: (-wheel / PER_PIXEL) as i64, dy: 0 },
        _ => return,
    };
    send(input);
}
