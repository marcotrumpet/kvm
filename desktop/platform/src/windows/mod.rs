mod inject;
mod keymap;

pub mod capture;
pub mod ui;

pub use inject::Injector;

/// Tags our injected events, so our own hooks let them through.
const MARKER: usize = 0x4B56_4D21;

/// Text on the clipboard. Use from one thread.
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

/// Message boxes. They block: call from a thread of their own.
pub mod dialog {
    use windows::Win32::UI::WindowsAndMessaging::{
        IDYES, MB_ICONINFORMATION, MB_ICONQUESTION, MB_OK, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO, MessageBoxW,
    };
    use windows::core::HSTRING;

    pub fn ask(title: &str, text: &str) -> bool {
        let style = MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND;
        unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), style) == IDYES }
    }

    pub fn tell(title: &str, text: &str) {
        let style = MB_OK | MB_ICONINFORMATION | MB_TOPMOST | MB_SETFOREGROUND;
        unsafe { MessageBoxW(None, &HSTRING::from(text), &HSTRING::from(title), style) };
    }
}

/// Updates with WinSparkle (WinSparkle.dll next to the executable), from the same GitHub
/// releases and signing key as the Mac app's Sparkle. Without the DLL (a development
/// build), there are no updates.
pub mod updater {
    use std::ffi::{CString, c_char, c_int};
    use std::sync::OnceLock;

    use libloading::Library;
    use windows::core::HSTRING;

    static DLL: OnceLock<Option<Library>> = OnceLock::new();

    fn dll() -> Option<&'static Library> {
        DLL.get_or_init(|| {
            let path = std::env::current_exe().ok()?.with_file_name("WinSparkle.dll");
            // SAFETY: WinSparkle's own DLL, shipped with the app; no initialization side effects.
            unsafe { Library::new(&path) }.ok()
        })
        .as_ref()
    }

    /// Starts checking (daily, asking before installing). `on_quit` is called on
    /// WinSparkle's thread when an update is ready to install: quit, so it can replace us.
    pub fn start(feed_url: &str, public_key: &str, version: &str, on_quit: extern "C" fn()) -> bool {
        let Some(lib) = dll() else { return false };
        let (Ok(url), Ok(key)) = (CString::new(feed_url), CString::new(public_key)) else { return false };
        // SAFETY: signatures as in winsparkle.h (0.9).
        unsafe {
            let set_url = lib.get::<unsafe extern "C" fn(*const c_char)>(b"win_sparkle_set_appcast_url\0");
            let set_key = lib.get::<unsafe extern "C" fn(*const c_char) -> c_int>(b"win_sparkle_set_eddsa_public_key\0");
            let details = lib.get::<unsafe extern "C" fn(*const u16, *const u16, *const u16)>(b"win_sparkle_set_app_details\0");
            let auto = lib.get::<unsafe extern "C" fn(c_int)>(b"win_sparkle_set_automatic_check_for_updates\0");
            let shutdown = lib.get::<unsafe extern "C" fn(extern "C" fn())>(b"win_sparkle_set_shutdown_request_callback\0");
            let init = lib.get::<unsafe extern "C" fn()>(b"win_sparkle_init\0");
            let (Ok(set_url), Ok(set_key), Ok(details), Ok(auto), Ok(shutdown), Ok(init)) =
                (set_url, set_key, details, auto, shutdown, init)
            else {
                return false;
            };
            set_url(url.as_ptr());
            if set_key(key.as_ptr()) == 0 {
                return false;
            }
            let (company, app, version) = (HSTRING::from("KVM"), HSTRING::from("KVM"), HSTRING::from(version));
            details(company.as_ptr(), app.as_ptr(), version.as_ptr());
            auto(1);
            shutdown(on_quit);
            init();
        }
        true
    }

    pub fn available() -> bool {
        dll().is_some()
    }

    pub fn check_now() {
        if let Some(lib) = dll() {
            // SAFETY: as above.
            if let Ok(check) = unsafe { lib.get::<unsafe extern "C" fn()>(b"win_sparkle_check_update_with_ui\0") } {
                unsafe { check() };
            }
        }
    }

    pub fn stop() {
        if let Some(lib) = dll() {
            // SAFETY: as above.
            if let Ok(cleanup) = unsafe { lib.get::<unsafe extern "C" fn()>(b"win_sparkle_cleanup\0") } {
                unsafe { cleanup() };
            }
        }
    }
}
