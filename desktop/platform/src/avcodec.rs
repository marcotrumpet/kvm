//! Decoding with the system's FFmpeg (libavcodec), loaded at run time, on Linux: H.264 for
//! viewing the peer's screen, AAC-ELD for the Mac's microphone. Not OpenH264 for video: its
//! decoder silently mispredicts the Mac's streams (VideoToolbox's low-latency mode marks
//! long-term references), which FFmpeg decodes correctly.
//!
//! No headers: only functions, plus the leading fields of AVPacket, AVFrame and
//! AVCodecParameters, which have kept their places in every FFmpeg from 4 to 8
//! (libavcodec 58 to 62).

use std::ffi::{CStr, c_char, c_int, c_void};
use std::sync::OnceLock;

use libloading::Library;

use crate::video::i420_to_bgra;

const AV_CODEC_ID_H264: c_int = 27;
const AV_CODEC_ID_AAC: c_int = 86018;
const AVMEDIA_TYPE_AUDIO: c_int = 1;
const AV_PIX_FMT_YUV420P: c_int = 0;
const AV_PIX_FMT_YUVJ420P: c_int = 12;
const AV_SAMPLE_FMT_FLTP: c_int = 8;
/// Zeroed bytes FFmpeg wants after extradata.
const PADDING: usize = 64;

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

/// The start of AVCodecParameters.
#[repr(C)]
struct Parameters {
    codec_type: c_int,
    codec_id: c_int,
    codec_tag: u32,
    extradata: *mut u8,
    extradata_size: c_int,
}

/// The FFmpeg functions used, from one libavcodec and its libavutil.
struct Api {
    find_decoder: unsafe extern "C" fn(c_int) -> *const c_void,
    alloc_context: unsafe extern "C" fn(*const c_void) -> *mut c_void,
    open: unsafe extern "C" fn(*mut c_void, *const c_void, *mut c_void) -> c_int,
    free_context: unsafe extern "C" fn(*mut *mut c_void),
    parameters_alloc: unsafe extern "C" fn() -> *mut Parameters,
    parameters_to_context: unsafe extern "C" fn(*mut c_void, *const Parameters) -> c_int,
    parameters_free: unsafe extern "C" fn(*mut *mut Parameters),
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
    mallocz: unsafe extern "C" fn(usize) -> *mut c_void,
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
                parameters_alloc: sym!(codec, "avcodec_parameters_alloc"),
                parameters_to_context: sym!(codec, "avcodec_parameters_to_context"),
                parameters_free: sym!(codec, "avcodec_parameters_free"),
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
                mallocz: sym!(util, "av_mallocz"),
                _libraries: (codec, util),
            })
        })
    })
    .as_ref()
}

fn has_decoder(id: c_int) -> bool {
    api().is_some_and(|api| unsafe { !(api.find_decoder)(id).is_null() })
}

/// Whether FFmpeg's H.264 decoder is installed.
pub fn available() -> bool {
    has_decoder(AV_CODEC_ID_H264)
}

/// Whether FFmpeg's AAC decoder is installed.
pub fn aac_available() -> bool {
    has_decoder(AV_CODEC_ID_AAC)
}

/// An open FFmpeg decoder, with a packet and a frame to use with it.
struct Codec {
    api: &'static Api,
    context: *mut c_void,
    packet: *mut Packet,
    frame: *mut Frame,
}

// Used from one thread at a time.
unsafe impl Send for Codec {}

impl Codec {
    /// Opens the decoder for `id`, with `extradata` (the stream's configuration) and options.
    fn open(id: c_int, extradata: Option<&[u8]>, options: &[(&CStr, &CStr)]) -> Option<Codec> {
        let api = api()?;
        unsafe {
            let decoder = (api.find_decoder)(id);
            if decoder.is_null() {
                return None;
            }
            // Owned from here, so dropped (and freed) on any failure.
            let codec = Codec {
                api,
                context: (api.alloc_context)(decoder),
                packet: (api.packet_alloc)(),
                frame: (api.frame_alloc)(),
            };
            if codec.context.is_null() || codec.packet.is_null() || codec.frame.is_null() {
                return None;
            }
            if let Some(extra) = extradata {
                let mut params = (api.parameters_alloc)();
                if params.is_null() {
                    return None;
                }
                let bytes = (api.mallocz)(extra.len() + PADDING).cast::<u8>(); // the parameters own it
                if !bytes.is_null() {
                    std::ptr::copy_nonoverlapping(extra.as_ptr(), bytes, extra.len());
                    (*params).codec_type = AVMEDIA_TYPE_AUDIO;
                    (*params).codec_id = id;
                    (*params).extradata = bytes;
                    (*params).extradata_size = extra.len() as c_int;
                }
                let copied = !bytes.is_null() && (api.parameters_to_context)(codec.context, params) >= 0;
                (api.parameters_free)(&mut params);
                if !copied {
                    return None;
                }
            }
            for (name, value) in options {
                (api.opt_set)(codec.context, name.as_ptr(), value.as_ptr(), 0);
            }
            if (api.open)(codec.context, decoder, std::ptr::null_mut()) < 0 {
                return None;
            }
            Some(codec)
        }
    }

