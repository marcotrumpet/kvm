//! This computer's microphone, sent to the Mac while controlling it: captured by the OS
//! layer, encoded as Opus payloads ([`crate::mic_encoder`]).

#[cfg(target_os = "linux")]
use crate::linux::mic as capture;
use crate::mic_encoder::MicEncoder;
#[cfg(windows)]
use crate::windows::mic as capture;

/// Whether there's a way to capture here.
pub fn supported() -> bool {
    capture::available()
}

/// Sending while alive.
pub struct Microphone {
    _capture: capture::Capture,
}

impl Microphone {
    /// Starts capturing; `on_payload` gets each encoded payload (on the capture thread).
    /// None if there's no microphone.
    pub fn start(mut on_payload: Box<dyn FnMut(Vec<u8>) + Send>) -> Option<Microphone> {
        let mut encoder = match MicEncoder::new() {
            Ok(encoder) => encoder,
            Err(err) => {
                tracing::warn!("no microphone encoder: {err}");
                return None;
            }
        };
        let capture = capture::Capture::start(Box::new(move |samples, rate| {
            for payload in encoder.push(samples, rate) {
                on_payload(payload);
            }
        }))?;
        Some(Microphone { _capture: capture })
    }
}
