//! Captures the default communications microphone with WASAPI (shared mode), on a thread of
//! its own: 48 kHz mono float when Windows converts for us, else the device's own format
//! (always float in shared mode), first channel only.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    WAVEFORMATEX, eCapture, eCommunications,
};
use windows::Win32::System::Com::{CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoTaskMemFree};

const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;

pub type SampleSink = Box<dyn FnMut(&[f32], u32) + Send>;

pub fn available() -> bool {
    true
}

/// Capturing while alive.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl Capture {
    /// Starts capturing; `sink` gets mono samples and their rate. None if there's no
    /// microphone (or access to it is off in Settings > Privacy > Microphone).
    pub fn start(sink: SampleSink) -> Option<Capture> {
        let stop = Arc::new(AtomicBool::new(false));
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("microphone".into())
            .spawn(move || match open() {
                Ok(stream) => {
                    let _ = ready_tx.send(true);
                    run(stream, sink, &flag);
                }
                Err(err) => {
                    tracing::warn!("no microphone: {err}");
                    let _ = ready_tx.send(false);
                }
            })
            .ok()?;
        if ready_rx.recv().unwrap_or(false) {
            Some(Capture { stop, thread: Some(thread) })
        } else {
            let _ = thread.join();
            None
        }
    }
}

impl Drop for Capture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Stream {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    rate: u32,
    channels: usize,
}

/// 48 kHz mono float.
fn wanted_format() -> WAVEFORMATEX {
    WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_IEEE_FLOAT,
        nChannels: 1,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * 4,
        nBlockAlign: 4,
        wBitsPerSample: 32,
        cbSize: 0,
    }
}

fn open() -> windows::core::Result<Stream> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
        let enumerator: IMMDeviceEnumerator = CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)?;
        let device = enumerator.GetDefaultAudioEndpoint(eCapture, eCommunications)?;
        let buffer = 200_000; // 20 ms, in 100 ns units
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
        let wanted = wanted_format();
        let flags = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
        let (rate, channels) = if client.Initialize(AUDCLNT_SHAREMODE_SHARED, flags, buffer, 0, &wanted, None).is_ok() {
            (48_000, 1)
        } else {
            // A fresh client in the device's own format.
            let client2: IAudioClient = device.Activate(CLSCTX_ALL, None)?;
            let mix = client2.GetMixFormat()?;
            let (rate, channels, bits) = ((*mix).nSamplesPerSec, (*mix).nChannels as usize, (*mix).wBitsPerSample);
            let result = client2.Initialize(AUDCLNT_SHAREMODE_SHARED, 0, buffer, 0, mix, None);
            CoTaskMemFree(Some(mix as *const _));
            result?;
            if bits != 32 {
                return Err(windows::core::Error::new(windows::core::HRESULT(-1), "the microphone's format isn't float"));
            }
            let capture = client2.GetService()?;
            client2.Start()?;
            return Ok(Stream { client: client2, capture, rate, channels });
        };
        let capture = client.GetService()?;
        client.Start()?;
        Ok(Stream { client, capture, rate, channels })
    }
}

fn run(stream: Stream, mut sink: SampleSink, stop: &AtomicBool) {
    let mut mono = Vec::new();
    while !stop.load(Ordering::Acquire) {
        std::thread::sleep(Duration::from_millis(5));
        loop {
            let size = unsafe { stream.capture.GetNextPacketSize() }.unwrap_or(0);
            if size == 0 {
                break;
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0u32, 0u32);
            if unsafe { stream.capture.GetBuffer(&mut data, &mut frames, &mut flags, None, None) }.is_err() {
                break;
            }
            mono.clear();
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                mono.resize(frames as usize, 0.0);
            } else {
                let samples = unsafe { std::slice::from_raw_parts(data.cast::<f32>(), frames as usize * stream.channels) };
                mono.extend(samples.chunks_exact(stream.channels).map(|frame| frame[0]));
            }
            let _ = unsafe { stream.capture.ReleaseBuffer(frames) };
            sink(&mono, stream.rate);
        }
    }
    let _ = unsafe { stream.client.Stop() };
}
