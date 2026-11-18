import CKvm
import Foundation

/// Events from the Rust core. Delivered in order on a background thread.
enum CoreEvent {
    case connected(name: String, peer: String)
    case path(direct: Bool, rttMicros: Int64)
    case latency(micros: Int64)
    case entered
    case left
    case move(dx: Int64, dy: Int64)
    case scroll(dx: Int64, dy: Int64)
    case key(hid: UInt16, down: Bool)
    case button(Int, down: Bool)
    case consumer(usage: UInt16, down: Bool)
    case wheel(dx: Int16, dy: Int16)
    /// Text to type, whatever this machine's keyboard layout.
    case text(String)
    case gesture(Gesture)
    case videoRequested(VideoParams)
    case videoStopped
    case keyframeRequested
    case videoFrame(Data, keyframe: Bool)
    case pointerAbs(x: UInt16, y: UInt16, display: UInt32)
    case displayList(Data)
    case confine(display: UInt32)
    case edgeExit(edge: Int, along: UInt16)
    case videoBitrate(kbps: Int)
    case clipboard(Data)
    case fileSendProgress(transfer: UInt64, sent: Int64, total: Int64)
    case fileSendDone(transfer: UInt64, error: String?)
    /// `clipboard`: files copied on the peer, cached for pasting here (not downloads).
    case fileReceiveProgress(transfer: UInt64, name: String, received: Int64, total: Int64, clipboard: Bool)
    case fileReceived(transfer: UInt64, path: String, index: Int, count: Int, clipboard: Bool)
    case fileReceiveFailed(transfer: UInt64, message: String, clipboard: Bool)
    case stalled
    case recovered
    case disconnected(reason: String)
}

/// Machine-level events: discovery on this network and pairing. Background thread.
enum MachineEvent {
    case discovered(id: String, name: String)
    case expired(id: String)
    /// `remote`: a phone or tablet asking to control this computer, trusted next to the
    /// paired computer rather than replacing it.
    case pairRequest(id: String, name: String, code: String, remote: Bool)
    /// `detail` is the other machine's name, or the error if it `failed`.
    case pairResult(id: String, accepted: Bool, detail: String, failed: Bool)
}

struct VideoParams: Equatable {
    var maxWidth: Int
    var maxHeight: Int
    var fps: Int
    /// A ceiling: the host adapts below it to what the link carries.
    var bitrateKbps: Int
    var prefer: VideoPreference = .balanced
    /// One of the host's displays (an id from its display list); 0 is its main display.
    var display: UInt32 = 0
}

/// What the host gives up first when bits run short.
enum VideoPreference: Int {
    case balanced = 0
    /// Keep resolution, lower the frame rate: sharp text.
    case resolution = 1
    /// Keep the frame rate, lower resolution: smooth motion.
    case frameRate = 2
}

/// The viewer's quality presets.
enum VideoQuality: String, CaseIterable {
    case auto, sharp, smooth, saver

    var title: String {
        switch self {
        case .auto: "Auto"
        case .sharp: "Sharp text"
        case .smooth: "Smooth motion"
        case .saver: "Data saver"
        }
    }

    /// Parameters for a picture of at most `size` pixels.
    func params(maxSize size: CGSize) -> VideoParams {
        let (w, h) = (Int(size.width), Int(size.height))
        return switch self {
        case .auto: VideoParams(maxWidth: w, maxHeight: h, fps: 60, bitrateKbps: 40_000, prefer: .balanced)
        case .sharp: VideoParams(maxWidth: w, maxHeight: h, fps: 30, bitrateKbps: 40_000, prefer: .resolution)
        case .smooth: VideoParams(maxWidth: w, maxHeight: h, fps: 60, bitrateKbps: 40_000, prefer: .frameRate)
        case .saver: VideoParams(maxWidth: w, maxHeight: h, fps: 30, bitrateKbps: 10_000, prefer: .balanced)
        }
    }
}

/// One of the host's displays, as told to a viewer (see docs/protocol.md).
struct DisplayInfo: Codable, Equatable {
    let id: UInt32
    let name: String
    let width: Int
    let height: Int
    let main: Bool
}

/// A pinch or rotation (see `kvm_send_gesture`).
struct Gesture {
    enum Kind: UInt8 {
        case magnify = 0, rotate = 1
    }

    enum Phase: UInt8 {
        case began = 1, changed = 2, ended = 4
    }

