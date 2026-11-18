//! Networking related utilities

// KVM patch: vendored for iOS (see sys_libc.rs); keep upstream warnings out of our builds.
#![allow(deprecated)]

pub mod interfaces;
pub mod ip;
mod ip_family;
pub mod netmon;
#[cfg(target_vendor = "apple")]
mod sys_libc;
#[cfg(not(wasm_browser))]
mod udp;

pub use self::ip_family::IpFamily;
#[cfg(not(wasm_browser))]
pub use self::udp::{UdpSender, UdpSocket};
