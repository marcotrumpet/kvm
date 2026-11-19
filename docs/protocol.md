# Wire protocol (v2)

One QUIC connection per peer pair (iroh, ALPN `kvm/1`). Both ends are authenticated by their
Ed25519 endpoint id; only the paired id is accepted. The side with the smaller id dials.

Remote controls (the iPhone/iPad app) are the exception: they always dial, and a computer
accepts them on a second link next to the one with its paired peer, from any id in its list
of paired remote controls, one session at a time. They only ever send input.

## Channels

| Channel | Reliability | Carries |
|---|---|---|
| QUIC datagrams | unreliable | `Datagram::Motion`, `Datagram::Audio`, `Datagram::Microphone` |
| first bi-directional stream | reliable, ordered | `Control` frames: u32 LE length + postcard body, max 64 KiB |

## Motion

`Motion { seq, x, y, scroll_x, scroll_y }` holds *cumulative* totals since the connection
started. The receiver injects the difference from the last applied state and ignores
anything with an older `seq`. A lost datagram costs nothing: the next one catches up.
`Key`/`Button` frames carry the sender's current `Motion`, so the pointer is caught up
before a click lands even if the datagrams before it were lost. If the path can't carry
datagrams, motion falls back to `Control::Motion` on the stream.

## Control

`Hello{version,name}` (dialer first) · `Ping{nonce}`/`Pong{nonce}` · `Enter` · `Leave` ·
`Key{code: HID usage, down, motion}` · `Button{button, down, motion}` ·
`Consumer{usage: HID consumer-page usage, down}` (media and system keys) · `Motion` ·
`Text(String)` (typed as-is whatever the receiver's keyboard layout, max 16 KiB: a phone's
on-screen keyboard has no key positions to send; line breaks and tabs are typed as keys) ·
`Gesture{kind, phase, value}` (pinch: magnification change x 10000, or rotation: degrees
counterclockwise x 100; phases 1 began, 2 changed, 4 ended; replayed as real trackpad
gesture events).

