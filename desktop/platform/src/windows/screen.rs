//! Shares this screen with a viewer: DXGI Desktop Duplication captures a display (the
//! pointer, which it leaves out, is drawn in from its shape), the picture is scaled to NV12
//! and encoded with Media Foundation's H.264 encoder (low latency, constant bitrate), and
//! each frame is packaged for the viewers ([`crate::video`]). One thread per share.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::{
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAP_READ, D3D11_MAPPED_SUBRESOURCE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING, D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTDUPL_POINTER_SHAPE_INFO, IDXGIFactory1,
    IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
};
use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFTransform,
    MF_E_TRANSFORM_NEED_MORE_INPUT, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
    MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_VERSION, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSTARTUP_LITE, MFSampleExtension_CleanPoint, MFStartup,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFVideoFormat_H264,
    MFVideoFormat_NV12, MFVideoInterlace_Progressive, eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx};
use windows::Win32::System::Variant::VARIANT;
use windows::core::Interface;

use crate::video::{Display, H264Packager, ScreenParams, bgra_to_nv12, fit};

/// Media Foundation's software encoder tops out at H.264 level 5.1.
const MAX_WIDTH: u16 = 4096;
const MAX_HEIGHT: u16 = 2304;

pub fn supported() -> bool {
    true
}

/// This machine's displays; ids count outputs across adapters, from 1.
pub fn displays() -> Vec<Display> {
    let mut list = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else { return list };
    let mut id = 0;
    for a in 0.. {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(a) }) else { break };
        for o in 0.. {
            let Ok(output) = (unsafe { adapter.EnumOutputs(o) }) else { break };
            id += 1;
            let Ok(desc) = (unsafe { output.GetDesc() }) else { continue };
            if !desc.AttachedToDesktop.as_bool() {
                continue;
            }
            let r = desc.DesktopCoordinates;
            let name_len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(desc.DeviceName.len());
            let device = String::from_utf16_lossy(&desc.DeviceName[..name_len]);
            list.push(Display {
                id,
                name: format!("Display {}", device.trim_start_matches("\\\\.\\DISPLAY")),
                left: r.left,
                top: r.top,
                width: (r.right - r.left).max(1) as u32,
                height: (r.bottom - r.top).max(1) as u32,
                main: r.left == 0 && r.top == 0,
            });
        }
    }
    list
}

/// The display `id` names (0: the main one).
pub fn display(id: u32) -> Option<Display> {
    let all = displays();
    all.iter().find(|d| if id == 0 { d.main } else { d.id == id }).or(all.first()).cloned()
}

/// Sends one packaged frame; false if the link refused it (the next one is then a keyframe).
pub type FrameSink = Box<dyn FnMut(Vec<u8>, bool) -> bool + Send>;

struct Shared {
    params: Mutex<ScreenParams>,
    bitrate_kbps: AtomicU32,
    keyframe: AtomicBool,
    stop: AtomicBool,
}

/// A running share. Dropping it stops capturing.
pub struct ScreenHost {
    shared: Arc<Shared>,
    thread: Option<JoinHandle<()>>,
}

