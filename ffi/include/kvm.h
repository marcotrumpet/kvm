// C ABI of kvm-ffi (ffi/src/lib.rs). Keep in sync with the Rust side.
#ifndef KVM_H
#define KVM_H

#include <stdbool.h>
#include <stddef.h>
#include <stdint.h>

typedef struct KvmNode KvmNode;

enum {
    KVM_EV_CONNECTED = 0,    // text = peer name, id = its id
    KVM_EV_PATH = 1,         // a = rtt (µs), flag = direct (false: relayed)
    KVM_EV_LATENCY = 2,      // a = rtt (µs)
    KVM_EV_ENTERED = 3,      // peer took control of this machine: start injecting
    KVM_EV_LEFT = 4,         // peer released control (held input already released)
    KVM_EV_MOVE = 5,         // a = dx, b = dy (points)
    KVM_EV_SCROLL = 6,       // a = dx, b = dy (pixels)
    KVM_EV_KEY = 7,          // a = USB HID usage (page 0x07), flag = down
    KVM_EV_BUTTON = 8,       // a = button (0 left, 1 right, 2 middle, ...), flag = down
    KVM_EV_STALLED = 9,      // heartbeat lost: control is back to local on both sides
    KVM_EV_RECOVERED = 10,
    KVM_EV_DISCONNECTED = 11, // text = reason
    KVM_EV_CONSUMER = 12,     // a = USB HID consumer usage (page 0x0C), flag = down
    KVM_EV_WHEEL = 13,        // mouse wheel notches: a = dx, b = dy
    KVM_EV_VIDEO_REQUESTED = 14, // start (or update) streaming: a = display << 32 | max_width << 16 | max_height,
                                 // b = fit_host << 48 | prefer << 40 | fps << 32 | kbps
    KVM_EV_VIDEO_STOPPED = 15,
    KVM_EV_KEYFRAME_REQUESTED = 16,
    KVM_EV_VIDEO_FRAME = 17,  // data/data_len = encoded frame, flag = keyframe
    KVM_EV_POINTER_ABS = 18,  // a = display << 16 | x, b = y; x, y in 0..65535, display 0 = main
    KVM_EV_VIDEO_BITRATE = 19, // encode at a = kbit/s from now on (adapted to the link)

    KVM_EV_CLIPBOARD = 30,    // data/data_len = clipboard payload from the peer
    KVM_EV_DISPLAY_LIST = 36, // data/data_len = the viewed peer's displays (JSON)
    KVM_EV_CONFINE = 37,      // controlled: keep the pointer on display a (0xFFFFFFFF: anywhere)
    KVM_EV_EDGE_EXIT = 38,    // controlling: peer pointer pushed past edge a (0 left, 1 right,
                              // 2 top, 3 bottom) at b = 0..65535 along it
    KVM_EV_TEXT = 39,         // controlled: type text (layout-independent)
    KVM_EV_GESTURE = 40,      // controlled: a = kind << 8 | phase, b = value (see kvm_send_gesture)
    // File transfers; `id` is the transfer id (decimal). For receive events, flag = the
    // files were copied on the peer (cached for pasting in <clipboard dir>/<id>/).
    KVM_EV_FILE_SEND_PROGRESS = 31,    // a = bytes sent, b = total
    KVM_EV_FILE_SEND_DONE = 32,        // flag = delivered; text = error otherwise
    KVM_EV_FILE_RECEIVE_PROGRESS = 33, // text = file name, a = bytes received, b = total; flag = clipboard
    KVM_EV_FILE_RECEIVED = 34,         // text = saved path, a = index, b = count; flag = clipboard
    KVM_EV_FILE_RECEIVE_FAILED = 35,   // text = "name: error"; flag = clipboard

    // Machine-level events (kvm_node_watch); `id` is the other machine's id.
    KVM_EV_DISCOVERED = 20,   // a KVM machine on this network; text = name
    KVM_EV_EXPIRED = 21,      // it is gone
    KVM_EV_PAIR_REQUEST = 22, // it asks to pair; text = name, a = 6-digit code,
                              // flag = as a remote control (trusted next to the paired peer)
    KVM_EV_PAIR_RESULT = 23,  // answer to kvm_pair_request; flag = accepted, text = name, or the
                              // error when a = 1
};

typedef struct {
    uint32_t kind;
    int64_t a;
    int64_t b;
    bool flag;
    const char *text; // NULL or NUL-terminated; valid only during the callback
    const char *id;   // machine-level and file events; same lifetime as text
    const uint8_t *data; // KVM_EV_VIDEO_FRAME only; same lifetime as text
    size_t data_len;
} KvmEvent;