    /// Sends one packet and hands each frame it gives to `each`. False if it was refused.
    fn decode(&mut self, data: &[u8], mut each: impl FnMut(&Frame)) -> bool {
        let api = self.api;
        unsafe {
            if (api.new_packet)(self.packet, data.len() as c_int) < 0 {
                return false;
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), (*self.packet).data, data.len());
            let sent = (api.send_packet)(self.context, self.packet);
            (api.packet_unref)(self.packet);
            if sent < 0 {
                return false;
            }
            while (api.receive_frame)(self.context, self.frame) >= 0 {
                each(&*self.frame);
                (api.frame_unref)(self.frame);
            }
            true
        }
    }
}

impl Drop for Codec {
    fn drop(&mut self) {
        unsafe {
            (self.api.frame_free)(&mut self.frame);
            (self.api.packet_free)(&mut self.packet);
            (self.api.free_context)(&mut self.context);
        }
    }
}

/// One H.264 stream: Annex B access units in, BGRA pictures out.
pub struct Decoder {
    codec: Codec,
}

impl Decoder {
    pub fn new() -> Option<Decoder> {
        // Each picture as soon as its frame arrives: no frame threads (each adds a frame of
        // delay) and no waiting for reordering (the encoders don't reorder).
        let options = [(c"threads", c"1"), (c"flags", c"+low_delay")];
        Some(Decoder { codec: Codec::open(AV_CODEC_ID_H264, None, &options)? })
    }

    /// Decodes one access unit into `bgra`; returns the picture's size, or None while
    /// there's none (before the first keyframe, or on errors, which FFmpeg conceals).
    pub fn decode(&mut self, annex_b: &[u8], bgra: &mut Vec<u8>) -> Option<(usize, usize)> {
        let mut size = None;
        self.codec.decode(annex_b, |f| {
            if matches!(f.format, AV_PIX_FMT_YUV420P | AV_PIX_FMT_YUVJ420P) && f.width > 0 && f.height > 0 {
                let (w, h) = (f.width as usize, f.height as usize);
                let (ys, uvs) = (f.linesize[0] as usize, f.linesize[1] as usize);
                unsafe {
                    let y = std::slice::from_raw_parts(f.data[0], ys * h);
                    let u = std::slice::from_raw_parts(f.data[1], uvs * h.div_ceil(2));
                    let v = std::slice::from_raw_parts(f.data[2], uvs * h.div_ceil(2));
                    i420_to_bgra(y, ys, u, v, uvs, w, h, bgra);
                }
                size = Some((w, h));
            }
        });
        size
    }
}

/// The Mac's AAC-ELD sound (docs/protocol.md, Audio): packets in, the first channel out as
/// 48 kHz float samples (the microphone is mono, sent on both).
pub struct AudioDecoder {
    codec: Codec,
}

impl AudioDecoder {
    /// From the payload's magic cookie.
    pub fn new(cookie: &[u8]) -> Option<AudioDecoder> {
        let config = audio_specific_config(cookie)?;
        // Stereo at 48 kHz, as the protocol has it: without the rate, every frame is refused.
        // (The channel options' names changed in FFmpeg 5.1; ones a version doesn't know are
        // ignored.)
        let options = [(c"ar", c"48000"), (c"ch_layout", c"stereo"), (c"channel_layout", c"stereo"), (c"ac", c"2")];
        Some(AudioDecoder { codec: Codec::open(AV_CODEC_ID_AAC, Some(config), &options)? })
    }

    /// Decodes one packet, adding its samples to `out`.
    pub fn decode(&mut self, packet: &[u8], out: &mut Vec<f32>) {
        self.codec.decode(packet, |f| {
            if f.format == AV_SAMPLE_FMT_FLTP && f.nb_samples > 0 && !f.data[0].is_null() {
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(f.data[0].cast::<f32>(), f.nb_samples as usize) });
            }
        });
    }
}

