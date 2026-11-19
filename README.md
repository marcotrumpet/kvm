# KVM

Share one mouse and keyboard between two computers, peer to peer, on the same network or
over the internet. A menu bar app on macOS, plus an iPhone and iPad app that controls a Mac;
Windows and Linux are planned.

- **Switching**: a global shortcut, or hovering over the other computer's screen in the viewer.
- **Direct connections** via [iroh](https://github.com/n0-computer/iroh) (QUIC, hole punching,
  relay fallback). No account or server of its own; local discovery by mDNS.
- **Input**: relative mouse, keys as USB HID usages, media and Mac system keys, mouse wheel
  notches; nothing stays stuck if the connection drops (heartbeat failsafe).
- **Clipboard**: text, rich text, images, and files copied in Finder (pasted on demand).
- **File sharing**: drop on the menu bar icon or the viewer; permissions, links and app bundles survive.
- **Screen viewer**: hardware HEVC or H.264, adaptive bitrate and quality, the host's sound,
  multiple displays, full-screen 1:1. The host can re-lay out its desktop to the viewer's
  size or shape, or add the viewer as an extra screen (iPad, like Sidecar).
- **Microphone**: the controlling computer's, iPhone's or iPad's microphone becomes "KVM
  Microphone" on the controlled Mac, for calls there (a small Core Audio driver).
- **Lock and wake**: locking either Mac locks both; switching to a sleeping Mac wakes it
  (Wake-on-LAN, same network).
- **In use from elsewhere**: a panel on the Mac while it is controlled or viewed, with a
  Take Back button.
- **Wi-Fi**: optionally pauses AWDL (AirDrop's radio hopping) while connected, which removes
  80–100 ms lag spikes.

## Layout

| Path | What |
|---|---|
| `core/` | Rust: protocol, sessions, discovery and pairing, video transport and rate control, files. `kvm-cli` test tool. |
| `ffi/` | C ABI over the core (`ffi/include/kvm.h`) for the platform apps. |
| `awdl-helper/` | Small root daemon that keeps `awdl0` down while the app asks. |
| `macos/` | Swift menu bar app: capture, injection, UI, screen capture and display. |
| `macos/Microphone/` | "KVM Microphone" Core Audio driver (C): a hidden output device looped into an input for call apps. |
| `ios/` | iPhone/iPad remote control (Xcode project); shares `Core.swift` and `VideoFrames.swift` with `macos/`. |
| `vendor/` | `netdev` and `netwatch` patched to build for iOS (see the root `Cargo.toml`). |
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

## iPhone and iPad

A remote control for a Mac: its screen, the touch screen as a trackpad (or direct touch on the
picture), the on-screen keyboard (typed text arrives right whatever the Mac's layout, plus a
bar with esc, tab, arrows and sticky ⌃⌥⌘⇧), and on iPad a hardware keyboard and a mouse or
trackpad (pointer locked). It is paired next to the Mac's other computer, not instead of it.

```sh
rustup target add aarch64-apple-ios aarch64-apple-ios-sim   # once
open ios/KVM.xcodeproj                                      # then Run; a build phase builds the Rust core
```

Pair: on the Mac open KVM Settings and scan the code under "iPhone and iPad" with the Camera
or the app's + button, then allow it on the Mac.

Gestures (trackpad mode): drag moves the pointer, tap clicks, two-finger tap right-clicks,
two-finger drag scrolls, touch and hold then drag drags. Touch mode: tap clicks where you
tap, drag drags from there, touch and hold right-clicks. Both: pinch zooms and two fingers
turning rotate (real trackpad gestures on the Mac, also from an iPad trackpad); three fingers
swiping up show Mission Control, down App Exposé, left and right switch Spaces. With several
displays, pushing the pointer past the left or right edge of the one shown switches to the
next. An iPad trackpad or mouse moves the Mac's pointer directly (raw deltas when iPadOS
locks the pointer to the app, otherwise the iPad pointer points at the picture).

## Testing without the app

```sh
cargo build --release
target/release/kvm-cli id
target/release/kvm-cli run --peer <other id> --synthetic 5 --bench 10
target/release/kvm-cli host            # stand-in computer for the iOS app: accepts pairing, prints input
cargo test -p kvm-core --test remote -- --ignored   # remote-control link (needs the network)
```