// Called in order, on a background thread.
typedef void (*KvmEventCallback)(void *ctx, const KvmEvent *event);

void kvm_init_logging(const char *path);
void kvm_log(uint32_t level, const char *message); // 0 info, 1 warn, 2 error

KvmNode *kvm_node_start(const char *state_dir, const char *name, char *err, size_t err_len);
void kvm_node_id(const KvmNode *node, char *buf, size_t len); // len >= 65
bool kvm_node_set_peer(const KvmNode *node, const char *peer, KvmEventCallback callback, void *ctx);
void kvm_node_clear_peer(const KvmNode *node); // no callbacks run after this returns
// Remote controls (phone, tablet) always dial; computers accept them next to their peer.
bool kvm_node_dial_peer(const KvmNode *node, const char *peer, KvmEventCallback callback, void *ctx);
// A second handle on node for the remote-control link; every per-link function works on it.
// Free it (kvm_node_free) before node.
KvmNode *kvm_node_remotes(const KvmNode *node);
// On a kvm_node_remotes handle: accept these ids, one session at a time. count 0 stops.
bool kvm_node_set_remotes(const KvmNode *node, const char *const *ids, size_t count,
                          KvmEventCallback callback, void *ctx);
void kvm_node_free(KvmNode *node);

// Discovery and pairing. Callbacks run on a background thread until the node is freed.
bool kvm_node_watch(const KvmNode *node, KvmEventCallback callback, void *ctx); // once
void kvm_set_discoverable(const KvmNode *node, bool on); // pairing mode, off by default
bool kvm_pair_request(const KvmNode *node, const char *peer);
bool kvm_pair_request_remote(const KvmNode *node, const char *peer); // as a remote control
bool kvm_pair_respond(const KvmNode *node, const char *peer, bool accept);
bool kvm_pairing_code(const KvmNode *node, const char *peer, char *buf, size_t len); // len >= 7

// Non-blocking; safe to call from an event tap callback.
void kvm_enter(const KvmNode *node);
void kvm_leave(const KvmNode *node);
bool kvm_is_connected(const KvmNode *node);
bool kvm_is_controlling(const KvmNode *node);
void kvm_send_motion(const KvmNode *node, int64_t dx, int64_t dy, int64_t scroll_x, int64_t scroll_y);
void kvm_send_key(const KvmNode *node, uint16_t code, bool down);
void kvm_send_button(const KvmNode *node, uint8_t button, bool down);
void kvm_send_consumer(const KvmNode *node, uint16_t usage, bool down);
void kvm_send_wheel(const KvmNode *node, int16_t dx, int16_t dy); // mouse wheel notches
void kvm_send_text(const KvmNode *node, const char *text); // UTF-8, typed whatever the layout
// Pinch or rotation: kind 0 magnify, 1 rotate; phase 1 began, 2 changed, 4 ended;
// value = magnification change x 10000, or degrees counterclockwise x 100.
void kvm_send_gesture(const KvmNode *node, uint8_t kind, uint8_t phase, int32_t value);

// Screen viewing.
// prefer: 0 balanced, 1 keep resolution (drop frame rate first), 2 keep frame rate.
// fit_host: re-lay out the host's main display instead of scaling: 1 at max_width x max_height,
// 2 at that aspect ratio but the host's own pixel count (0: off).
// Calling again while streaming updates the stream in place.
void kvm_video_request(const KvmNode *node, uint16_t max_width, uint16_t max_height, uint8_t fps,
                       uint32_t bitrate_kbps, uint8_t prefer, uint32_t display, uint8_t fit_host);
void kvm_video_stop(const KvmNode *node);
bool kvm_video_can_send(const KvmNode *node); // ask before encoding; skip the frame on false
bool kvm_video_send_frame(const KvmNode *node, const uint8_t *data, size_t len, bool keyframe); // copies data
void kvm_pointer_abs(const KvmNode *node, uint16_t x, uint16_t y, uint32_t display);
void kvm_send_display_list(const KvmNode *node, const uint8_t *data, size_t len);
void kvm_confine(const KvmNode *node, uint32_t display); // 0xFFFFFFFF: not confined
void kvm_edge_exit(const KvmNode *node, uint8_t edge, uint16_t along);

// Clipboard and files.
void kvm_send_clipboard(const KvmNode *node, const uint8_t *data, size_t len); // copies data
// clipboard: the files were copied here, for pasting on the peer. Returns 0 if not connected.
uint64_t kvm_send_files(const KvmNode *node, const char *const *paths, size_t count, bool clipboard);
void kvm_set_clipboard_dir(const char *path);
void kvm_set_download_dir(const char *path);

#endif
