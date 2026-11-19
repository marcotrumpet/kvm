//! Media Foundation's H.264 encoders, fed NV12 frames, giving Annex B access units: the
//! GPU's (NVIDIA, Intel, AMD: an asynchronous MFT, driven by its events) when there is one,
//! else Microsoft's software encoder. Both low latency, constant bitrate. A hardware encoder
//! that fails, or stops giving frames, is given up on for the rest of the run.

use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Media::MediaFoundation::{
    CLSID_MSH264EncoderMFT, CODECAPI_AVEncCommonMeanBitRate, CODECAPI_AVEncCommonRateControlMode,
    CODECAPI_AVEncMPVGOPSize, CODECAPI_AVEncVideoForceKeyFrame, CODECAPI_AVLowLatencyMode, ICodecAPI, IMFActivate,
    IMFMediaEventGenerator, IMFSample, IMFTransform, MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS, METransformHaveOutput,
    METransformNeedInput, MF_E_NO_EVENTS_AVAILABLE, MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
    MF_LOW_LATENCY, MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_TRANSFORM_ASYNC_UNLOCK, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFSampleExtension_CleanPoint, MFT_CATEGORY_VIDEO_ENCODER,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_FRIENDLY_NAME_Attribute, MFT_MESSAGE_NOTIFY_BEGIN_STREAMING,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_REGISTER_TYPE_INFO, MFTEnumEx, MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
    eAVEncCommonRateControlMode_CBR, eAVEncH264VProfile_Main,
};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance, CoTaskMemFree};
use windows::Win32::System::Variant::VARIANT;
use windows::core::{Interface, PWSTR};

/// Set once a hardware encoder failed: software from then on.
static HARDWARE_FAILED: AtomicBool = AtomicBool::new(false);

/// Encoded access units: Annex B, and whether each is a keyframe.
type Frames = Vec<(Vec<u8>, bool)>;

/// A hardware encoder giving nothing for this many frames in a row has stalled.
const STALLED_AFTER: u32 = 30;

pub(super) struct Encoder {
    mft: IMFTransform,
    codec: Option<ICodecAPI>,
    /// The hardware encoder's events (None: the synchronous software encoder).
    events: Option<IMFMediaEventGenerator>,
    /// Inputs the hardware encoder asked for and hasn't had yet.
    wanted: u32,
    /// Encoded frames without anything coming out.
    silent: u32,
    provides_samples: bool,
    frame_len: usize,
    duration: i64,
    time: i64,
}

fn pack(hi: u32, lo: u32) -> u64 {
    (hi as u64) << 32 | lo as u64
}

impl Encoder {
    /// The GPU's encoder if there's one that works, else the software one.
    pub(super) fn new(width: u32, height: u32, fps: u32, kbps: u32) -> windows::core::Result<Encoder> {
        let software_only = HARDWARE_FAILED.load(Ordering::Acquire) || std::env::var_os("KVM_SOFTWARE_ENCODER").is_some();
        if !software_only {
            match hardware().and_then(|(mft, name)| Ok((Encoder::configure(mft, true, width, height, fps, kbps)?, name))) {
                Ok((encoder, name)) => {
                    tracing::info!("encoding with {name}");
                    return Ok(encoder);
                }
                Err(err) => {
                    tracing::info!("no hardware encoder ({err}): encoding in software");
                    HARDWARE_FAILED.store(true, Ordering::Release);
                }
            }
        }
        let mft: IMFTransform = unsafe { CoCreateInstance(&CLSID_MSH264EncoderMFT, None, CLSCTX_INPROC_SERVER)? };
        Encoder::configure(mft, false, width, height, fps, kbps)
    }

    pub(super) fn is_hardware(&self) -> bool {
        self.events.is_some()
    }

    /// Gives up on hardware encoding for the rest of the run.
    pub(super) fn hardware_failed() {
        HARDWARE_FAILED.store(true, Ordering::Release);
    }

    fn configure(mft: IMFTransform, hardware: bool, width: u32, height: u32, fps: u32, kbps: u32) -> windows::core::Result<Encoder> {
        unsafe {
            let events = if hardware {
                let attributes = mft.GetAttributes()?;
                attributes.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
                let _ = attributes.SetUINT32(&MF_LOW_LATENCY, 1);
                Some(mft.cast::<IMFMediaEventGenerator>()?)
            } else {
                None
            };
            // Output first: H.264 encoders only take an input type once they have it.
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
            let provides_samples = mft.GetOutputStreamInfo(0)?.dwFlags & MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 != 0;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            mft.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            Ok(Encoder {
                mft,
                codec,
                events,
                wanted: 0,
                silent: 0,
                provides_samples,
                frame_len: (width * height * 3 / 2) as usize,
                duration: 10_000_000 / fps as i64,
                time: 0,
            })
        }
    }

