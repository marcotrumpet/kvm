//! Stand-ins where there is no implementation yet (macOS has its own app), so the app
//! still builds and its logic can be checked anywhere.

#![allow(dead_code)]

#[derive(Default)]
pub struct Injector;

impl Injector {
    pub fn begin(&self) {}
    pub fn end(&self) {}
    pub fn motion(&self, _dx: i64, _dy: i64) {}
    pub fn warp(&self, _fx: u16, _fy: u16, _display: u32) {}
    pub fn button(&self, _button: u8, _down: bool) {}
    pub fn wheel(&self, _dx: i16, _dy: i16) {}
    pub fn scroll(&self, _dx: i64, _dy: i64) {}
    pub fn key(&self, _hid: u16, _down: bool) {}
    pub fn consumer(&self, _usage: u16, _down: bool) {}
    pub fn text(&self, _text: &str) {}
    pub fn lock_screen(&self) {}
}

pub fn system_proxy() -> Option<String> {
    None
}

/// Sharing this screen isn't available here yet.
pub mod screen {
    pub use crate::video::Display;
    use crate::video::ScreenParams;

    pub type FrameSink = Box<dyn FnMut(Vec<u8>, bool) -> bool + Send>;

    pub fn supported() -> bool {
        false
    }

    pub fn displays() -> Vec<Display> {
        Vec::new()
    }

    pub struct ScreenHost;

    impl ScreenHost {
        pub fn start(_params: ScreenParams, _sink: FrameSink) -> ScreenHost {
            ScreenHost
        }
        pub fn update(&self, _params: ScreenParams) {}
        pub fn set_bitrate(&self, _kbps: u32) {}
        pub fn request_keyframe(&self) {}
    }
}

pub mod capture {
    pub fn set_hotkey(_hotkey: crate::Hotkey) {}
    pub fn set_remote(_on: bool) {}
    pub fn is_remote() -> bool {
        false
    }
}

pub mod ui {
    use std::sync::Arc;

    pub fn set_menu(_entries: Vec<crate::MenuEntry>) {}
    pub fn set_tooltip(_text: &str) {}
    pub fn quit() {}
    pub fn run(_delegate: Arc<dyn crate::Delegate>, _hotkey: crate::Hotkey) -> anyhow::Result<()> {
        anyhow::bail!("this platform has no desktop app here (use the Mac app on macOS)")
    }
}

pub mod clipboard {
    pub struct Clipboard;

    impl Clipboard {
        pub fn new() -> anyhow::Result<Self> {
            anyhow::bail!("no clipboard on this platform")
        }
        pub fn text(&mut self) -> Option<String> {
            None
        }
        pub fn set_text(&mut self, _text: &str) {}
    }
}

pub mod dialog {
    pub fn ask(_title: &str, _text: &str) -> bool {
        false
    }
    pub fn tell(_title: &str, _text: &str) {}
    pub fn choose_files(_title: &str) -> Vec<std::path::PathBuf> {
        Vec::new()
    }
}

pub mod updater {
    pub fn start(_feed_url: &str, _public_key: &str, _version: &str, _on_quit: extern "C" fn()) -> bool {
        false
    }
    pub fn available() -> bool {
        false
    }
    pub fn check_now() {}
    pub fn stop() {}
}
