//! H.264 decoding with the system's FFmpeg (libavcodec), loaded at run time, for viewing
//! the peer's screen on Linux. Not OpenH264: its decoder silently mispredicts the Mac's
//! streams (VideoToolbox's low-latency mode marks long-term references), which FFmpeg
//! decodes correctly.
//!
//! No headers: only functions, plus the leading fields of AVPacket and AVFrame, which have
//! kept their places in every FFmpeg from 4 to 8 (libavcodec 58 to 62).

use std::ffi::{c_char, c_int, c_void};
use std::sync::OnceLock;

use libloading::Library;

use crate::video::i420_to_bgra;

const AV_CODEC_ID_H264: c_int = 27;
const AV_PIX_FMT_YUV420P: c_int = 0;
const AV_PIX_FMT_YUVJ420P: c_int = 12;

/// The start of AVPacket.
#[repr(C)]
struct Packet {
    buf: *mut c_void,
    pts: i64,
    dts: i64,
    data: *mut u8,
    size: c_int,
}

/// The start of AVFrame.
#[repr(C)]
struct Frame {
    data: [*mut u8; 8],
    linesize: [c_int; 8],
    extended_data: *mut *mut u8,
    width: c_int,
    height: c_int,
    nb_samples: c_int,
    format: c_int,
}

/// The FFmpeg functions used, from one libavcodec and its libavutil.
struct Api {
    find_decoder: unsafe extern "C" fn(c_int) -> *const c_void,
    alloc_context: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    open: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int,
    free_context: unsafe extern "C" fn(*mut *mut c_void),
    packet_alloc: unsafe extern "C" fn() -> *mut Packet,
    new_packet: unsafe extern "C" fn(*mut Packet, c_int) -> c_int,
    packet_unref: unsafe extern "C" fn(*mut Packet),
    packet_free: unsafe extern "C" fn(*mut *mut Packet),
    send_packet: unsafe extern "C" fn(*mut c_void, *const Packet) -> c_int,
    receive_frame: unsafe extern "C" fn(*mut c_void, *mut Frame) -> c_int,
    frame_alloc: unsafe extern "C" fn() -> *mut Frame,
    frame_unref: unsafe extern "C" fn(*mut Frame),
    frame_free: unsafe extern "C" fn(*mut *mut Frame),
    opt_set: unsafe extern "C" fn(*mut c_void, *const c_char, *const c_char, c_int) -> c_int,
    _libraries: (Library, Library),
}

/// libavcodec majors with their libavutil's, newest first.
const VERSIONS: [(u32, u32); 5] = [(62, 60), (61, 59), (60, 58), (59, 57), (58, 56)];

fn candidates() -> Vec<(String, String)> {
    let mut list = Vec::new();
    for (codec, util) in VERSIONS {
        if cfg!(target_os = "macos") {
            // Homebrew's, for the tests.
            for dir in ["/opt/homebrew/lib", "/usr/local/lib"] {
                list.push((format!("{dir}/libavcodec.{codec}.dylib"), format!("{dir}/libavutil.{util}.dylib")));
            }
        } else {
            list.push((format!("libavcodec.so.{codec}"), format!("libavutil.so.{util}")));
        }
    }
    list
}

fn api() -> Option<&'static Api> {
    static API: OnceLock<Option<Api>> = OnceLock::new();
    API.get_or_init(|| {
        candidates().into_iter().find_map(|(codec, util)| unsafe {
            let codec = Library::new(codec).ok()?;
            let util = Library::new(util).ok()?;
            macro_rules! sym {
                ($lib:expr, $name:literal) => {
                    *$lib.get(concat!($name, "\0").as_bytes()).ok()?
                };
            }
            Some(Api {
                find_decoder: sym!(codec, "avcodec_find_decoder"),
                alloc_context: sym!(codec, "avcodec_alloc_context3"),
                open: sym!(codec, "avcodec_open2"),
                free_context: sym!(codec, "avcodec_free_context"),
                packet_alloc: sym!(codec, "av_packet_alloc"),
                new_packet: sym!(codec, "av_new_packet"),
                packet_unref: sym!(codec, "av_packet_unref"),
                packet_free: sym!(codec, "av_packet_free"),
                send_packet: sym!(codec, "avcodec_send_packet"),
                receive_frame: sym!(codec, "avcodec_receive_frame"),
                frame_alloc: sym!(util, "av_frame_alloc"),
                frame_unref: sym!(util, "av_frame_unref"),
                frame_free: sym!(util, "av_frame_free"),
                opt_set: sym!(util, "av_opt_set"),
                _libraries: (codec, util),
            })
        })
    })
    .as_ref()
}

/// Whether FFmpeg's H.264 decoder is installed.
pub fn available() -> bool {
    api().is_some_and(|api| unsafe { !(api.find_decoder)(AV_CODEC_ID_H264).is_null() })
}

/// One H.264 stream: Annex B access units in, BGRA pictures out.
pub struct Decoder {
    api: &'static Api,
    context: *mut c_void,
    packet: *mut Packet,
    frame: *mut Frame,
}

// Used from one thread at a time.
unsafe impl Send for Decoder {}

impl Decoder {
    pub fn new() -> Option<Decoder> {
        let api = api()?;
        unsafe {
            let codec = (api.find_decoder)(AV_CODEC_ID_H264);
            if codec.is_null() {
                return None;
            }
            let decoder = Decoder {
                api,
                context: (api.alloc_context)(codec),
                packet: (api.packet_alloc)(),
                frame: (api.frame_alloc)(),
            };
            if decoder.context.is_null() || decoder.packet.is_null() || decoder.frame.is_null() {
                return None;
            }
            // Each picture as soon as its frame arrives: no frame threads (each adds a
            // frame of delay) and no waiting for reordering (the encoders don't reorder).
            let set = |name: &[u8], value: &[u8]| (api.opt_set)(decoder.context, name.as_ptr().cast(), value.as_ptr().cast(), 0);
            set(b"threads\0", b"1\0");
            set(b"flags\0", b"+low_delay\0");
            if (api.open)(decoder.context, codec, std::ptr::null_mut()) < 0 {
                return None;
            }
            Some(decoder)
        }
    }

