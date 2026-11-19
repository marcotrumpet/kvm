//! A software H.264 encoder (Cisco's OpenH264, built from source) for where the OS has
//! none: Linux. Tuned for screen content in real time; Annex B out, like Media Foundation.
//!
//! OpenH264 can't change a running encoder's bitrate, so a big enough change from the
//! core's rate control rebuilds it (costing a keyframe), at most every couple of seconds.

use std::time::{Duration, Instant};

use openh264::OpenH264API;
use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, RateControlMode, UsageType,
};
use openh264::formats::YUVBuffer;

/// Rebuild for bitrate changes of at least this fraction...
const REBUILD_CHANGE: f64 = 0.25;
/// ...and not more often than this.
const REBUILD_EVERY: Duration = Duration::from_secs(2);

pub struct SoftwareEncoder {
    encoder: Encoder,
    width: usize,
    height: usize,
    fps: u32,
    kbps: u32,
    wanted_kbps: u32,
    built: Instant,
}

fn build(fps: u32, kbps: u32) -> Result<Encoder, openh264::Error> {
    let config = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .rate_control_mode(RateControlMode::Bitrate)
        .bitrate(BitRate::from_bps(kbps.saturating_mul(1000)))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps * 10))
        .skip_frames(false);
    Encoder::with_api_config(OpenH264API::from_source(), config)
}

impl SoftwareEncoder {
    pub fn new(width: u32, height: u32, fps: u32, kbps: u32) -> Result<SoftwareEncoder, openh264::Error> {
        Ok(SoftwareEncoder {
            encoder: build(fps, kbps)?,
            width: width as usize,
            height: height as usize,
            fps,
            kbps,
            wanted_kbps: kbps,
            built: Instant::now(),
        })
    }

    pub fn set_bitrate(&mut self, kbps: u32) {
        self.wanted_kbps = kbps;
    }

    /// Encodes one NV12 frame; returns the Annex B access unit and whether it's a keyframe.
    pub fn encode(&mut self, nv12: &[u8], keyframe: bool) -> Result<(Vec<u8>, bool), openh264::Error> {
        let mut keyframe = keyframe;
        let change = (self.wanted_kbps as f64 / self.kbps.max(1) as f64 - 1.0).abs();
        if change >= REBUILD_CHANGE && self.built.elapsed() >= REBUILD_EVERY {
            self.encoder = build(self.fps, self.wanted_kbps)?;
            self.kbps = self.wanted_kbps;
            self.built = Instant::now();
            keyframe = true; // a new encoder starts over anyway
        }
        let mut i420 = Vec::new();
        nv12_to_i420(nv12, self.width, self.height, &mut i420);
        let frame = YUVBuffer::from_vec(i420, self.width, self.height);
        if keyframe {
            self.encoder.force_intra_frame();
        }
        let encoded = self.encoder.encode(&frame)?;
        let key = matches!(encoded.frame_type(), FrameType::IDR | FrameType::I);
        Ok((encoded.to_vec(), key))
    }
}

/// NV12 (interleaved chroma) to I420 (separate U and V planes), as OpenH264 takes it.
pub fn nv12_to_i420(nv12: &[u8], width: usize, height: usize, out: &mut Vec<u8>) {
    let luma = width * height;
    let chroma = luma / 4;
    out.resize(luma + 2 * chroma, 0);
    out[..luma].copy_from_slice(&nv12[..luma]);
    let (u, v) = out[luma..].split_at_mut(chroma);
    for (i, pair) in nv12[luma..luma + 2 * chroma].chunks_exact(2).enumerate() {
        u[i] = pair[0];
        v[i] = pair[1];
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::{H264Packager, bgra_to_nv12, unpack};

    /// A test picture: a gradient with a square that moves each frame.
    fn picture(width: u32, height: u32, frame: u32) -> Vec<u8> {
        let mut bgra = vec![0u8; (width * height * 4) as usize];
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let inside = (x as i32 - (frame * 7 % width) as i32).unsigned_abs() < 40 && (y as i32 - 100).abs() < 40;
                let px = if inside { [255, 255, 255, 255] } else { [(x % 256) as u8, (y % 256) as u8, 128, 255] };
                bgra[i..i + 4].copy_from_slice(&px);
            }
        }
        bgra
    }

    #[test]
    fn i420_planes() {
        let nv12 = [1, 2, 3, 4, 10, 20]; // 2x2: four luma, one U/V pair
        let mut out = Vec::new();
        nv12_to_i420(&nv12, 2, 2, &mut out);
        assert_eq!(out, vec![1, 2, 3, 4, 10, 20]);
    }

    /// Encodes frames for real and checks the payloads: a keyframe first (with SPS and
    /// PPS), then deltas, all unpacking back to H.264. With KVM_H264_PAYLOADS set, the
    /// payloads are also written there (length-prefixed), to decode with the viewers' code.
    #[test]
    fn encodes_screen_frames() {
        let (w, h, fps) = (320u32, 240u32, 30u32);
        let mut encoder = SoftwareEncoder::new(w, h, fps, 2000).unwrap();
        let mut packager = H264Packager::default();
        let mut nv12 = Vec::new();
        let mut keys = 0;
        let mut written = Vec::new();
        for frame in 0..20 {
            bgra_to_nv12(&picture(w, h, frame), w, h, (w * 4) as usize, w, h, &mut nv12);
            let (annex_b, key) = encoder.encode(&nv12, frame == 0 || frame == 10).unwrap();
            assert_eq!(key, frame == 0 || frame == 10, "frame {frame}");
            let payload = packager.package(&annex_b, key).expect("a payload per frame");
            let (hevc, has_sets, stream) = unpack(&payload).unwrap();
            assert!(!hevc);
            assert_eq!(has_sets, key);
            assert!(!stream.is_empty());
            keys += key as u32;
            written.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            written.extend_from_slice(&payload);
        }
        assert_eq!(keys, 2);
        if let Ok(path) = std::env::var("KVM_H264_PAYLOADS") {
            std::fs::write(path, written).unwrap();
        }
    }
}