`Lock` (the sender's screen locked; the receiver locks too if its user wants that) ·
`WakeAddress(mac)` (6 bytes, for Wake-on-LAN later) · `TakeBack` (from the controlled
side: its user took the machine back; the receiver stops controlling and viewing it, and
the sender has already released its input and stopped sharing).

The receiver only injects input between `Enter` and `Leave`, tracks what it holds down, and
drops key-ups for keys it never pressed. `Leave` releases everything still held.

## Liveness

Each side pings every 250 ms, and liveness is measured on *pongs* to its own pings, so a
one-way loss stalls both sides.
- **1 s without a pong:** release everything held, drop control on both sides (`Stalled`).
- **Pongs resume:** `Recovered`, but control stays local.
- **5 s without a pong:** close the connection; the link reconnects.

## Unidirectional streams

Each starts with a kind byte: `0` video frame, `1` clipboard, `2` file. Priorities: control
stream 10, clipboard -5, video -10, files -20, so input always goes first.

### Video

`seq: u64 LE | flags: u8 (1 = keyframe) | payload`. At most 3 frames in flight; beyond that
the host refuses frames and encodes the next as a keyframe. The viewer delivers frames in
order and, after a loss (a gap for 40 ms), waits for the next keyframe and requests one.

Payload (the apps' format): `codec << 7 | nalLengthSize: u8 | parameterSetCount: u8 |
(len: u16 BE, bytes)* | AVCC`. Codec 0 is H.264 (SPS, PPS), 1 HEVC (VPS, SPS, PPS); the
host uses HEVC only when `VideoParams.codecs` bit 0 says the viewer decodes it. Keyframes
carry the parameter sets.

The host adapts the bitrate every 500 ms (AIMD): congestion (a refused frame, or mean frame
delivery time 25 ms above the best seen) cuts it to 70%, then it holds for 2 s; otherwise it
grows 8% per tick while frames flow, up to the viewer's ceiling, never below 1 Mbit/s.

### Clipboard

Opaque to the core (max 32 MiB). The apps use: `count: u16 BE`, then per entry
`type_len: u16 BE | type (UTF-8, MIME) | len: u32 BE | bytes`. Types: `text/plain`
(UTF-8), `text/html`, `text/rtf`, `image/png`.

### Files

`header_len: u32 LE | FileHeader (postcard) | bytes`. `FileHeader { transfer, index, count,
path, kind (file/dir/symlink), size, mode, mtime, target, offset, total, clipboard }`, one
stream per entry, folders before their contents; `path` is relative and '/'-separated, its first component the
item that was sent. Receivers keep permission bits (never setuid/setgid/sticky), modification times, empty
folders and symbolic links whose target stays inside the sent item; they never write
through a link. They keep only plain path components, write `.name.kvmpart` and
rename once the size matches, and give clashing top-level items Finder-style names.

Files copied in a file manager travel the same way with `clipboard = true`: the receiver
writes them to `<clipboard dir>/<transfer>/` and the app offers them for pasting. The
clipboard payload announces them with an `application/x-kvm-files` entry: JSON
`{"transfer": "<id>", "names": ["top-level item", ...]}`. Copies above the sender's limit
are announced as `application/x-kvm-files-too-large`: `{"names": [...], "bytes": n}`.

### Audio

With `VideoParams.audio`, the host also sends its sound (not the app's own), one AAC-ELD
packet (48 kHz stereo, 128 kbit/s, 512 frames) per `Datagram::Audio`: `kind: u8 | bytes`,
kind 0 the encoder's magic cookie (before the first packet and every 200 packets, so the
viewer can start or recover any time), kind 1 a packet. Lost packets are skipped, and the
viewer drops packets while more than 150 ms are queued, so delay can't build up. Without
datagrams there is no sound.

### Microphone

The other way, the controller may send its microphone while it controls the receiver
(after `Enter`, before `Leave`): `Datagram::Microphone`, same payloads as `Audio` (mono
input on both channels). The core sends it only while controlling and passes it on only
while controlled. A macOS receiver plays it into the hidden output device of its "KVM
Microphone" driver, which loops it into the "KVM Microphone" input that call apps use.

### Displays

While being viewed, the host sends `Control::DisplayList` (and again when its displays
change): JSON `[{"id": u32, "name": str, "width": px, "height": px, "main": bool}, ...]`
ordered left to right. `VideoParams.display` and `PointerAbs.display` pick one by id; 0
means the main display.

With `VideoParams.fit_host` (main display only), the host re-lays out its desktop to the
viewer: a macOS host creates a virtual display and mirrors its main display onto it, so the
picture keeps the viewer's aspect ratio. 1 sizes it to `max_width` x
`max_height`; 2 keeps only that aspect ratio, at about the host's own pixel
count, so `max_*` stays a cap the viewer can raise when zooming in. The virtual display is
the main one while it exists; it goes away with `VideoStop`, a request with `fit_host` 0,
or the connection.

`fit_host` 3 instead adds the virtual display as an extra screen at `max_width` x
`max_height`, to the right of the host's own, like Sidecar. While it exists it is display 0
(and the `main` one in the display list) for the viewer; the host's real main display is
still there by its id.

### Viewer control

Entering control from the screen viewer, the controller sends `Enter`, `PointerAbs` (where
the cursor entered the picture) and `Confine{display}`. The controlled side then keeps the
pointer on that display and, when it is pushed past an edge, answers `EdgeExit{edge,
along}`; the controller returns control and puts its own cursor just outside the viewer
at the matching spot. `Leave` lifts the confinement.

## Pairing

ALPN `kvm-pair/1`, one bi-directional stream: `Pair::Request{name}` or, from a remote control,
`Pair::RequestRemote{name}`; answered by `Pair::Reply{accepted, name}`. Only machines in
pairing mode (KVM Settings open) ask their user; others decline. Both show a six-digit code
derived from the two ids. A remote control is added to the computer's list of remote
controls instead of replacing its paired peer. The Mac's Settings show a QR code with
`kvm://pair?id=<id>&name=<name>`, which the iOS app (or the Camera) opens to request pairing.
iOS has no mDNS (it needs a multicast entitlement), so remote controls reach computers by id
through iroh's DNS address lookup and relays, then hole-punch to a direct path.
