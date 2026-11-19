//! Shares this screen (X11): GetImage captures a monitor (RandR) on a connection of its own,
//! XFixes supplies the pointer to draw in, the picture is scaled to NV12 and encoded with
//! OpenH264 ([`crate::h264`]), then packaged for the viewers. Unchanged pictures aren't
//! encoded again. Same interface as the Windows host.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::randr::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{ConnectionExt as _, ImageFormat};
use x11rb::rust_connection::RustConnection;

use crate::h264::SoftwareEncoder;
use crate::video::{Display, H264Packager, ScreenParams, bgra_to_nv12, fit};

pub fn supported() -> bool {
    true
}

/// The monitors (RandR), or the whole screen as one; ids from 1.
pub fn displays() -> Vec<Display> {
    let Ok((conn, screen)) = RustConnection::connect(None) else { return Vec::new() };
    monitors(&conn, screen)
}

fn monitors(conn: &RustConnection, screen: usize) -> Vec<Display> {
    let root = &conn.setup().roots[screen];
    let listed = conn
        .randr_get_monitors(root.root, true)
        .ok()
        .and_then(|c| c.reply().ok())
        .map(|r| r.monitors)
        .unwrap_or_default();
    if listed.is_empty() {
        return vec![Display {
            id: 1,
            name: "Screen".into(),
            left: 0,
            top: 0,
            width: root.width_in_pixels as u32,
            height: root.height_in_pixels as u32,
            main: true,
        }];
    }
    let any_primary = listed.iter().any(|m| m.primary);
    listed
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let name = conn
                .get_atom_name(m.name)
                .ok()
                .and_then(|c| c.reply().ok())
                .map(|r| String::from_utf8_lossy(&r.name).into_owned())
                .unwrap_or_else(|| format!("Monitor {}", i + 1));
            Display {
                id: i as u32 + 1,
                name,
                left: m.x as i32,
                top: m.y as i32,
                width: m.width as u32,
                height: m.height as u32,
                main: if any_primary { m.primary } else { i == 0 },
            }
        })
        .collect()
}

pub fn display(id: u32) -> Option<Display> {
    let all = displays();
    all.iter().find(|d| if id == 0 { d.main } else { d.id == id }).or(all.first()).cloned()
}

pub type FrameSink = Box<dyn FnMut(Vec<u8>, bool) -> bool + Send>;

struct Shared {
    params: Mutex<ScreenParams>,
    bitrate_kbps: AtomicU32,
    keyframe: AtomicBool,
    stop: AtomicBool,
}

pub struct ScreenHost {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenHost {
    pub fn start(params: ScreenParams, sink: FrameSink) -> ScreenHost {
        let shared = Arc::new(Shared {
            params: Mutex::new(params),
            bitrate_kbps: AtomicU32::new(params.bitrate_kbps.min(8_000)),
            keyframe: AtomicBool::new(true),
            stop: AtomicBool::new(false),
        });
        let worker = shared.clone();
        let thread = std::thread::Builder::new()
            .name("screen".into())
            .spawn(move || run(worker, sink))
            .expect("screen thread");
        ScreenHost { shared, thread: Some(thread) }
    }

    pub fn update(&self, params: ScreenParams) {
        *self.shared.params.lock().unwrap() = params;
        self.shared.keyframe.store(true, Ordering::Release);
    }

    pub fn set_bitrate(&self, kbps: u32) {
        self.shared.bitrate_kbps.store(kbps, Ordering::Release);
    }

