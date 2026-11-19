//! The Mac's microphone as this computer's "KVM Microphone" while the Mac controls it
//! (Linux): AAC-ELD payloads decoded with FFmpeg ([`crate::avcodec`]), played into the
//! virtual source ([`crate::linux::mic`]).

use std::sync::Mutex;

use crate::avcodec::{AudioDecoder, aac_available};
use crate::linux::mic::{Playback, VirtualMicrophone};

/// Payload kinds (docs/protocol.md, Audio).
const KIND_COOKIE: u8 = 0;
const KIND_PACKET: u8 = 1;

pub struct MicFeed {
    _device: VirtualMicrophone,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    cookie: Vec<u8>,
    decoder: Option<AudioDecoder>,
    playback: Option<Playback>,
}

impl MicFeed {
    /// Makes the "KVM Microphone" source. None without FFmpeg's AAC decoder or pactl.
    pub fn new() -> Option<MicFeed> {
        if !aac_available() {
            return None;
        }
        let device = VirtualMicrophone::create()?;
        Some(MicFeed { _device: device, state: Mutex::new(State::default()) })
    }

    /// One payload of the Mac's microphone.
    pub fn handle(&self, payload: &[u8]) {
        let Some((&kind, body)) = payload.split_first() else { return };
        let mut s = self.state.lock().unwrap();
        match kind {
            KIND_COOKIE if body != s.cookie => {
                s.decoder = AudioDecoder::new(body);
                if s.decoder.is_none() {
                    tracing::warn!("can't decode the Mac's microphone");
                }
                s.cookie = body.to_vec();
            }
            KIND_PACKET if !body.is_empty() => {
                let Some(decoder) = s.decoder.as_mut() else { return };
                let mut samples = Vec::with_capacity(512);
                decoder.decode(body, &mut samples);
                if samples.is_empty() {
                    return;
                }
                if s.playback.is_none() {
                    s.playback = Playback::start();
                }
                if let Some(playback) = &s.playback {
                    playback.play(samples);
                }
            }
            _ => {}
        }
    }

    /// Silence until the next payload.
    pub fn stop(&self) {
        *self.state.lock().unwrap() = State::default();
    }
}
