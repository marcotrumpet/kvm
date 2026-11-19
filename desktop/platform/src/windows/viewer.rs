//! Shows the peer's screen in a window: Media Foundation's H.264 decoder on a thread of its
//! own, NV12 converted to BGRA, drawn scaled to fit with GDI. Clicking the picture reports
//! where (as fractions), so the app can take control there; one viewer at a time.

use std::mem::ManuallyDrop;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BI_RGB, BITMAPINFO, BITMAPINFOHEADER, BLACK_BRUSH, BeginPaint, DIB_RGB_COLORS, EndPaint, FillRect, GetStockObject,
    HALFTONE, HBRUSH, InvalidateRect, PAINTSTRUCT, SRCCOPY, SetStretchBltMode, StretchDIBits,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264DecoderMFT, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFTransform, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_E_TRANSFORM_STREAM_CHANGE, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE, MF_MT_MAJOR_TYPE,
    MF_MT_MINIMUM_DISPLAY_APERTURE, MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFMediaType_Video, MFSTARTUP_LITE, MFStartup, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_OUTPUT_DATA_BUFFER,
    MFVideoArea, MFVideoFormat_H264, MFVideoFormat_NV12,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::WindowsAndMessaging::{
    CW_USEDEFAULT, CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, IDC_ARROW, LoadCursorW,
    PostMessageW, RegisterClassW, SW_SHOW, ShowWindow, WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_PAINT, WM_SIZE, WNDCLASSW, WS_OVERLAPPEDWINDOW,
};
use windows::core::{HSTRING, Interface, w};

use crate::video::{nv12_to_bgra, unpack};

/// What happens in the viewer window (on the UI thread).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewerEvent {
    Closed,
    /// The picture was clicked here, as fractions 0..=65535.
    Clicked { x: u16, y: u16 },
    /// The window's picture area changed size, in pixels.
    Resized { width: u32, height: u32 },
}

pub type ViewerHandler = Arc<dyn Fn(ViewerEvent) + Send + Sync>;

struct Open {
    hwnd: isize,
    frames: Sender<Vec<u8>>,
    handler: ViewerHandler,
}

#[derive(Default)]
struct Picture {
    bgra: Vec<u8>,
    width: usize,
    height: usize,
}

static OPEN: Mutex<Option<Open>> = Mutex::new(None);
static PICTURE: Mutex<Picture> = Mutex::new(Picture { bgra: Vec::new(), width: 0, height: 0 });

pub fn supported() -> bool {
    true
}

pub fn is_open() -> bool {
    OPEN.lock().unwrap().is_some()
}

/// Opens the viewer (UI thread). Returns its picture area in pixels.
pub fn open(title: &str, handler: ViewerHandler) -> Option<(u32, u32)> {
    if is_open() {
        return size();
    }
    unsafe {
        let module = GetModuleHandleW(None).ok()?;
        let class = WNDCLASSW {
            lpfnWndProc: Some(window_proc),
            hInstance: module.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            lpszClassName: w!("KVMViewer"),
            ..Default::default()
        };
        RegisterClassW(&class);
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("KVMViewer"),
            &HSTRING::from(title),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            1280,
            800,
            None,
            None,
            Some(module.into()),
            None,
        )
        .ok()?;
        let (frames, rx) = channel();
        *OPEN.lock().unwrap() = Some(Open { hwnd: hwnd.0 as isize, frames, handler });
        *PICTURE.lock().unwrap() = Picture::default();
        let target = hwnd.0 as isize;
        std::thread::Builder::new().name("viewer".into()).spawn(move || decode_loop(rx, target)).ok()?;
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    size()
}

/// The picture area in pixels.
pub fn size() -> Option<(u32, u32)> {
    let hwnd = HWND(OPEN.lock().unwrap().as_ref()?.hwnd as *mut _);
    let mut r = RECT::default();
    unsafe { GetClientRect(hwnd, &mut r).ok()? };
    Some(((r.right - r.left).max(1) as u32, (r.bottom - r.top).max(1) as u32))
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
        let _ = unsafe { PostMessageW(Some(HWND(open.hwnd as *mut _)), WM_CLOSE, WPARAM(0), LPARAM(0)) };
    }
}

fn notify(event: ViewerEvent) {
    let handler = OPEN.lock().unwrap().as_ref().map(|o| o.handler.clone());
    if let Some(handler) = handler {
        handler(event);
    }
}