    let kind: Kind
    let phase: Phase
    /// Magnification change, or degrees counterclockwise; an increment.
    let value: Double
}

struct CoreError: Error, CustomStringConvertible {
    let description: String
}

/// Swift wrapper over the C ABI in ffi/include/kvm.h.
final class Core {
    private let node: OpaquePointer
    /// For a `remotesOf` handle: the node it shares, kept alive (and freed) after it.
    private let parent: Core?
    let id: String
    /// Set before calling `setPeer`. Runs on the core's event thread.
    var onEvent: ((CoreEvent) -> Void)?
    /// Set before calling `watchMachines`. Runs on a background thread.
    var onMachineEvent: ((MachineEvent) -> Void)?

    init(stateDir: URL, logFile: URL, name: String) throws {
        kvm_init_logging(logFile.path)
        var err = [CChar](repeating: 0, count: 512)
        guard let node = kvm_node_start(stateDir.path, name, &err, err.count) else {
            throw CoreError(description: String(cString: err))
        }
        self.node = node
        parent = nil
        var buf = [CChar](repeating: 0, count: 80)
        kvm_node_id(node, &buf, buf.count)
        id = String(cString: buf)
    }

    /// A second handle on `core`'s node for the link to remote controls (`setRemotes`). It
    /// has the same id and every per-link method; `onEvent` gets that link's events.
    init(remotesOf core: Core) {
        node = kvm_node_remotes(core.node)
        parent = core
        id = core.id
    }

    deinit {
        kvm_node_free(node)
    }

    private static let deliver: KvmEventCallback = { ctx, event in
        guard let ctx, let event else { return }
        let core = Unmanaged<Core>.fromOpaque(ctx).takeUnretainedValue()
        core.onEvent?(CoreEvent(event.pointee))
    }

    /// Starts the link to `peer`. Returns false if the id is malformed. Main thread only.
    func setPeer(_ peer: String) -> Bool {
        kvm_node_set_peer(node, peer, Core.deliver, Unmanaged.passUnretained(self).toOpaque())
    }

    /// Like `setPeer`, always dialing: how a remote control (phone, tablet) connects.
    func dialPeer(_ peer: String) -> Bool {
        kvm_node_dial_peer(node, peer, Core.deliver, Unmanaged.passUnretained(self).toOpaque())
    }

    /// On a `remotesOf` handle: accept these remote controls, one at a time (none: stop).
    /// Returns false if an id is malformed. Main thread only.
    func setRemotes(_ ids: [String]) -> Bool {
        let strings = ids.map { strdup($0) }
        defer { strings.forEach { free($0) } }
        let pointers = strings.map { UnsafePointer($0) }
        return pointers.withUnsafeBufferPointer {
            kvm_node_set_remotes(node, $0.baseAddress, $0.count, Core.deliver, Unmanaged.passUnretained(self).toOpaque())
        }
    }

    func clearPeer() {
        kvm_node_clear_peer(node)
    }

    /// Starts delivering discovery and pairing events to `onMachineEvent`.
    func watchMachines() {
        let ctx = Unmanaged.passUnretained(self).toOpaque()
        _ = kvm_node_watch(node, { ctx, event in
            guard let ctx, let event, let e = MachineEvent(event.pointee) else { return }
            Unmanaged<Core>.fromOpaque(ctx).takeUnretainedValue().onMachineEvent?(e)
        }, ctx)
    }

    /// Pairing mode: visible to nearby computers and accepting pairing requests.
    func setDiscoverable(_ on: Bool) { kvm_set_discoverable(node, on) }

    /// `remote`: ask as a remote control (see `MachineEvent.pairRequest`).
    func requestPairing(_ peer: String, remote: Bool = false) -> Bool {
        remote ? kvm_pair_request_remote(node, peer) : kvm_pair_request(node, peer)
    }
    func respondPairing(_ peer: String, accept: Bool) -> Bool { kvm_pair_respond(node, peer, accept) }

    func pairingCode(_ peer: String) -> String? {
        var buf = [CChar](repeating: 0, count: 8)
        return kvm_pairing_code(node, peer, &buf, buf.count) ? String(cString: buf) : nil
    }

    var isConnected: Bool { kvm_is_connected(node) }

    func enter() { kvm_enter(node) }
    func leave() { kvm_leave(node) }

