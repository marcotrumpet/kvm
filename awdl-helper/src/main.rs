//! Root helper (a launchd daemon) that keeps `awdl0` down while the KVM app asks for it.
//!
//! AWDL (AirDrop, Sidecar, Universal Control) periodically moves the Wi-Fi radio to
//! another channel, which shows up as 80-100 ms input lag spikes. macOS brings the
//! interface back up whenever something wants it, so the helper keeps checking.
//!
//!   kvm-awdl-helper <request-file>
//!
//! The request file holds the PID of the running KVM app. While it exists and that process
//! is alive, awdl0 is kept down. Otherwise, if the helper took it down, it brings it back
//! up, so a crashed or quit app never leaves AirDrop disabled.

use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

const INTERFACE: &[u8] = b"awdl0";
const POLL: Duration = Duration::from_millis(500);

// _IOWR('i', 17, struct ifreq) and _IOW('i', 16, struct ifreq) on macOS.
const SIOCGIFFLAGS: libc::c_ulong = 0xc020_6911;
const SIOCSIFFLAGS: libc::c_ulong = 0x8020_6910;

#[repr(C)]
struct IfReq {
    name: [u8; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 14],
}

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_signal(_: libc::c_int) {
    STOP.store(true, Ordering::SeqCst);
}

fn main() {
    let Some(request_file) = std::env::args_os().nth(1) else {
        eprintln!("usage: kvm-awdl-helper <request-file>");
        std::process::exit(2);
    };
    let request_file = Path::new(&request_file);
    unsafe {
        libc::signal(libc::SIGTERM, on_signal as *const () as libc::sighandler_t);
        libc::signal(libc::SIGINT, on_signal as *const () as libc::sighandler_t);
    }
    let sock = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0) };
    if sock < 0 {
        eprintln!("socket: {}", std::io::Error::last_os_error());
        std::process::exit(1);
    }

    let mut we_took_it_down = false;
    while !STOP.load(Ordering::SeqCst) {
        let wanted_down = is_requested(request_file);
        if let Some(flags) = get_flags(sock) {
            let up = flags & libc::IFF_UP as libc::c_short != 0;
            if wanted_down && up {
                if set_flags(sock, flags & !(libc::IFF_UP as libc::c_short)) {
                    we_took_it_down = true;
                    eprintln!("awdl0 down");
                }
            } else if !wanted_down && we_took_it_down {
                if !up && set_flags(sock, flags | libc::IFF_UP as libc::c_short) {
                    eprintln!("awdl0 up");
                }
                we_took_it_down = false;
            }
        }
        std::thread::sleep(POLL);
    }

    if we_took_it_down
        && let Some(flags) = get_flags(sock)
    {
        set_flags(sock, flags | libc::IFF_UP as libc::c_short);
        eprintln!("awdl0 up (helper stopping)");
    }
}

/// True if the request file names a live process.
fn is_requested(path: &Path) -> bool {
    let Ok(mut file) = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
    else {
        return false;
    };
    let mut buf = [0u8; 32];
    let n = file.read(&mut buf).unwrap_or(0);
    let Some(pid) = std::str::from_utf8(&buf[..n])
        .ok()
        .and_then(|s| s.trim().parse::<libc::pid_t>().ok())
        .filter(|&pid| pid > 1)
    else {
        return false;
    };
    let alive = unsafe { libc::kill(pid, 0) } == 0
        || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM);
    alive
}

fn request() -> IfReq {
    let mut req = IfReq { name: [0; libc::IFNAMSIZ], flags: 0, _pad: [0; 14] };
    req.name[..INTERFACE.len()].copy_from_slice(INTERFACE);
    req
}

fn get_flags(sock: libc::c_int) -> Option<libc::c_short> {
    let mut req = request();
    (unsafe { libc::ioctl(sock, SIOCGIFFLAGS, &mut req) } == 0).then_some(req.flags)
}

fn set_flags(sock: libc::c_int, flags: libc::c_short) -> bool {
    let mut req = request();
    req.flags = flags;
    let ok = unsafe { libc::ioctl(sock, SIOCSIFFLAGS, &mut req) } == 0;
    if !ok {
        eprintln!("setting awdl0 flags: {}", std::io::Error::last_os_error());
    }
    ok
}
