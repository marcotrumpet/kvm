//! Shared core of the KVM app: the wire protocol, input state, and the peer-to-peer
//! session with the paired machine. Platform apps capture and inject input; this crate
//! moves it between machines.

pub mod files;
pub mod input;
pub mod node;
pub mod protocol;
pub mod session;
pub mod video;

pub use iroh::EndpointId;
pub use node::{Link, Node, NodeEvent};
pub use session::{Event, Session};
pub use files::{set_clipboard_dir, set_download_dir};
pub use video::VideoParams;