    func sendMotion(dx: Int64, dy: Int64, scrollX: Int64 = 0, scrollY: Int64 = 0) {
        kvm_send_motion(node, dx, dy, scrollX, scrollY)
    }

    func sendKey(hid: UInt16, down: Bool) { kvm_send_key(node, hid, down) }
    func sendButton(_ button: Int, down: Bool) { kvm_send_button(node, UInt8(clamping: button), down) }
    func sendConsumer(usage: UInt16, down: Bool) { kvm_send_consumer(node, usage, down) }
    func sendWheel(dx: Int16, dy: Int16) { kvm_send_wheel(node, dx, dy) }
    func sendText(_ text: String) { kvm_send_text(node, text) }

    func sendGesture(_ g: Gesture) {
        let scale = g.kind == .magnify ? 10_000.0 : 100.0
        kvm_send_gesture(node, g.kind.rawValue, g.phase.rawValue, Int32(clamping: Int((g.value * scale).rounded())))
    }

    func requestVideo(_ p: VideoParams) {
        kvm_video_request(node, UInt16(clamping: p.maxWidth), UInt16(clamping: p.maxHeight),
                          UInt8(clamping: p.fps), UInt32(clamping: p.bitrateKbps), UInt8(p.prefer.rawValue), p.display)
    }

    func stopVideo() { kvm_video_stop(node) }

    /// Whether the link can take another frame now; skip encoding when it can't.
    func videoCanSend() -> Bool { kvm_video_can_send(node) }

    /// False if refused: the next frame must be a keyframe.
    func sendVideoFrame(_ data: Data, keyframe: Bool) -> Bool {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return false }
            return kvm_video_send_frame(node, base, raw.count, keyframe)
        }
    }

    func sendClipboard(_ data: Data) {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            kvm_send_clipboard(node, base, raw.count)
        }
    }

    /// Returns the transfer id, or nil when not connected.
    func sendFiles(_ urls: [URL], clipboard: Bool = false) -> UInt64? {
        let paths = urls.map { strdup($0.path) }
        defer { paths.forEach { free($0) } }
        let pointers = paths.map { UnsafePointer($0) }
        let transfer = pointers.withUnsafeBufferPointer { kvm_send_files(node, $0.baseAddress, $0.count, clipboard) }
        return transfer == 0 ? nil : transfer
    }

    static func setDownloadDirectory(_ url: URL) {
        kvm_set_download_dir(url.path)
    }

    static func setClipboardDirectory(_ url: URL) {
        kvm_set_clipboard_dir(url.path)
    }

    /// While controlling: moves the peer's pointer to a point (0...1) on one of its displays.
    func pointerAbs(x: Double, y: Double, display: UInt32) {
        let scale = { (v: Double) in UInt16(clamping: Int((min(max(v, 0), 1) * 65535).rounded())) }
        kvm_pointer_abs(node, scale(x), scale(y), display)
    }

    /// While controlling: keep the peer's pointer on one of its displays and report edge
    /// exits (`UInt32.max`: lift it).
    func confine(display: UInt32) { kvm_confine(node, display) }

    /// While controlled and confined: the pointer was pushed past an edge.
    func edgeExit(edge: Int, along: UInt16) { kvm_edge_exit(node, UInt8(edge), along) }

    /// While the peer views this screen: which displays there are.
    func sendDisplayList(_ data: Data) {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            kvm_send_display_list(node, base, raw.count)
        }
    }
}

/// Writes to the shared log file (on macOS ~/Library/Logs/KVM/kvm.log).
enum Log {
    static func info(_ message: String) { kvm_log(0, message) }
    static func warn(_ message: String) { kvm_log(1, message) }
}

