//! Shows the peer's screen in a window (X11): FFmpeg decodes on a thread of its own
//! ([`crate::avcodec`]), the picture is scaled to fit and put with PutImage on a connection
//! of the viewer's own, whose events another thread reads. Clicking the picture reports
//! where (as fractions), so the app can take control there; one viewer at a time.

use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use x11rb::connection::{Connection, RequestConnection as _};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{
    AtomEnum, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, ImageFormat,
    ImageOrder, PropMode, Rectangle, Window, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::avcodec;
use crate::video::{scale_bgra, unpack};

/// What happens in the viewer window (on the viewer's event thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerEvent {
    Closed,
    /// The picture was clicked here, as fractions 0..=65535.
    Clicked { x: u16, y: u16 },
    /// The window's picture area changed size, in pixels.
    Resized { width: u32, height: u32 },
}

pub type ViewerHandler = Arc<dyn Fn(ViewerEvent) + Send + Sync>;

/// One open viewer: its connection, window, and latest picture.
struct Viewer {
    conn: RustConnection,
    window: Window,
    gc: u32,
    depth: u8,
    wm_protocols: u32,
    wm_delete: u32,
    size: Mutex<(u16, u16)>,
    picture: Mutex<Picture>,
}

#[derive(Default)]
struct Picture {
    bgra: Vec<u8>,
    width: usize,
    height: usize,
    /// `bgra` scaled to the window, reused.
    scaled: Vec<u8>,
}

struct Open {
    viewer: Arc<Viewer>,
    frames: Sender<Vec<u8>>,
    handler: ViewerHandler,
}

static OPEN: Mutex<Option<Open>> = Mutex::new(None);

/// Whether FFmpeg (libavcodec) is installed to decode with.
pub fn supported() -> bool {
    avcodec::available()
}

pub fn is_open() -> bool {
    OPEN.lock().unwrap().is_some()
}

/// Opens the viewer. Returns its picture area in pixels.
pub fn open(title: &str, handler: ViewerHandler) -> Option<(u32, u32)> {
    if is_open() {
        return size();
    }
    let viewer = match create(title) {
        Ok(viewer) => Arc::new(viewer),
        Err(err) => {
            tracing::warn!("no viewer window: {err}");
            return None;
        }
    };
    let (frames, rx) = channel();
    *OPEN.lock().unwrap() = Some(Open { viewer: viewer.clone(), frames, handler });
    let decoding = viewer.clone();
    let events = viewer.clone();
    let started = std::thread::Builder::new().name("viewer".into()).spawn(move || decode_loop(rx, decoding)).is_ok()
        && std::thread::Builder::new().name("viewer events".into()).spawn(move || event_loop(events)).is_ok();
    if !started {
        OPEN.lock().unwrap().take();
        return None;
    }
    size()
}

/// The picture area in pixels.
pub fn size() -> Option<(u32, u32)> {
    let open = OPEN.lock().unwrap();
    let (w, h) = *open.as_ref()?.viewer.size.lock().unwrap();
    Some((w.max(1) as u32, h.max(1) as u32))
}

/// One frame from the peer (any thread).
pub fn frame(payload: Vec<u8>) {
    if let Some(open) = OPEN.lock().unwrap().as_ref() {
        let _ = open.frames.send(payload);
    }
}

/// Closes the viewer (any thread).
pub fn close() {
    if let Some(open) = OPEN.lock().unwrap().as_ref() {
        let v = &open.viewer;
        let _ = v.conn.destroy_window(v.window);
        let _ = v.conn.flush();
    }
}

