//! Captures the default microphone through PulseAudio's simple API (PipeWire serves it too),
//! loaded at run time: 48 kHz mono float, converted by the sound server, read 10 ms at a
//! time on a thread of its own.

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use libloading::Library;

pub type SampleSink = Box<dyn FnMut(&[f32], u32) + Send>;

const PA_STREAM_RECORD: c_int = 2;
const PA_SAMPLE_FLOAT32LE: c_int = 5;
const RATE: u32 = 48_000;
/// Samples per read: 10 ms.
const CHUNK: usize = 480;

#[repr(C)]
struct SampleSpec {
    format: c_int,
    rate: u32,
    channels: u8,
}

#[repr(C)]
struct BufferAttr {
    maxlength: u32,
    tlength: u32,
    prebuf: u32,
    minreq: u32,
    fragsize: u32,
}

type New = unsafe extern "C" fn(
    *const c_char,
    *const c_char,
    c_int,
    *const c_char,
    *const c_char,
    *const SampleSpec,
    *const c_void,
    *const BufferAttr,
    *mut c_int,
) -> *mut c_void;
type Read = unsafe extern "C" fn(*mut c_void, *mut c_void, usize, *mut c_int) -> c_int;
type Free = unsafe extern "C" fn(*mut c_void);

struct Api {
    new: New,
    read: Read,
    free: Free,
    _library: Library,
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| unsafe {
        let library = Library::new("libpulse-simple.so.0").ok()?;
        Some(Api {
            new: *library.get(b"pa_simple_new\0").ok()?,
            read: *library.get(b"pa_simple_read\0").ok()?,
            free: *library.get(b"pa_simple_free\0").ok()?,
            _library: library,
        })
    })
    .as_ref()
}

/// Whether PulseAudio's (or PipeWire's) client library is installed.
pub fn available() -> bool {
    api().is_some()
}

/// Capturing while alive.
pub struct Capture {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

/// The sound server's connection, for the capture thread.
struct Simple(*mut c_void);

unsafe impl Send for Simple {}

impl Capture {
    /// Starts capturing; `sink` gets mono samples and their rate. None without a sound
    /// server or microphone.
    pub fn start(mut sink: SampleSink) -> Option<Capture> {
        let api = api()?;
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: RATE, channels: 1 };
        // Small fragments, so each read returns as soon as 10 ms are in.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: u32::MAX,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: (CHUNK * 4) as u32,
        };
        let mut error = 0;
        let simple = unsafe {
            (api.new)(
                std::ptr::null(),
                c"KVM".as_ptr(),
                PA_STREAM_RECORD,
                std::ptr::null(),
                c"Microphone for the other computer".as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            )
        };
        if simple.is_null() {
            tracing::warn!("no microphone (PulseAudio error {error})");
            return None;
        }
        let simple = Simple(simple);
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let thread = std::thread::Builder::new()
            .name("microphone".into())
            .spawn(move || {
                let simple = simple;
                let mut buffer = vec![0f32; CHUNK];
                while !flag.load(Ordering::Acquire) {
                    let mut error = 0;
                    let read = unsafe { (api.read)(simple.0, buffer.as_mut_ptr().cast(), CHUNK * 4, &mut error) };
                    if read < 0 {
                        tracing::warn!("reading the microphone: PulseAudio error {error}");
                        break;
                    }
                    sink(&buffer, RATE);
                }
                unsafe { (api.free)(simple.0) };
            })
            .ok()?;
        Some(Capture { stop, thread: Some(thread) })
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
