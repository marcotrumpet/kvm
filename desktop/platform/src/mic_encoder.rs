//! The microphone as the Mac plays it (docs/protocol.md, AudioStream.swift): payload kind 2,
//! one Opus packet each: 48 kHz, 10 ms, stereo with the (mono) microphone on both channels.
//! Windows and Linux have no AAC-ELD encoder, which kind 1 needs.

use opus::{Application, Bitrate, Channels, Encoder};

/// Payload kind for one Opus packet.
pub const KIND_OPUS: u8 = 2;
const RATE: u32 = 48_000;
/// Samples per packet: 10 ms.
const FRAME: usize = 480;

/// Mono samples at any rate in, payloads out.
pub struct MicEncoder {
    opus: Encoder,
    /// 48 kHz samples not yet encoded.
    pending: Vec<f32>,
    /// Where the next 48 kHz sample falls in the next input, in input samples (-1 is `prev`).
    pos: f64,
    prev: f32,
    stereo: Vec<f32>,
}

impl MicEncoder {
    pub fn new() -> Result<MicEncoder, opus::Error> {
        let mut opus = Encoder::new(RATE, Channels::Stereo, Application::Voip)?;
        opus.set_bitrate(Bitrate::Bits(64_000))?;
        Ok(MicEncoder { opus, pending: Vec::new(), pos: 0.0, prev: 0.0, stereo: Vec::with_capacity(FRAME * 2) })
    }

    /// Encodes `samples` (mono, at `rate` Hz) as far as whole packets go; the rest waits.
    pub fn push(&mut self, samples: &[f32], rate: u32) -> Vec<Vec<u8>> {
        resample(samples, rate, &mut self.pos, &mut self.prev, &mut self.pending);
        let mut payloads = Vec::new();
        while self.pending.len() >= FRAME {
            self.stereo.clear();
            self.stereo.extend(self.pending[..FRAME].iter().flat_map(|&s| [s, s]));
            self.pending.drain(..FRAME);
            match self.opus.encode_vec_float(&self.stereo, 1500) {
                Ok(packet) => {
                    let mut payload = Vec::with_capacity(packet.len() + 1);
                    payload.push(KIND_OPUS);
                    payload.extend_from_slice(&packet);
                    payloads.push(payload);
                }
                Err(err) => tracing::warn!("encoding the microphone: {err}"),
            }
        }
        payloads
    }
}

/// Linear resampling to 48 kHz, carrying `pos` and the last sample `prev` across calls.
fn resample(input: &[f32], rate: u32, pos: &mut f64, prev: &mut f32, out: &mut Vec<f32>) {
    if input.is_empty() || rate == 0 {
        return;
    }
    if rate == RATE {
        out.extend_from_slice(input);
        *prev = input[input.len() - 1];
        return;
    }
    let step = rate as f64 / RATE as f64;
    let last = (input.len() - 1) as f64;
    let at = |i: isize| if i < 0 { *prev } else { input[i as usize] };
    let mut t = *pos;
    while t <= last {
        let i = t.floor() as isize;
        let frac = (t - i as f64) as f32;
        let a = at(i);
        let b = if frac > 0.0 { at(i + 1) } else { a };
        out.push(a + (b - a) * frac);
        t += step;
    }
    *pos = t - input.len() as f64;
    *prev = input[input.len() - 1];
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tone(rate: u32, from: usize, n: usize) -> Vec<f32> {
        (from..from + n).map(|i| (i as f32 / rate as f32 * 440.0 * std::f32::consts::TAU).sin() * 0.5).collect()
    }

    #[test]
    fn resamples_in_pieces_like_at_once() {
        let input = tone(44_100, 0, 4410);
        let (mut whole, mut pieces) = (Vec::new(), Vec::new());
        let (mut pos, mut prev) = (0.0, 0.0);
        resample(&input, 44_100, &mut pos, &mut prev, &mut whole);
        let (mut pos, mut prev) = (0.0, 0.0);
        for chunk in input.chunks(441) {
            resample(chunk, 44_100, &mut pos, &mut prev, &mut pieces);
        }
        assert_eq!(whole.len(), pieces.len());
        assert!((whole.len() as i64 - 4800).abs() <= 1, "{}", whole.len());
        for (a, b) in whole.iter().zip(&pieces) {
            assert!((a - b).abs() < 1e-5);
        }
        // And it's the same tone at 48 kHz.
        for (got, want) in whole.iter().zip(tone(48_000, 0, whole.len())) {
            assert!((got - want).abs() < 0.01, "{got} vs {want}");
        }
    }

    /// Packets come out every 10 ms of input, whatever the capture's chunks, and decode
    /// (with libopus) back to the tone on both channels. With KVM_MIC_PACKETS set, the Opus
    /// packets are also written there (length-prefixed), to decode with the Mac's code.
    #[test]
    fn encodes_10ms_stereo_packets() {
        let mut encoder = MicEncoder::new().unwrap();
        let mut payloads = Vec::new();
        let input = tone(44_100, 0, 44_100);
        for chunk in input.chunks(1000) {
            payloads.extend(encoder.push(chunk, 44_100));
        }
        assert!((99..=100).contains(&payloads.len()), "{}", payloads.len());
        let mut decoder = opus::Decoder::new(RATE, Channels::Stereo).unwrap();
        let mut decoded = Vec::new();
        for payload in &payloads {
            assert_eq!(payload[0], KIND_OPUS);
            let mut out = vec![0f32; FRAME * 2];
            assert_eq!(decoder.decode_float(&payload[1..], &mut out, false).unwrap(), FRAME);
            decoded.extend(out);
        }
        if let Ok(path) = std::env::var("KVM_MIC_PACKETS") {
            let bytes: Vec<u8> =
                payloads.iter().flat_map(|p| ((p.len() - 1) as u32).to_be_bytes().into_iter().chain(p[1..].iter().copied())).collect();
            std::fs::write(path, bytes).unwrap();
        }
        // Past the codec's warm-up: both channels the same, and loud like the tone.
        let settled = &decoded[9600..];
        let rms = (settled.iter().map(|s| s * s).sum::<f32>() / settled.len() as f32).sqrt();
        assert!((rms - 0.5 / 2f32.sqrt()).abs() < 0.05, "rms {rms}");
        for pair in settled.chunks(2) {
            assert!((pair[0] - pair[1]).abs() < 0.02);
        }
    }
}