fn create(title: &str) -> anyhow::Result<Viewer> {
    let (conn, screen) = RustConnection::connect(None)?;
    let setup = conn.setup();
    let s = &setup.roots[screen];
    let (root, depth, black) = (s.root, s.root_depth, s.black_pixel);
    // Pictures are put as 32-bit BGRA: the usual TrueColor layout, little-endian.
    let bgra = setup.image_byte_order == ImageOrder::LSB_FIRST
        && setup.pixmap_formats.iter().any(|f| f.depth == depth && f.bits_per_pixel == 32)
        && (depth == 24 || depth == 32);
    anyhow::ensure!(bgra, "unsupported display format (depth {depth})");
    let atom = |name: &[u8]| -> anyhow::Result<u32> { Ok(conn.intern_atom(false, name)?.reply()?.atom) };
    let (wm_protocols, wm_delete) = (atom(b"WM_PROTOCOLS")?, atom(b"WM_DELETE_WINDOW")?);
    let (net_wm_name, utf8) = (atom(b"_NET_WM_NAME")?, atom(b"UTF8_STRING")?);

    let (width, height) = (1280u16, 800u16);
    let window = conn.generate_id()?;
    let events = EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::BUTTON_PRESS;
    let aux = CreateWindowAux::new().background_pixel(black).event_mask(events);
    conn.create_window(0, window, root, 0, 0, width, height, 0, WindowClass::INPUT_OUTPUT, 0, &aux)?;
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes())?;
    conn.change_property8(PropMode::REPLACE, window, net_wm_name, utf8, title.as_bytes())?;
    conn.change_property8(PropMode::REPLACE, window, AtomEnum::WM_CLASS, AtomEnum::STRING, b"kvm\0KVM\0")?;
    conn.change_property32(PropMode::REPLACE, window, wm_protocols, AtomEnum::ATOM, &[wm_delete])?;
    let gc = conn.generate_id()?;
    conn.create_gc(gc, window, &CreateGCAux::new().foreground(black).graphics_exposures(0))?;
    conn.map_window(window)?;
    conn.flush()?;
    Ok(Viewer {
        conn,
        window,
        gc,
        depth,
        wm_protocols,
        wm_delete,
        size: Mutex::new((width, height)),
        picture: Mutex::new(Picture::default()),
    })
}

fn notify(viewer: &Viewer, event: ViewerEvent) {
    let handler = OPEN.lock().unwrap().as_ref().filter(|o| o.viewer.window == viewer.window).map(|o| o.handler.clone());
    if let Some(handler) = handler {
        handler(event);
    }
}

/// Where the picture goes in a `cw` x `ch` window, keeping its aspect ratio: x, y, w, h.
fn picture_rect(cw: u16, ch: u16, width: usize, height: usize) -> (i16, i16, u16, u16) {
    let scale = (cw as f64 / width.max(1) as f64).min(ch as f64 / height.max(1) as f64);
    let (w, h) = (((width as f64 * scale) as u16).max(1), ((height as f64 * scale) as u16).max(1));
    (((cw - w.min(cw)) / 2) as i16, ((ch - h.min(ch)) / 2) as i16, w, h)
}

fn event_loop(viewer: Arc<Viewer>) {
    let v = &*viewer;
    while let Ok(event) = v.conn.wait_for_event() {
        match event {
            Event::Expose(e) if e.count == 0 => draw(v),
            Event::ConfigureNotify(e) => {
                let changed = {
                    let mut size = v.size.lock().unwrap();
                    let changed = *size != (e.width, e.height);
                    *size = (e.width, e.height);
                    changed
                };
                if changed && e.width > 0 && e.height > 0 {
                    notify(v, ViewerEvent::Resized { width: e.width as u32, height: e.height as u32 });
                }
            }
            Event::ButtonPress(e) if e.detail == 1 => {
                let (pw, ph) = {
                    let p = v.picture.lock().unwrap();
                    (p.width, p.height)
                };
                let (cw, ch) = *v.size.lock().unwrap();
                let (rx, ry, rw, rh) = picture_rect(cw, ch, pw, ph);
                let (x, y) = (e.event_x - rx, e.event_y - ry);
                if pw > 0 && x >= 0 && y >= 0 && (x as u16) < rw && (y as u16) < rh {
                    let fx = (x as f64 / rw as f64 * 65535.0) as u16;
                    let fy = (y as f64 / rh as f64 * 65535.0) as u16;
                    notify(v, ViewerEvent::Clicked { x: fx, y: fy });
                }
            }
            Event::ClientMessage(e) if e.type_ == v.wm_protocols && e.data.as_data32()[0] == v.wm_delete => {
                let _ = v.conn.destroy_window(v.window);
                let _ = v.conn.flush();
            }
            Event::DestroyNotify(e) if e.window == v.window => break,
            _ => {}
        }
    }
    // Closed first (dropping the frame channel stops the decoder), then the app hears.
    let open = {
        let mut open = OPEN.lock().unwrap();
        if open.as_ref().is_some_and(|o| o.viewer.window == v.window) { open.take() } else { None }
    };
    if let Some(open) = open {
        (open.handler)(ViewerEvent::Closed);
    }
}