    pub(super) fn set_bitrate(&self, kbps: u32) {
        if let Some(codec) = &self.codec {
            let _ = unsafe { codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &VARIANT::from(kbps * 1000)) };
        }
    }

    /// Encodes one frame; returns what the encoder has ready (Annex B, keyframe). A hardware
    /// encoder that isn't ready for the frame skips it (false: ask for the keyframe again).
    pub(super) fn encode(&mut self, nv12: &[u8], keyframe: bool) -> windows::core::Result<(Frames, bool)> {
        let mut out = Vec::new();
        let sample = self.sample(nv12)?;
        if self.events.is_none() {
            self.force_keyframe(keyframe);
            unsafe { self.mft.ProcessInput(0, &sample, 0)? };
            self.drain(&mut out)?;
            return Ok((out, true));
        }
        // Asynchronous: take what's waiting, wait (briefly) to be asked for input, give the
        // frame, then wait a little for it to come out (low latency: one in, one out).
        self.pump(&mut out, Duration::ZERO, false)?;
        if self.wanted == 0 {
            self.pump(&mut out, Duration::from_millis(100), true)?;
        }
        if self.wanted == 0 {
            return Ok((out, false));
        }
        self.wanted -= 1;
        self.force_keyframe(keyframe);
        unsafe { self.mft.ProcessInput(0, &sample, 0)? };
        let before = out.len();
        self.pump(&mut out, Duration::from_millis(30), false)?;
        self.silent = if out.len() > before { 0 } else { self.silent + 1 };
        if self.silent >= STALLED_AFTER {
            return Err(windows::core::Error::new(windows::core::HRESULT(-1), "the hardware encoder stopped giving frames"));
        }
        Ok((out, true))
    }

    fn force_keyframe(&self, keyframe: bool) {
        if keyframe && let Some(codec) = &self.codec {
            let _ = unsafe { codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &VARIANT::from(1u32)) };
        }
    }

    fn sample(&mut self, nv12: &[u8]) -> windows::core::Result<IMFSample> {
        unsafe {
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
            Ok(sample)
        }
    }

    /// Handles the hardware encoder's events for up to `wait`: counts input requests, takes
    /// output. Returns at the first output (or, with `until_input`, the first request).
    fn pump(&mut self, out: &mut Frames, wait: Duration, until_input: bool) -> windows::core::Result<()> {
        let Some(events) = self.events.clone() else { return Ok(()) };
        let deadline = Instant::now() + wait;
        loop {
            match unsafe { events.GetEvent(MEDIA_EVENT_GENERATOR_GET_EVENT_FLAGS(1)) } {
                // MF_EVENT_FLAG_NO_WAIT
                Ok(event) => {
                    let kind = unsafe { event.GetType()? };
                    if kind == METransformNeedInput.0 as u32 {
                        self.wanted += 1;
                        if until_input {
                            return Ok(());
                        }
                    } else if kind == METransformHaveOutput.0 as u32 {
                        let before = out.len();
                        self.output(out)?;
                        if out.len() > before && !until_input && !wait.is_zero() {
                            return Ok(());
                        }
                    }
                }
                Err(err) if err.code() == MF_E_NO_EVENTS_AVAILABLE => {
                    if Instant::now() >= deadline {
                        return Ok(());
                    }
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(err) => return Err(err),
            }
        }
    }

    /// The software encoder: everything it has ready.
    fn drain(&mut self, out: &mut Frames) -> windows::core::Result<()> {
        while self.output(out)? {}
        Ok(())
    }

    /// One ProcessOutput; false when the encoder needs more input.
    fn output(&mut self, out: &mut Frames) -> windows::core::Result<bool> {
        unsafe {
            let ours = if self.provides_samples {
                None
            } else {
                let info = self.mft.GetOutputStreamInfo(0)?;
                let buffer = MFCreateMemoryBuffer((info.cbSize as usize).max(self.frame_len) as u32)?;
                let sample = MFCreateSample()?;
                sample.AddBuffer(&buffer)?;
                Some(sample)
            };
            let mut buffers = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(ours),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.mft.ProcessOutput(0, &mut buffers, &mut status);
            let sample = ManuallyDrop::take(&mut buffers[0].pSample);
            ManuallyDrop::drop(&mut buffers[0].pEvents);
            match result {
                Ok(()) => {
                    let Some(sample) = sample else { return Ok(true) };
                    let contiguous = sample.ConvertToContiguousBuffer()?;
                    let mut ptr = std::ptr::null_mut();
                    let mut len = 0u32;
                    contiguous.Lock(&mut ptr, None, Some(&mut len))?;
                    let bytes = std::slice::from_raw_parts(ptr, len as usize).to_vec();
                    contiguous.Unlock()?;
                    let key = sample.GetUINT32(&MFSampleExtension_CleanPoint).unwrap_or(0) != 0;
                    if !bytes.is_empty() {
                        out.push((bytes, key));
                    }
                    Ok(true)
                }
                Err(err) if err.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => Ok(false),
                Err(err) if err.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    // The encoder revised its output type: take it as offered.
                    let offered = self.mft.GetOutputAvailableType(0, 0)?;
                    self.mft.SetOutputType(0, &offered, 0)?;
                    Ok(true)
                }
                Err(err) => Err(err),
            }
        }
    }
}

/// The first hardware H.264 encoder taking NV12, by Media Foundation's ranking, and its name.
fn hardware() -> windows::core::Result<(IMFTransform, String)> {
    unsafe {
        let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
        let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
        let mut list: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count = 0u32;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER,
            Some(&input),
            Some(&output),
            &mut list,
            &mut count,
        )?;
        let activates: Vec<Option<IMFActivate>> =
            (0..count as usize).map(|i| std::ptr::replace(list.add(i), None)).collect();
        if !list.is_null() {
            CoTaskMemFree(Some(list as *const _));
        }
        let mut last = windows::core::Error::new(windows::core::HRESULT(-1), "none installed");
        for activate in activates.into_iter().flatten() {
            let name = {
                let mut text = PWSTR::null();
                let mut len = 0;
                if activate.GetAllocatedString(&MFT_FRIENDLY_NAME_Attribute, &mut text, &mut len).is_ok() {
                    let name = text.to_string().unwrap_or_default();
                    CoTaskMemFree(Some(text.0 as *const _));
                    name
                } else {
                    "the hardware encoder".into()
                }
            };
            match activate.ActivateObject::<IMFTransform>() {
                Ok(mft) => return Ok((mft, name)),
                Err(err) => last = err,
            }
        }
        Err(last)
    }
}