private extension CoreEvent {
    init(_ e: KvmEvent) {
        let text = e.text.map { String(cString: $0) } ?? ""
        switch Int(e.kind) {
        case KVM_EV_CONNECTED: self = .connected(name: text, peer: e.id.map { String(cString: $0) } ?? "")
        case KVM_EV_PATH: self = .path(direct: e.flag, rttMicros: e.a)
        case KVM_EV_LATENCY: self = .latency(micros: e.a)
        case KVM_EV_ENTERED: self = .entered
        case KVM_EV_LEFT: self = .left
        case KVM_EV_MOVE: self = .move(dx: e.a, dy: e.b)
        case KVM_EV_SCROLL: self = .scroll(dx: e.a, dy: e.b)
        case KVM_EV_KEY: self = .key(hid: UInt16(truncatingIfNeeded: e.a), down: e.flag)
        case KVM_EV_BUTTON: self = .button(Int(e.a), down: e.flag)
        case KVM_EV_CONSUMER: self = .consumer(usage: UInt16(truncatingIfNeeded: e.a), down: e.flag)
        case KVM_EV_WHEEL: self = .wheel(dx: Int16(truncatingIfNeeded: e.a), dy: Int16(truncatingIfNeeded: e.b))
        case KVM_EV_TEXT: self = .text(text)
        case KVM_EV_GESTURE:
            let kind = Gesture.Kind(rawValue: UInt8(truncatingIfNeeded: e.a >> 8)) ?? .magnify
            let phase = Gesture.Phase(rawValue: UInt8(truncatingIfNeeded: e.a)) ?? .changed
            self = .gesture(Gesture(kind: kind, phase: phase,
                                    value: Double(e.b) / (kind == .magnify ? 10_000 : 100)))
        case KVM_EV_VIDEO_REQUESTED:
            self = .videoRequested(VideoParams(
                maxWidth: Int((e.a >> 16) & 0xFFFF), maxHeight: Int(e.a & 0xFFFF),
                fps: Int((e.b >> 32) & 0xFF), bitrateKbps: Int(e.b & 0xFFFF_FFFF),
                prefer: VideoPreference(rawValue: Int((e.b >> 40) & 0xFF)) ?? .balanced,
                display: UInt32(truncatingIfNeeded: e.a >> 32)))
        case KVM_EV_VIDEO_STOPPED: self = .videoStopped
        case KVM_EV_KEYFRAME_REQUESTED: self = .keyframeRequested
        case KVM_EV_VIDEO_FRAME:
            let data = e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data()
            self = .videoFrame(data, keyframe: e.flag)
        case KVM_EV_POINTER_ABS:
            self = .pointerAbs(x: UInt16(truncatingIfNeeded: e.a), y: UInt16(truncatingIfNeeded: e.b),
                               display: UInt32(truncatingIfNeeded: e.a >> 16))
        case KVM_EV_DISPLAY_LIST: self = .displayList(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_CONFINE: self = .confine(display: UInt32(truncatingIfNeeded: e.a))
        case KVM_EV_EDGE_EXIT: self = .edgeExit(edge: Int(e.a), along: UInt16(truncatingIfNeeded: e.b))
        case KVM_EV_VIDEO_BITRATE: self = .videoBitrate(kbps: Int(e.a))
        case KVM_EV_CLIPBOARD: self = .clipboard(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_STALLED: self = .stalled
        case KVM_EV_RECOVERED: self = .recovered
        default:
            let transfer = e.id.flatMap { UInt64(String(cString: $0)) } ?? 0
            switch Int(e.kind) {
            case KVM_EV_FILE_SEND_PROGRESS: self = .fileSendProgress(transfer: transfer, sent: e.a, total: e.b)
            case KVM_EV_FILE_SEND_DONE: self = .fileSendDone(transfer: transfer, error: e.flag ? nil : text)
            case KVM_EV_FILE_RECEIVE_PROGRESS:
                self = .fileReceiveProgress(transfer: transfer, name: text, received: e.a, total: e.b, clipboard: e.flag)
            case KVM_EV_FILE_RECEIVED:
                self = .fileReceived(transfer: transfer, path: text, index: Int(e.a), count: Int(e.b), clipboard: e.flag)
            case KVM_EV_FILE_RECEIVE_FAILED: self = .fileReceiveFailed(transfer: transfer, message: text, clipboard: e.flag)
            default: self = .disconnected(reason: text)
            }
        }
    }
}

private extension MachineEvent {
    init?(_ e: KvmEvent) {
        guard let idPtr = e.id else { return nil }
        let id = String(cString: idPtr)
        let text = e.text.map { String(cString: $0) } ?? ""
        switch Int(e.kind) {
        case KVM_EV_DISCOVERED: self = .discovered(id: id, name: text)
        case KVM_EV_EXPIRED: self = .expired(id: id)
        case KVM_EV_PAIR_REQUEST:
            self = .pairRequest(id: id, name: text, code: String(format: "%06lld", e.a), remote: e.flag)
        case KVM_EV_PAIR_RESULT: self = .pairResult(id: id, accepted: e.flag, detail: text, failed: e.a != 0)
        default: return nil
        }
    }
}
