# Wire protocol (v2)

One QUIC connection per peer pair (iroh, ALPN `kvm/1`). Both ends are authenticated by their
Ed25519 endpoint id; only the paired id is accepted. The side with the smaller id dials.

## Channels

| Channel | Reliability | Carries |
|---|---|---|
| QUIC datagrams | unreliable | `Datagram::Motion` |
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
`Consumer{usage: HID consumer-page usage, down}` (media and system keys) · `Motion`.

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

### Displays

While being viewed, the host sends `Control::DisplayList` (and again when its displays
change): JSON `[{"id": u32, "name": str, "width": px, "height": px, "main": bool}, ...]`
ordered left to right. `VideoParams.display` and `PointerAbs.display` pick one by id; 0
means the main display.

### Viewer control

Entering control from the screen viewer, the controller sends `Enter`, `PointerAbs` (where
the cursor entered the picture) and `Confine{display}`. The controlled side then keeps the
pointer on that display and, when it is pushed past an edge, answers `EdgeExit{edge,
along}`; the controller returns control and puts its own cursor just outside the viewer
at the matching spot. `Leave` lifts the confinement.