/// The AudioSpecificConfig in Apple's magic cookie, an MPEG-4 ES descriptor (tag 3) holding
/// a decoder config (4) holding the decoder-specific info (5); a cookie that isn't one is
/// taken as the config itself.
fn audio_specific_config(cookie: &[u8]) -> Option<&[u8]> {
    if cookie.first() != Some(&0x03) {
        return (!cookie.is_empty()).then_some(cookie);
    }
    let mut i = 0;
    while i < cookie.len() {
        let tag = cookie[i];
        i += 1;
        let mut len = 0usize;
        loop {
            let b = *cookie.get(i)?;
            i += 1;
            len = len << 7 | (b & 0x7F) as usize;
            if b & 0x80 == 0 {
                break;
            }
        }
        match tag {
            0x03 => {
                // ES_ID, then flags for optional fields.
                let flags = *cookie.get(i + 2)?;
                i += 3;
                if flags & 0x80 != 0 {
                    i += 2;
                }
                if flags & 0x40 != 0 {
                    i += 1 + *cookie.get(i)? as usize;
                }
                if flags & 0x20 != 0 {
                    i += 2;
                }
            }
            0x04 => i += 13, // object type, stream type, buffer size, bitrates; then its children
            0x05 => return cookie.get(i..i + len),
            _ => i += len,
        }
    }
    None
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
        let Some(mut decoder) = ffmpeg(Decoder::new()) else { return };
        assert!(available() && aac_available());
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

    /// FFmpeg, if installed; else the test is skipped, unless KVM_REQUIRE_FFMPEG is set (CI).
    fn ffmpeg<T>(made: Option<T>) -> Option<T> {
        if made.is_none() {
            assert!(std::env::var_os("KVM_REQUIRE_FFMPEG").is_none(), "FFmpeg's libavcodec didn't load");
        }
        made
    }

    /// The length-prefixed payloads of a recorded stream.
    fn payloads(data: &[u8]) -> Vec<&[u8]> {
        let (mut rest, mut out) = (data, Vec::new());
        while rest.len() >= 4 {
            let len = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
            out.push(&rest[4..4 + len]);
            rest = &rest[4 + len..];
        }
        out
    }

    /// The Mac's screen stream, recorded from VideoToolbox with ScreenHost's settings
    /// (testdata/mac-h264-1080p.bin, or KVM_MAC_PAYLOADS): frame f shows a bright square
    /// centered on column 23f (wrapping at the width) in the middle row. Every frame must
    /// come out at once, and right: the stream uses long-term references.
    #[test]
    fn decodes_mac_screen() {
        let Some(mut decoder) = ffmpeg(Decoder::new()) else { return };
        let data = match std::env::var("KVM_MAC_PAYLOADS") {
            Ok(path) => std::fs::read(path).unwrap(),
            Err(_) => include_bytes!("../testdata/mac-h264-1080p.bin").to_vec(),
        };
        let mut bgra = Vec::new();
        for (frame, payload) in payloads(&data).into_iter().enumerate() {
            let frame = frame as i64;
            let (_, _, stream) = unpack(payload).unwrap();
            let (w, h) = decoder.decode(&stream, &mut bgra).unwrap_or_else(|| panic!("frame {frame}: no picture"));
            let edge = square(&bgra, w, h / 2).map(|x| x as i64);
            let want = (23 * frame % w as i64 - 59).max(0);
            assert!(edge.is_some_and(|x| (x - want).abs() <= 2), "frame {frame}: {edge:?}, not {want}");
        }
    }

    #[test]
    fn finds_the_config_in_apples_cookie() {
        // The Mac's AAC-ELD encoder's cookie (48 kHz stereo).
        let cookie = [
            0x03, 0x80, 0x80, 0x80, 0x24, 0x00, 0x00, 0x00, 0x04, 0x80, 0x80, 0x80, 0x16, 0x40, 0x14, 0x00, 0x18, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0xF4, 0x00, 0x05, 0x80, 0x80, 0x80, 0x04, 0xF8, 0xE6, 0x40, 0x00, 0x06, 0x80,
            0x80, 0x80, 0x01, 0x02,
        ];
        assert_eq!(audio_specific_config(&cookie), Some(&[0xF8, 0xE6, 0x40, 0x00][..]));
        assert_eq!(audio_specific_config(&[0x12, 0x10]), Some(&[0x12, 0x10][..]));
        assert_eq!(audio_specific_config(&cookie[..20]), None);
    }

    /// The Mac's microphone as it sends it (testdata/mac-aac-eld-tone.bin: AudioEncoder from
    /// AudioStream.swift, 1 s of a 440 Hz tone at half scale on both channels) decodes back
    /// to the tone: 512 samples a packet, at full level.
    #[test]
    fn decodes_mac_microphone() {
        let data = include_bytes!("../testdata/mac-aac-eld-tone.bin");
        let payloads = payloads(data);
        assert_eq!(payloads[0][0], 0, "starts with the cookie");
        let Some(mut decoder) = ffmpeg(AudioDecoder::new(&payloads[0][1..])) else { return };
        let mut samples = Vec::new();
        for payload in &payloads[1..] {
            assert_eq!(payload[0], 1);
            let before = samples.len();
            decoder.decode(&payload[1..], &mut samples);
            assert_eq!(samples.len() - before, 512);
        }
        // Past the warm-up, the best match with the tone (at some delay) is the tone itself.
        let tone = |k: usize| (k as f32 / 48_000.0 * 440.0 * std::f32::consts::TAU).sin() * 0.5;
        let (skip, n) = (4800, 24_000);
        let gain = (0..480)
            .map(|lag| {
                let (mut dot, mut energy) = (0.0, 0.0);
                for k in 0..n {
                    let reference = tone(k + skip - lag);
                    dot += samples[k + skip] * reference;
                    energy += reference * reference;
                }
                dot / energy
            })
            .fold(0f32, f32::max);
        assert!((gain - 1.0).abs() < 0.05, "gain {gain}");
    }
}