fn decode_loop(frames: Receiver<Vec<u8>>, viewer: Arc<Viewer>) {
    let Some(mut decoder) = avcodec::Decoder::new() else {
        tracing::warn!("no H.264 decoder (is FFmpeg's libavcodec installed?)");
        return;
    };
    let mut bgra = Vec::new();
    while let Ok(mut payload) = frames.recv() {
        // Behind? Decode everything (later frames build on earlier ones) but draw once.
        let mut size = None;
        loop {
            if let Some((hevc, _, annex_b)) = unpack(&payload)
                && !hevc
            {
                size = decoder.decode(&annex_b, &mut bgra).or(size);
            }
            match frames.try_recv() {
                Ok(next) => payload = next,
                Err(_) => break,
            }
        }
        let Some((w, h)) = size else { continue };
        {
            let mut p = viewer.picture.lock().unwrap();
            std::mem::swap(&mut p.bgra, &mut bgra);
            (p.width, p.height) = (w, h);
        }
        draw(&viewer);
    }
}

/// Paints the latest picture, scaled to fit, with black bars around it.
fn draw(v: &Viewer) {
    let (cw, ch) = *v.size.lock().unwrap();
    let mut guard = v.picture.lock().unwrap();
    let p = &mut *guard;
    let c = &v.conn;
    if p.width == 0 || cw == 0 || ch == 0 {
        let _ = c.poly_fill_rectangle(v.window, v.gc, &[Rectangle { x: 0, y: 0, width: cw, height: ch }]);
        let _ = c.flush();
        return;
    }
    let (x, y, w, h) = picture_rect(cw, ch, p.width, p.height);
    let bars = [
        Rectangle { x: 0, y: 0, width: cw, height: y as u16 },
        Rectangle { x: 0, y: y + h as i16, width: cw, height: ch.saturating_sub(y as u16 + h) },
        Rectangle { x: 0, y, width: x as u16, height: h },
        Rectangle { x: x + w as i16, y, width: cw.saturating_sub(x as u16 + w), height: h },
    ];
    let bars: Vec<Rectangle> = bars.into_iter().filter(|r| r.width > 0 && r.height > 0).collect();
    if !bars.is_empty() {
        let _ = c.poly_fill_rectangle(v.window, v.gc, &bars);
    }
    scale_bgra(&p.bgra, p.width, p.height, w as usize, h as usize, &mut p.scaled);
    // In bands that fit the server's request size.
    let row = w as usize * 4;
    let rows = ((c.maximum_request_bytes().saturating_sub(64)) / row).clamp(1, h as usize);
    for (i, band) in p.scaled.chunks(rows * row).enumerate() {
        let top = y + (i * rows) as i16;
        let n = (band.len() / row) as u16;
        let _ = c.put_image(ImageFormat::Z_PIXMAP, v.window, v.gc, w, n, x, top, 0, v.depth, band);
    }
    let _ = c.flush();
}