/// Where the picture goes in a `width` x `height` client area, keeping its aspect ratio.
fn picture_rect(client: &RECT, width: usize, height: usize) -> RECT {
    let (cw, ch) = ((client.right - client.left) as f64, (client.bottom - client.top) as f64);
    let scale = (cw / width.max(1) as f64).min(ch / height.max(1) as f64);
    let (w, h) = ((width as f64 * scale) as i32, (height as f64 * scale) as i32);
    let (x, y) = (((cw as i32) - w) / 2, ((ch as i32) - h) / 2);
    RECT { left: x, top: y, right: x + w, bottom: y + h }
}

unsafe extern "system" fn window_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            unsafe { paint(hwnd) };
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1), // painted whole, without flicker
        WM_SIZE => {
            let (w, h) = ((lparam.0 & 0xFFFF) as u32, ((lparam.0 >> 16) & 0xFFFF) as u32);
            if w > 0 && h > 0 {
                notify(ViewerEvent::Resized { width: w, height: h });
            }
            LRESULT(0)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = ((lparam.0 & 0xFFFF) as i16 as i32, ((lparam.0 >> 16) & 0xFFFF) as i16 as i32);
            let (pw, ph) = {
                let p = PICTURE.lock().unwrap();
                (p.width, p.height)
            };
            let mut client = RECT::default();
            let _ = unsafe { GetClientRect(hwnd, &mut client) };
            let r = picture_rect(&client, pw, ph);
            if pw > 0 && x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                let fx = ((x - r.left) as f64 / (r.right - r.left).max(1) as f64 * 65535.0) as u16;
                let fy = ((y - r.top) as f64 / (r.bottom - r.top).max(1) as f64 * 65535.0) as u16;
                notify(ViewerEvent::Clicked { x: fx, y: fy });
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = unsafe { DestroyWindow(hwnd) };
            LRESULT(0)
        }
        WM_DESTROY => {
            // Closed first (dropping the frame channel stops the decoder), then the app hears.
            let open = OPEN.lock().unwrap().take();
            if let Some(open) = open {
                (open.handler)(ViewerEvent::Closed);
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = unsafe { BeginPaint(hwnd, &mut ps) };
    let mut client = RECT::default();
    let _ = unsafe { GetClientRect(hwnd, &mut client) };
    let black = HBRUSH(unsafe { GetStockObject(BLACK_BRUSH) }.0);
    let picture = PICTURE.lock().unwrap();
    if picture.width == 0 {
        unsafe { FillRect(hdc, &client, black) };
    } else {
        let r = picture_rect(&client, picture.width, picture.height);
        // Black bars around the picture.
        for bar in [
            RECT { left: 0, top: 0, right: client.right, bottom: r.top },
            RECT { left: 0, top: r.bottom, right: client.right, bottom: client.bottom },
            RECT { left: 0, top: r.top, right: r.left, bottom: r.bottom },
            RECT { left: r.right, top: r.top, right: client.right, bottom: r.bottom },
        ] {
            unsafe { FillRect(hdc, &bar, black) };
        }
        let info = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: picture.width as i32,
                biHeight: -(picture.height as i32), // top-down rows
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        unsafe {
            SetStretchBltMode(hdc, HALFTONE);
            StretchDIBits(
                hdc,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                0,
                0,
                picture.width as i32,
                picture.height as i32,
                Some(picture.bgra.as_ptr().cast()),
                &info,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }
    }
    drop(picture);
    let _ = unsafe { EndPaint(hwnd, &ps) };
}

fn decode_loop(frames: Receiver<Vec<u8>>, hwnd: isize) {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let _ = MFStartup(MF_VERSION, MFSTARTUP_LITE);
    }
    let mut decoder = match Decoder::new() {
        Ok(d) => d,
        Err(err) => {
            tracing::warn!("no H.264 decoder: {err}");
            return;
        }
    };
    let mut bgra = Vec::new();
    while let Ok(payload) = frames.recv() {
        let Some((hevc, _, annex_b)) = unpack(&payload) else { continue };
        if hevc {
            continue; // only H.264 is asked for
        }
        match decoder.decode(&annex_b) {
            Ok(Some(nv12)) => {
                let (w, h, stride, rows) = (decoder.width, decoder.height, decoder.stride, decoder.rows);
                nv12_to_bgra(&nv12, w, h, stride, rows, &mut bgra);
                {
                    let mut p = PICTURE.lock().unwrap();
                    std::mem::swap(&mut p.bgra, &mut bgra);
                    p.width = w;
                    p.height = h;
                }
                let _ = unsafe { InvalidateRect(Some(HWND(hwnd as *mut _)), None, false) };
            }
            Ok(None) => {}
            Err(err) => tracing::warn!("decoding: {err}"),
        }
    }
}

/// Media Foundation's H.264 decoder, giving the latest NV12 picture.
struct Decoder {
    mft: IMFTransform,
    width: usize,
    height: usize,
    stride: usize,
    rows: usize,
}

impl Decoder {
    fn new() -> windows::core::Result<Decoder> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264DecoderMFT, None, CLSCTX_INPROC_SERVER)?;
            if let Ok(codec) = mft.cast::<ICodecAPI>() {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true)); // no frame buffering
            }
            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            mft.SetInputType(0, &input, 0)?;
            let mut decoder = Decoder { mft, width: 0, height: 0, stride: 0, rows: 0 };
            decoder.choose_output()?;
            decoder.mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            Ok(decoder)
        }
    }

    /// Picks NV12 output and reads the picture's layout (again after a stream change).
    fn choose_output(&mut self) -> windows::core::Result<()> {
        unsafe {
            for i in 0.. {
                let Ok(t) = self.mft.GetOutputAvailableType(0, i) else { break };
                if t.GetGUID(&MF_MT_SUBTYPE)? != MFVideoFormat_NV12 {
                    continue;
                }
                self.mft.SetOutputType(0, &t, 0)?;
                let size = t.GetUINT64(&MF_MT_FRAME_SIZE).unwrap_or(0);
                let (w, h) = ((size >> 32) as usize, (size & 0xFFFF_FFFF) as usize);
                self.stride = t.GetUINT32(&MF_MT_DEFAULT_STRIDE).map(|s| (s as i32).unsigned_abs() as usize).unwrap_or(w);
                self.rows = h;
                // The coded size is padded (1080 coded as 1088): show only the aperture.
                let mut area = MFVideoArea::default();
                let bytes = std::slice::from_raw_parts_mut(
                    (&mut area as *mut MFVideoArea).cast::<u8>(),
                    std::mem::size_of::<MFVideoArea>(),
                );
                if t.GetBlob(&MF_MT_MINIMUM_DISPLAY_APERTURE, bytes, None).is_ok() && area.Area.cx > 0 {
                    (self.width, self.height) = (area.Area.cx as usize, area.Area.cy as usize);
                } else {
                    (self.width, self.height) = (w, h);
                }
                self.stride = self.stride.max(self.width);
                return Ok(());
            }
            Ok(()) // no output type until the first frame's SPS arrives
        }
    }

    fn decode(&mut self, annex_b: &[u8]) -> windows::core::Result<Option<Vec<u8>>> {
        unsafe {
            let buffer = MFCreateMemoryBuffer(annex_b.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), data, annex_b.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(annex_b.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            self.mft.ProcessInput(0, &sample, 0)?;

            let mut latest = None;
            loop {
                let capacity = (self.stride * self.rows * 3 / 2).max(1920 * 1088 * 3 / 2);
                let out_buffer = MFCreateMemoryBuffer(capacity as u32)?;
                let out_sample = MFCreateSample()?;
                out_sample.AddBuffer(&out_buffer)?;
                let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                    dwStreamID: 0,
                    pSample: ManuallyDrop::new(Some(out_sample.clone())),
                    dwStatus: 0,
                    pEvents: ManuallyDrop::new(None),
                }];
                let mut status = 0;
                let result = self.mft.ProcessOutput(0, &mut buffers, &mut status);
                ManuallyDrop::drop(&mut buffers[0].pSample);
                ManuallyDrop::drop(&mut buffers[0].pEvents);
                match result {
                    Ok(()) => {
                        let contiguous = out_sample.ConvertToContiguousBuffer()?;
                        let mut ptr = std::ptr::null_mut();
                        let mut len = 0u32;
                        contiguous.Lock(&mut ptr, None, Some(&mut len))?;
                        let needed = self.stride * self.rows * 3 / 2;
                        if self.stride > 0 && len as usize >= needed {
                            latest = Some(std::slice::from_raw_parts(ptr, needed).to_vec());
                        }
                        contiguous.Unlock()?;
                    }
                    Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => self.choose_output()?,
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(err) => return Err(err),
                }
            }
            Ok(latest)
        }
    }
}