impl ScreenHost {
    pub fn start(params: ScreenParams, sink: FrameSink) -> ScreenHost {
        let shared = Arc::new(Shared {
            params: Mutex::new(params),
            bitrate_kbps: AtomicU32::new(params.bitrate_kbps.min(12_000)),
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

    /// New viewer settings (size, display, frame rate): applied in place.
    pub fn update(&self, params: ScreenParams) {
        *self.shared.params.lock().unwrap() = params;
        self.shared.keyframe.store(true, Ordering::Release);
    }

    /// From the core's rate control.
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
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        if let Err(err) = MFStartup(MF_VERSION, MFSTARTUP_LITE) {
            tracing::warn!("Media Foundation: {err}");
            return;
        }
    }
    while !shared.stop.load(Ordering::Acquire) {
        if let Err(err) = share(&shared, &mut sink) {
            tracing::warn!("screen sharing: {err}; retrying");
            std::thread::sleep(Duration::from_millis(500)); // e.g. the secure desktop, a mode change
        }
    }
    tracing::info!("screen sharing stopped");
}

/// Captures and encodes until stopped or the settings need a new capture or encoder.
fn share(shared: &Shared, sink: &mut FrameSink) -> windows::core::Result<()> {
    let params = *shared.params.lock().unwrap();
    let mut capture = Capture::open(params.display)?;
    let (width, height) =
        fit(capture.width, capture.height, params.max_width.min(MAX_WIDTH), params.max_height.min(MAX_HEIGHT));
    let fps = params.fps.clamp(1, 60) as u32;
    let mut kbps = shared.bitrate_kbps.load(Ordering::Acquire);
    let mut encoder = Encoder::new(width, height, fps, kbps)?;
    tracing::info!(
        "sharing display {} ({}x{}) as {width}x{height} @ {fps} fps, {kbps} kbit/s",
        params.display, capture.width, capture.height
    );
    let mut packager = H264Packager::default();
    let mut nv12 = Vec::new();
    let mut have_frame = false;
    let interval = Duration::from_secs(1) / fps;
    let mut last_sent = Instant::now() - interval;
    loop {
        if shared.stop.load(Ordering::Acquire) || *shared.params.lock().unwrap() != params {
            return Ok(());
        }
        let wanted = shared.bitrate_kbps.load(Ordering::Acquire);
        if wanted != kbps {
            kbps = wanted;
            encoder.set_bitrate(kbps);
        }
        let changed = capture.next(interval.as_millis().max(1) as u32)?;
        have_frame |= changed;
        let keyframe = shared.keyframe.load(Ordering::Acquire);
        if !have_frame || !(changed || keyframe) {
            continue;
        }
        // Not faster than the viewer asked for.
        let wait = interval.saturating_sub(last_sent.elapsed());
        if !wait.is_zero() {
            std::thread::sleep(wait);
        }
        let picture = capture.picture_with_pointer();
        bgra_to_nv12(&picture, capture.width, capture.height, capture.width as usize * 4, width, height, &mut nv12);
        if keyframe {
            shared.keyframe.store(false, Ordering::Release);
        }
        for (annex_b, key) in encoder.encode(&nv12, keyframe)? {
            if let Some(payload) = packager.package(&annex_b, key)
                && !sink(payload, key)
            {
                shared.keyframe.store(true, Ordering::Release); // the viewer missed one
            }
        }
        last_sent = Instant::now();
    }
}

/// One display, captured into a BGRA buffer, with the pointer's position and shape.
struct Capture {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: Option<ID3D11Texture2D>,
    width: u32,
    height: u32,
    frame: Vec<u8>,
    pointer: Pointer,
}

#[derive(Default)]
struct Pointer {
    visible: bool,
    x: i32,
    y: i32,
    shape: Vec<u8>,
    info: DXGI_OUTDUPL_POINTER_SHAPE_INFO,
}

impl Capture {
    fn open(display_id: u32) -> windows::core::Result<Capture> {
        let target = display(display_id).map(|d| d.id).unwrap_or(1);
        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()?;
            let mut id = 0;
            for a in 0.. {
                let adapter = factory.EnumAdapters1(a)?;
                for o in 0.. {
                    let Ok(output) = adapter.EnumOutputs(o) else { break };
                    id += 1;
                    if id != target {
                        continue;
                    }
                    let desc = output.GetDesc()?;
                    let r = desc.DesktopCoordinates;
                    let mut device = None;
                    let mut context = None;
                    D3D11CreateDevice(
                        &adapter,
                        D3D_DRIVER_TYPE_UNKNOWN,
                        HMODULE::default(),
                        D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                        None,
                        D3D11_SDK_VERSION,
                        Some(&mut device),
                        None,
                        Some(&mut context),
                    )?;
                    let (device, context) = (device.unwrap(), context.unwrap());
                    let duplication = output.cast::<IDXGIOutput1>()?.DuplicateOutput(&device)?;
                    let (width, height) = ((r.right - r.left) as u32, (r.bottom - r.top) as u32);
                    return Ok(Capture {
                        _device: device,
                        context,
                        duplication,
                        staging: None,
                        width,
                        height,
                        frame: vec![0; (width * height * 4) as usize],
                        pointer: Pointer::default(),
                    });
                }
            }
        }
        Err(windows::core::Error::from_hresult(windows::Win32::Foundation::E_FAIL))
    }

    /// Waits up to `timeout_ms` for a new picture or pointer move; true if there was one.
    fn next(&mut self, timeout_ms: u32) -> windows::core::Result<bool> {
        let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource: Option<IDXGIResource> = None;
        unsafe {
            match self.duplication.AcquireNextFrame(timeout_ms, &mut info, &mut resource) {
                Ok(()) => {}
                Err(err) if err.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(false),
                Err(err) => return Err(err),
            }
            let result = self.read(&info, resource);
            let _ = self.duplication.ReleaseFrame();
            result
        }
    }

    unsafe fn read(&mut self, info: &DXGI_OUTDUPL_FRAME_INFO, resource: Option<IDXGIResource>) -> windows::core::Result<bool> {
        let mut changed = false;
        if info.LastPresentTime != 0
            && let Some(resource) = resource
        {
            let texture: ID3D11Texture2D = resource.cast()?;
            let staging = match &self.staging {
                Some(s) => s.clone(),
                None => {
                    let mut desc = D3D11_TEXTURE2D_DESC::default();
                    unsafe { texture.GetDesc(&mut desc) };
                    desc.Usage = D3D11_USAGE_STAGING;
                    desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
                    desc.BindFlags = 0;
                    desc.MiscFlags = 0;
                    desc.MipLevels = 1;
                    desc.ArraySize = 1;
                    let mut staging = None;
                    unsafe { self._device.CreateTexture2D(&desc, None, Some(&mut staging))? };
                    let staging = staging.unwrap();
                    self.staging = Some(staging.clone());
                    staging
                }
            };
            unsafe {
                self.context.CopyResource(&staging, &texture);
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                self.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
                let row = self.width as usize * 4;
                for y in 0..self.height as usize {
                    let src = std::slice::from_raw_parts((mapped.pData as *const u8).add(y * mapped.RowPitch as usize), row);
                    self.frame[y * row..(y + 1) * row].copy_from_slice(src);
                }
                self.context.Unmap(&staging, 0);
            }
            changed = true;
        }
        if info.LastMouseUpdateTime != 0 {
            let p = info.PointerPosition;
            self.pointer.visible = p.Visible.as_bool();
            self.pointer.x = p.Position.x;
            self.pointer.y = p.Position.y;
            changed = true;
        }
        if info.PointerShapeBufferSize > 0 {
            self.pointer.shape.resize(info.PointerShapeBufferSize as usize, 0);
            let mut needed = 0;
            unsafe {
                self.duplication.GetFramePointerShape(
                    info.PointerShapeBufferSize,
                    self.pointer.shape.as_mut_ptr().cast(),
                    &mut needed,
                    &mut self.pointer.info,
                )?;
            }
            changed = true;
        }
        Ok(changed)
    }

    /// The picture with the pointer drawn in.
    fn picture_with_pointer(&self) -> std::borrow::Cow<'_, [u8]> {
        let p = &self.pointer;
        if !p.visible || p.shape.is_empty() {
            return std::borrow::Cow::Borrowed(&self.frame);
        }
        let mut out = self.frame.clone();
        draw_pointer(&mut out, self.width, self.height, p);
        std::borrow::Cow::Owned(out)
    }
}

