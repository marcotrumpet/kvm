# KVM

Share one mouse and keyboard between two computers, peer to peer, on the same network or
over the internet. A menu bar app on macOS, a tray app on Windows and Linux (mouse, keyboard
and clipboard so far), plus an iPhone and iPad app that controls a Mac.

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
| `desktop/app/` | Windows and Linux tray app in Rust, straight on the core: pairing, control, clipboard. |
| `desktop/platform/` | Its OS layer (Windows: hooks, SendInput, WinSparkle; Linux: X11 grabs, XTest, StatusNotifierItem tray), kept free of the core so `cargo check -p kvm-platform --target x86_64-pc-windows-msvc` (or `x86_64-unknown-linux-gnu`) works anywhere. |
| `desktop/windows/`, `desktop/linux/` | Inno Setup installer script; Linux install script and desktop entry. |
| `vendor/` | `netdev` and `netwatch` patched to build for iOS (see the root `Cargo.toml`). |
| `docs/protocol.md` | Wire protocol. |

## Networks and firewalls

Connections are direct whenever possible: on the same network (found by mDNS), or across the
internet by UDP hole punching. Where UDP is blocked or hole punching fails, traffic goes
through iroh's relays over HTTPS on port 443, like web browsing, so it gets through most
office, hotel and public Wi-Fi networks (slower; the menu shows when it's relayed).

- **Web proxy**: used automatically when the system has one (System Settings > Network >
  Details > Proxies on the Mac, the Wi-Fi settings on iPhone and iPad), or `HTTPS_PROXY`.
- **mDNS blocked** (guest and office networks): machines don't show up automatically, but
  pairing by ID ("Pair over the internet" in KVM Settings) works, and once paired the
  computers find each other by ID wherever they are.
- **macOS firewall**: allow KVM if asked (System Settings > Network > Firewall).
- Networks that only allow approved services block it by design: ask whoever runs them.

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

## Windows and Linux

A tray app that pairs with a Mac as its other computer: share mouse and keyboard both ways
(Ctrl+Alt+K switches, like the Mac's shortcut), and the clipboard (text). The Mac's Command
keys act as Ctrl on Windows (and Ctrl as Command the other way), so shortcuts stay under
the same fingers; the tray menu turns that off. Not yet: viewing screens, files, the
microphone.

To pair: open KVM Settings on the Mac, then on Windows choose the Mac under Pair With in
the tray menu (or Allow Pairing on Windows and pair from the Mac), and check the code.
Pairing replaces the Mac's current other computer.

Where mDNS is blocked (guest and office networks), pair by ID instead: Pair Over the
Internet > Copy This Computer's ID, or copy the Mac's ID (KVM Settings > Pair over the
internet) and choose Pair With the ID on the Clipboard. Behind a web proxy, the system's
proxy setting is used (Windows' Internet Settings; GNOME's on Linux, or `HTTPS_PROXY`). If
Windows Firewall asks, allow KVM on private networks.

Windows: the installer (per user, no admin) and updates (WinSparkle, from the same GitHub
releases and key as the Mac app) come from the release workflow.

Linux: X11 sessions only for now (Wayland has no global input capture or injection without
libei and portals). Unpack `KVM-linux-x86_64.tar.gz` and run `./install.sh` (installs to
`~/.local/bin`, adds a menu entry and starts it at sign-in). The tray needs a
StatusNotifierItem host (KDE, Xfce, GNOME with the AppIndicator extension); dialogs use
zenity or kdialog. It checks daily for a new release and offers the download page.

Locally: `cargo run -p kvm-desktop`.

## Testing without the app

```sh
cargo build --release
target/release/kvm-cli id
target/release/kvm-cli run --peer <other id> --synthetic 5 --bench 10
target/release/kvm-cli host            # stand-in computer for the iOS app: accepts pairing, prints input
cargo test -p kvm-core --test remote -- --ignored   # remote-control link (needs the network)
```
