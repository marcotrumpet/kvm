//! The microphone through PulseAudio's simple API (PipeWire serves it too), loaded at run
//! time. Capture: the default microphone as 48 kHz mono float, converted by the sound server,
//! read 10 ms at a time on a thread of its own. Playback: the Mac's microphone into a "KVM
//! Microphone Feed" null sink, whose monitor is remapped into the "KVM Microphone" source
//! that call apps can choose (made with pactl).

use std::ffi::{c_char, c_int, c_void};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::JoinHandle;

use libloading::Library;

pub type SampleSink = Box<dyn FnMut(&[f32], u32) + Send>;

const PA_STREAM_PLAYBACK: c_int = 1;
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
type Write = unsafe extern "C" fn(*mut c_void, *const c_void, usize, *mut c_int) -> c_int;
type Latency = unsafe extern "C" fn(*mut c_void, *mut c_int) -> u64;
type Free = unsafe extern "C" fn(*mut c_void);

struct Api {
    new: New,
    read: Read,
    write: Write,
    latency: Latency,
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
            write: *library.get(b"pa_simple_write\0").ok()?,
            latency: *library.get(b"pa_simple_get_latency\0").ok()?,
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

const FEED_SINK: &str = "kvm_microphone_feed";
const SOURCE: &str = "kvm_microphone";
/// Packets arriving while this much is queued are dropped, so delay can't build up.
const MAX_QUEUED_USEC: u64 = 150_000;

/// The "KVM Microphone" source and the sink feeding it, while alive.
pub struct VirtualMicrophone {
    modules: Vec<u32>,
}

fn pactl(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("pactl").args(args).output().ok()?;
    out.status.success().then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

impl VirtualMicrophone {
    /// Makes the devices (or takes over ones left by an earlier run). None without pactl.
    pub fn create() -> Option<VirtualMicrophone> {
        let listed = pactl(&["list", "short", "modules"])?;
        let ours: Vec<u32> = listed
            .lines()
            .filter(|line| line.contains(FEED_SINK) || line.contains(&format!("source_name={SOURCE}")))
            .filter_map(|line| line.split_whitespace().next()?.parse().ok())
            .collect();
        if ours.len() == 2 {
            return Some(VirtualMicrophone { modules: ours });
        }
        for module in ours {
            let _ = pactl(&["unload-module", &module.to_string()]);
        }
        let sink = pactl(&[
            "load-module",
            "module-null-sink",
            &format!("sink_name={FEED_SINK}"),
            "rate=48000",
            "channels=1",
            "sink_properties=device.description=\"KVM Microphone Feed\"",
        ])?;
        let mut devices = VirtualMicrophone { modules: vec![sink.trim().parse().ok()?] };
        let source = pactl(&[
            "load-module",
            "module-remap-source",
            &format!("master={FEED_SINK}.monitor"),
            &format!("source_name={SOURCE}"),
            "source_properties=device.description=\"KVM Microphone\"",
        ])?;
        devices.modules.push(source.trim().parse().ok()?);
        Some(devices)
    }
}

impl Drop for VirtualMicrophone {
    fn drop(&mut self) {
        for module in self.modules.iter().rev() {
            let _ = pactl(&["unload-module", &module.to_string()]);
        }
    }
}

/// Plays 48 kHz mono samples into the feed, on a thread of its own, while alive.
pub struct Playback {
    samples: Option<std::sync::mpsc::Sender<Vec<f32>>>,
    thread: Option<JoinHandle<()>>,
}

impl Playback {
    pub fn start() -> Option<Playback> {
        let api = api()?;
        let spec = SampleSpec { format: PA_SAMPLE_FLOAT32LE, rate: RATE, channels: 1 };
        // About 40 ms buffered: enough for the network's jitter.
        let attr = BufferAttr {
            maxlength: u32::MAX,
            tlength: (RATE as usize / 25 * 4) as u32,
            prebuf: u32::MAX,
            minreq: u32::MAX,
            fragsize: u32::MAX,
        };
        let device = std::ffi::CString::new(FEED_SINK).ok()?;
        let mut error = 0;
        let simple = unsafe {
            (api.new)(
                std::ptr::null(),
                c"KVM".as_ptr(),
                PA_STREAM_PLAYBACK,
                device.as_ptr(),
                c"Microphone from the other computer".as_ptr(),
                &spec,
                std::ptr::null(),
                &attr,
                &mut error,
            )
        };
        if simple.is_null() {
            tracing::warn!("can't play into the KVM Microphone (PulseAudio error {error})");
            return None;
        }
        let simple = Simple(simple);
        let (tx, rx) = std::sync::mpsc::channel::<Vec<f32>>();
        let thread = std::thread::Builder::new()
            .name("microphone feed".into())
            .spawn(move || {
                let simple = simple;
                while let Ok(samples) = rx.recv() {
                    let mut error = 0;
                    if unsafe { (api.latency)(simple.0, &mut error) } > MAX_QUEUED_USEC {
                        continue; // running late: skip
                    }
                    if unsafe { (api.write)(simple.0, samples.as_ptr().cast(), samples.len() * 4, &mut error) } < 0 {
                        tracing::warn!("playing the microphone: PulseAudio error {error}");
                        break;
                    }
                }
                unsafe { (api.free)(simple.0) };
            })
            .ok()?;
        Some(Playback { samples: Some(tx), thread: Some(thread) })
    }

    pub fn play(&self, samples: Vec<f32>) {
        if let Some(tx) = &self.samples {
            let _ = tx.send(samples);
        }
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        self.samples.take(); // ends the thread's loop
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
