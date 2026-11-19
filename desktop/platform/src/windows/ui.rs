//! The tray icon and the UI thread's message loop, which also runs the input hooks. Other
//! threads hand over menus and tooltips; the loop applies them on its timer.

use std::cell::RefCell;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};

use tray_icon::menu::{CheckMenuItem, IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, KillTimer, MSG, PostThreadMessageW, SetTimer, TranslateMessage, WM_QUIT, WM_TIMER,
};

use super::capture;
use crate::{Delegate, Hotkey, MenuEntry};

static PENDING: Mutex<Pending> = Mutex::new(Pending { menu: None, tooltip: None });
static UI_THREAD: AtomicU32 = AtomicU32::new(0);

struct Pending {
    menu: Option<Vec<MenuEntry>>,
    tooltip: Option<String>,
}

thread_local! {
    static TRAY: RefCell<Option<TrayIcon>> = const { RefCell::new(None) };
}

/// Shows `entries` as the tray menu. Any thread.
pub fn set_menu(entries: Vec<MenuEntry>) {
    PENDING.lock().unwrap().menu = Some(entries);
}

/// Any thread.
pub fn set_tooltip(text: &str) {
    PENDING.lock().unwrap().tooltip = Some(text.to_string());
}

/// Ends [`run`]. Any thread.
pub fn quit() {
    let thread = UI_THREAD.load(Ordering::Acquire);
    if thread != 0 {
        let _ = unsafe { PostThreadMessageW(thread, WM_QUIT, WPARAM(0), LPARAM(0)) };
    }
}

/// Runs the UI on this thread (the main one) until [`quit`].
pub fn run(delegate: Arc<dyn Delegate>, hotkey: Hotkey) -> anyhow::Result<()> {
    UI_THREAD.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
    capture::set_hotkey(hotkey);
    let _hooks = capture::install(delegate.clone())?;
    let tray = TrayIconBuilder::new()
        .with_icon(icon())
        .with_tooltip("KVM")
        .with_menu(Box::new(Menu::new()))
        .build()?;
    TRAY.with(|t| *t.borrow_mut() = Some(tray));

    let timer = unsafe { SetTimer(None, 0, 50, None) };
    let mut msg = MSG::default();
    loop {
        let got = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if got.0 <= 0 || msg.message == WM_QUIT {
            break;
        }
        if msg.message == WM_TIMER && msg.hwnd.is_invalid() {
            tick(&delegate);
            continue;
        }
        unsafe {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    let _ = unsafe { KillTimer(None, timer) };
    TRAY.with(|t| t.borrow_mut().take());
    Ok(())
}

fn tick(delegate: &Arc<dyn Delegate>) {
    while let Ok(event) = MenuEvent::receiver().try_recv() {
        delegate.menu(&event.id.0);
    }
    let (menu, tooltip) = {
        let mut p = PENDING.lock().unwrap();
        (p.menu.take(), p.tooltip.take())
    };
    TRAY.with(|t| {
        let tray = t.borrow();
        let Some(tray) = tray.as_ref() else { return };
        if let Some(entries) = menu {
            let built = Menu::new();
            for entry in &entries {
                let _ = built.append(build(entry).as_ref());
            }
            tray.set_menu(Some(Box::new(built)));
        }
        if let Some(text) = tooltip {
            let _ = tray.set_tooltip(Some(text));
        }
    });
}

fn build(entry: &MenuEntry) -> Box<dyn IsMenuItem> {
    match entry {
        MenuEntry::Item { id, label, enabled, checked: Some(checked) } => {
            Box::new(CheckMenuItem::with_id(id.as_str(), label, *enabled, *checked, None))
        }
        MenuEntry::Item { id, label, enabled, checked: None } => {
            Box::new(MenuItem::with_id(id.as_str(), label, *enabled, None))
        }
        MenuEntry::Submenu { label, children } => {
            let submenu = Submenu::new(label, !children.is_empty());
            for child in children {
                let _ = submenu.append(build(child).as_ref());
            }
            Box::new(submenu)
        }
        MenuEntry::Separator => Box::new(PredefinedMenuItem::separator()),
    }
}

/// The app icon's look: two screens on the orange-to-violet gradient, drawn at 32x32.
fn icon() -> Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    let lerp = |a: f32, b: f32, t: f32| a + (b - a) * t;
    let inside_rounded = |x: f32, y: f32, l: f32, t: f32, r: f32, b: f32, radius: f32| {
        let cx = x.clamp(l + radius, r - radius);
        let cy = y.clamp(t + radius, b - radius);
        (x - cx).powi(2) + (y - cy).powi(2) <= radius * radius && x >= l && x <= r && y >= t && y <= b
    };
    let outline = |x: f32, y: f32, l: f32, t: f32, r: f32, b: f32, w: f32| {
        inside_rounded(x, y, l, t, r, b, 2.0) && !inside_rounded(x, y, l + w, t + w, r - w, b - w, 1.0)
    };
    for py in 0..N {
        for px in 0..N {
            let (x, y) = (px as f32 + 0.5, py as f32 + 0.5);
            let i = (py * N + px) * 4;
            if !inside_rounded(x, y, 0.0, 0.0, N as f32, N as f32, 7.0) {
                continue;
            }
            let t = (x + y) / (2.0 * N as f32);
            let (r, g, b) = if t < 0.5 {
                let u = t * 2.0;
                (lerp(255.0, 255.0, u), lerp(181.0, 79.0, u), lerp(71.0, 123.0, u))
            } else {
                let u = (t - 0.5) * 2.0;
                (lerp(255.0, 184.0, u), lerp(79.0, 59.0, u), lerp(123.0, 214.0, u))
            };
            let mut pixel = [r as u8, g as u8, b as u8, 255];
            let front = outline(x, y, 13.0, 12.0, 28.0, 23.0, 2.0)
                || (x >= 19.5 && x <= 21.5 && y >= 23.0 && y <= 26.0)
                || (x >= 16.0 && x <= 25.0 && y >= 25.5 && y <= 27.5);
            let behind = inside_rounded(x, y, 11.0, 10.0, 30.0, 25.0, 3.0);
            let back = !behind && outline(x, y, 4.0, 5.0, 19.0, 16.0, 2.0);
            if front {
                pixel = [255, 255, 255, 255];
            } else if back {
                pixel = [255, 225, 230, 255];
            }
            rgba[i..i + 4].copy_from_slice(&pixel);
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32).expect("valid icon")
}