    /// Decodes one access unit into `bgra`; returns the picture's size, or None while
    /// there's none (before the first keyframe, or on errors, which FFmpeg conceals).
    pub fn decode(&mut self, annex_b: &[u8], bgra: &mut Vec<u8>) -> Option<(usize, usize)> {
        let api = self.api;
        unsafe {
            if (api.new_packet)(self.packet, annex_b.len() as c_int) < 0 {
                return None;
            }
            std::ptr::copy_nonoverlapping(annex_b.as_ptr(), (*self.packet).data, annex_b.len());
            let sent = (api.send_packet)(self.context, self.packet);
            (api.packet_unref)(self.packet);
            if sent < 0 {
                return None;
            }
            let mut size = None;
            while (api.receive_frame)(self.context, self.frame) >= 0 {
                let f = &*self.frame;
                if matches!(f.format, AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P) && f.width > 0 && f.height > 0 {
                    let (w, h) = (f.width as usize, f.height as usize);
                    let (ys, uvs) = (f.linesize[0] as usize, f.linesize[1] as usize);
                    let y = std::slice::from_raw_parts(f.data[0], ys * h);
                    let u = std::slice::from_raw_parts(f.data[1], uvs * h.div_ceil(2));
                    let v = std::slice::from_raw_parts(f.data[2], uvs * h.div_ceil(2));
                    i420_to_bgra(y, ys, u, v, uvs, w, h, bgra);
                    size = Some((w, h));
                }
                (api.frame_unref)(self.frame);
            }
            size
        }
    }
}

impl Drop for Decoder {
    fn drop(&mut self) {
        unsafe {
            (self.api.frame_free)(&mut self.frame);
            (self.api.packet_free)(&mut self.packet);
            (self.api.free_context)(&mut self.context);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::SoftwareEncoder;
    use crate::video::{H264Packager, bgra_to_nv12, unpack};

    /// The left edge of the bright square in a row of a test picture, if any.
    fn square(bgra: &[u8], width: usize, row: usize) -> Option<usize> {
        (0..width).find(|&x| {
            let px = &bgra[(row * width + x) * 4..];
            px[1] > 200 && px[0] > 150
        })
    }

    /// Our encoder's frames, through FFmpeg, come back with the square in place. Skipped
    /// where FFmpeg isn't installed (on the Mac: brew install ffmpeg), unless
    /// KVM_REQUIRE_FFMPEG is set (CI).
    #[test]
    fn decodes_openh264_stream() {
        let Some(mut decoder) = Decoder::new() else {
            assert!(std::env::var_os("KVM_REQUIRE_FFMPEG").is_none(), "FFmpeg's libavcodec didn't load");
            return;
        };
        assert!(available());
        let (w, h) = (320usize, 240usize);
        let mut encoder = SoftwareEncoder::new(w as u32, h as u32, 30, 4000).unwrap();
        let mut packager = H264Packager::default();
        let (mut src, mut nv12, mut out) = (vec![0u8; w * h * 4], Vec::new(), Vec::new());
        for frame in 0..10 {
            for (i, px) in src.chunks_mut(4).enumerate() {
                let (x, y) = (i % w, i / w);
                let inside = x >= 10 + frame * 20 && x < 50 + frame * 20 && (100..140).contains(&y);
                px.copy_from_slice(&if inside { [255, 255, 255, 255] } else { [40, (x % 200) as u8, 90, 255] });
            }
            bgra_to_nv12(&src, w as u32, h as u32, w * 4, w as u32, h as u32, &mut nv12);
            let (annex_b, key) = encoder.encode(&nv12, frame == 0).unwrap();
            let (_, _, stream) = unpack(&packager.package(&annex_b, key).unwrap()).unwrap();
            assert_eq!(decoder.decode(&stream, &mut out), Some((w, h)), "frame {frame}: no picture");
            let edge = square(&out, w, 120).map(|x| x as i64);
            assert!(edge.is_some_and(|x| (x - (10 + frame as i64 * 20)).abs() <= 2), "frame {frame}: {edge:?}");
        }
    }

    /// Payloads from the Mac's encoder (VideoToolbox, the Mac app's settings), written
    /// length-prefixed to KVM_MAC_PAYLOADS by a script; skipped without it. Frame f shows a
    /// bright square centered on column 23f (wrapping at the width) in the middle row.
    #[test]
    fn decodes_mac_payloads() {
        let Ok(path) = std::env::var("KVM_MAC_PAYLOADS") else { return };
        let mut decoder = Decoder::new().expect("FFmpeg");
        let data = std::fs::read(path).unwrap();
        let (mut rest, mut frame, mut bgra) = (&data[..], 0i64, Vec::new());
        while rest.len() >= 4 {
            let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            let (_, _, stream) = unpack(&rest[4..4 + len]).unwrap();
            rest = &rest[4 + len..];
            let (w, h) = decoder.decode(&stream, &mut bgra).unwrap_or_else(|| panic!("frame {frame}: no picture"));
            let edge = square(&bgra, w, h / 2).map(|x| x as i64);
            let want = (23 * frame % w as i64 - 59).max(0);
            assert!(edge.is_some_and(|x| (x - want).abs() <= 2), "frame {frame}: {edge:?}, not {want}");
            frame += 1;
        }
        assert!(frame > 0);
    }
}
