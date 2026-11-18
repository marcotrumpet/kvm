pub mod flags;
// KVM patch: `libc::in6_ifreq` only exists for macOS, so iOS goes without the flags.
#[cfg(target_os = "macos")]
pub mod ipv6_addr_flags;
pub mod mtu;
#[cfg(feature = "gateway")]
pub mod route;
pub mod state;
pub mod types;
