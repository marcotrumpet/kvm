# KVM

Share one mouse and keyboard between two computers, peer to peer, on the same network or
over the internet. A menu bar app on macOS; Windows and Linux are planned.

- **Switching**: a global shortcut, or hovering over the other computer's screen in the viewer.
- **Direct connections** via [iroh](https://github.com/n0-computer/iroh) (QUIC, hole punching,
  relay fallback). No account or server of its own; local discovery by mDNS.
- **Input**: relative mouse, keys as USB HID usages, media and Mac system keys, mouse wheel
  notches; nothing stays stuck if the connection drops (heartbeat failsafe).
- **Clipboard**: text, rich text, images, and files copied in Finder (pasted on demand).
- **File sharing**: drop on the menu bar icon or the viewer; permissions, links and app bundles survive.
- **Screen viewer**: hardware H.264, adaptive bitrate and quality, multiple displays, full-screen 1:1.
- **Wi-Fi**: optionally pauses AWDL (AirDrop's radio hopping) while connected, which removes
  80–100 ms lag spikes.

## Layout

| Path | What |
|---|---|
| `core/` | Rust: protocol, sessions, discovery and pairing, video transport and rate control, files. `kvm-cli` test tool. |
| `ffi/` | C ABI over the core (`ffi/include/kvm.h`) for the platform apps. |
| `awdl-helper/` | Small root daemon that keeps `awdl0` down while the app asks. |
| `macos/` | Swift menu bar app: capture, injection, UI, screen capture and display. |
| `docs/protocol.md` | Wire protocol. |

## Build (macOS)

Needs Rust (`rustup`) and Xcode.

```sh
macos/build.sh        # universal KVM.app and zip in dist/
cargo test            # core tests
```

`build.sh` signs with a local self-signed certificate named "KVM Local Signing" if one is in
your keychain (so macOS keeps the Accessibility / Input Monitoring grants across builds), and
ad-hoc otherwise.

The app needs Accessibility and Input Monitoring, plus Screen Recording to share its screen.

## Testing without the app

```sh
cargo build --release
target/release/kvm-cli id
target/release/kvm-cli run --peer <other id> --synthetic 5 --bench 10
```
