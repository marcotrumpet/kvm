mod inject;
mod keymap;

pub mod capture;
pub mod screen;
pub mod ui;

pub use inject::Injector;

/// Tags our injected events, so our own hooks let them through.
const MARKER: usize = 0x4B56_4D21;

/// The proxy set in Windows' Internet Settings (Settings > Network > Proxy), as a URL for
/// HTTPS_PROXY: "host:port", or the https= (else http=) entry of a per-protocol list.
pub fn system_proxy() -> Option<String> {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;

    let key = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Internet Settings");
    let mut enabled = 0u32;
    let mut size = 4u32;
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, w!("ProxyEnable"), RRF_RT_REG_DWORD, None,
                     Some(&mut enabled as *mut u32 as *mut _), Some(&mut size))
    };
    if status.is_err() || enabled == 0 {
        return None;
    }
    let mut buf = [0u16; 512];
    let mut size = (buf.len() * 2) as u32;
    let status = unsafe {
        RegGetValueW(HKEY_CURRENT_USER, key, w!("ProxyServer"), RRF_RT_REG_SZ, None,
                     Some(buf.as_mut_ptr() as *mut _), Some(&mut size))
    };
    if status.is_err() {
        return None;
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    let server = String::from_utf16_lossy(&buf[..len]);
    let pick = |scheme: &str| {
        server.split(';').find_map(|part| part.strip_prefix(scheme).map(str::to_string))
    };
    let host = if server.contains('=') { pick("https=").or_else(|| pick("http="))? } else { server.trim().to_string() };
    (!host.is_empty()).then(|| if host.contains("://") { host } else { format!("http://{host}") })
}

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

    /// The standard Open dialog, several files at once; empty if cancelled.
    pub fn choose_files(title: &str) -> Vec<std::path::PathBuf> {
        use windows::Win32::System::Com::{
            CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree,
        };
        use windows::Win32::UI::Shell::{
            FOS_ALLOWMULTISELECT, FOS_FORCEFILESYSTEM, FileOpenDialog, IFileOpenDialog, SIGDN_FILESYSPATH,
        };

        let pick = || -> windows::core::Result<Vec<std::path::PathBuf>> {
            unsafe {
                let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
                let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER)?;
                dialog.SetOptions(dialog.GetOptions()? | FOS_ALLOWMULTISELECT | FOS_FORCEFILESYSTEM)?;
                dialog.SetTitle(&HSTRING::from(title))?;
                dialog.Show(None)?; // an error when cancelled
                let items = dialog.GetResults()?;
                let mut paths = Vec::new();
                for i in 0..items.GetCount()? {
                    let name = items.GetItemAt(i)?.GetDisplayName(SIGDN_FILESYSPATH)?;
                    paths.push(std::path::PathBuf::from(name.to_string()?));
                    CoTaskMemFree(Some(name.0 as _));
                }
                Ok(paths)
            }
        };
        pick().unwrap_or_default()
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