/// Pointer shapes: 1 monochrome (AND mask over XOR mask, 1 bit each), 2 color (BGRA, alpha
/// blended), 4 masked color (BGRA; alpha 0xFF means XOR with the screen).
fn draw_pointer(frame: &mut [u8], width: u32, height: u32, p: &Pointer) {
    let info = &p.info;
    let rows = if info.Type == 1 { info.Height / 2 } else { info.Height };
    for sy in 0..rows {
        let y = p.y + sy as i32;
        if y < 0 || y >= height as i32 {
            continue;
        }
        for sx in 0..info.Width {
            let x = p.x + sx as i32;
            if x < 0 || x >= width as i32 {
                continue;
            }
            let dst = (y as usize * width as usize + x as usize) * 4;
            match info.Type {
                1 => {
                    let byte = (sx / 8) as usize;
                    let bit = 0x80 >> (sx % 8);
                    let and = p.shape[sy as usize * info.Pitch as usize + byte] & bit != 0;
                    let xor = p.shape[(sy + rows) as usize * info.Pitch as usize + byte] & bit != 0;
                    for c in 0..3 {
                        let v = frame[dst + c];
                        frame[dst + c] = match (and, xor) {
                            (false, false) => 0,
                            (false, true) => 255,
                            (true, false) => v,
                            (true, true) => !v,
                        };
                    }
                }
                2 | 4 => {
                    let src = sy as usize * info.Pitch as usize + sx as usize * 4;
                    let (b, g, r, a) = (p.shape[src], p.shape[src + 1], p.shape[src + 2], p.shape[src + 3]);
                    if info.Type == 4 {
                        if a == 0xFF {
                            frame[dst] ^= b;
                            frame[dst + 1] ^= g;
                            frame[dst + 2] ^= r;
                        } else {
                            frame[dst..dst + 3].copy_from_slice(&[b, g, r]);
                        }
                    } else {
                        let a = a as u32;
                        for (c, v) in [b, g, r].into_iter().enumerate() {
                            frame[dst + c] = ((v as u32 * a + frame[dst + c] as u32 * (255 - a)) / 255) as u8;
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Media Foundation's H.264 encoder, fed NV12 frames, giving Annex B access units.
struct Encoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    frame_len: usize,
    duration: i64,
    time: i64,
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

impl Encoder {
    fn new(width: u32, height: u32, fps: u32, kbps: u32) -> windows::core::Result<Encoder> {
        unsafe {
            let mft: IMFTransform = CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)?;
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, kbps * 1000)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            output.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_Main.0 as u32)?;
            mft.SetOutputType(0, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, pack(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, pack(fps, 1))?;
            input.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack(1, 1))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
            mft.SetInputType(0, &input, 0)?;

            let codec = mft.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                // No frame reordering or lookahead: each frame comes out as soon as it goes in.
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &VARIANT::from(true));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonRateControlMode, &VARIANT::from(eAVEncCommonRateControlMode_CBR.0 as u32));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(kbps * 1000));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &VARIANT::from(fps * 10));
            }
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Encoder {
                mft,
                codec,
                frame_len: (width * height * 3 / 2) as usize,
                duration: 10_000_000 / fps as i64,
                time: 0,
            })
        }
    }

    fn set_bitrate(&self, kbps: u32) {
        if let Some(codec) = &self.codec {
            let _ = unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(kbps * 1000)) };
        }
    }

    /// Encodes one frame; returns what the encoder has ready (Annex B, keyframe).
    fn encode(&mut self, nv12: &[u8], keyframe: bool) -> windows::core::Result<Vec<(Vec<u8>, bool)>> {
        unsafe {
            if keyframe && let Some(codec) = &self.codec {
                let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32));
            }
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.time)?;
            sample.SetSampleDuration(self.duration)?;
            self.time += self.duration;
            self.mft.ProcessInput(0, &sample, 0)?;

            let mut out = Vec::new();
            loop {
                let info = self.mft.GetOutputStreamInfo(0)?;
                let capacity = (info.cbSize as usize).max(self.frame_len);
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
                        let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                        contiguous.Unlock()?;
                        let key = out_sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                        out.push((bytes, key));
                    }
                    Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                    Err(err) => return Err(err),
                }
            }
            Ok(out)
        }
    }
}