    pub fn request_keyframe(&self) {
        self.shared.keyframe.store(true, Ordering::Release);
    }
}

impl Drop for ScreenHost {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

fn run(shared: Arc<Shared>, mut sink: FrameSink) {
    while !shared.stop.load(Ordering::Acquire) {
        if let Err(err) = share(&shared, &mut sink) {
            tracing::warn!("screen sharing: {err}; retrying");
            std::thread::sleep(Duration::from_millis(500));
        }
    }
    tracing::info!("screen sharing stopped");
}

fn share(shared: &Shared, sink: &mut FrameSink) -> anyhow::Result<()> {
    let params = *shared.params.lock().unwrap();
    let (conn, screen) = RustConnection::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let shown = monitors(&conn, screen)
        .into_iter()
        .find(|d| if params.display == 0 { d.main } else { d.id == params.display })
        .ok_or_else(|| anyhow::anyhow!("no such display"))?;
    let has_cursor = conn.xfixes_query_version(4, 0).ok().and_then(|c| c.reply().ok()).is_some();
    let (src_w, src_h) = (shown.width, shown.height);
    let (width, height) = fit(src_w, src_h, params.max_width, params.max_height);
    let fps = params.fps.clamp(1, 60) as u32;
    let mut kbps = shared.bitrate_kbps.load(Ordering::Acquire);
    let mut encoder = SoftwareEncoder::new(width, height, fps, kbps)?;
    tracing::info!("sharing {} ({src_w}x{src_h}) as {width}x{height} @ {fps} fps, {kbps} kbit/s", shown.name);
    let mut packager = H264Packager::default();
    let (mut nv12, mut last) = (Vec::new(), Vec::new());
    let interval = Duration::from_secs(1) / fps;
    loop {
        let started = Instant::now();
        if shared.stop.load(Ordering::Acquire) || *shared.params.lock().unwrap() != params {
            return Ok(());
        }
        let wanted = shared.bitrate_kbps.load(Ordering::Acquire);
        if wanted != kbps {
            kbps = wanted;
            encoder.set_bitrate(kbps);
        }
        let image = conn
            .get_image(ImageFormat::Z_PIXMAP, root, shown.left as i16, shown.top as i16, src_w as u16, src_h as u16, !0)?
            .reply()?;
        let mut picture = image.data; // 32 bits per pixel, BGRX on little-endian servers
        if picture.len() < (src_w * src_h * 4) as usize {
            anyhow::bail!("unexpected image format (depth {})", image.depth);
        }
        if has_cursor && let Ok(cursor) = conn.xfixes_get_cursor_image()?.reply() {
            draw_cursor(&mut picture, src_w, src_h, &shown, &cursor);
        }
        let keyframe = shared.keyframe.swap(false, Ordering::AcqRel);
        if keyframe || picture != last {
            bgra_to_nv12(&picture, src_w, src_h, src_w as usize * 4, width, height, &mut nv12);
            let (annex_b, key) = encoder.encode(&nv12, keyframe)?;
            if let Some(payload) = packager.package(&annex_b, key)
                && !sink(payload, key)
            {
                shared.keyframe.store(true, Ordering::Release); // the viewer missed one
            }
            last = picture;
        }
        if let Some(rest) = interval.checked_sub(started.elapsed()) {
            std::thread::sleep(rest);
        }
    }
}

/// Draws XFixes' pointer image (premultiplied ARGB) over the monitor's picture.
fn draw_cursor(
    picture: &mut [u8],
    width: u32,
    height: u32,
    shown: &Display,
    cursor: &x11rb::protocol::xfixes::GetCursorImageReply,
) {
    let left = cursor.x as i32 - cursor.xhot as i32 - shown.left;
    let top = cursor.y as i32 - cursor.yhot as i32 - shown.top;
    for cy in 0..cursor.height as i32 {
        let y = top + cy;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for cx in 0..cursor.width as i32 {
            let x = left + cx;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let argb = cursor.cursor_image[(cy * cursor.width as i32 + cx) as usize];
            let a = argb >> 24;
            if a == 0 {
                continue;
            }
            let i = ((y as u32 * width + x as u32) * 4) as usize;
            for (c, shift) in [0u32, 8, 16].into_iter().enumerate() {
                let src = (argb >> shift) & 0xFF; // premultiplied
                let dst = picture[i + c] as u32;
                picture[i + c] = (src + dst * (255 - a) / 255).min(255) as u8;
            }
        }
    }
}
