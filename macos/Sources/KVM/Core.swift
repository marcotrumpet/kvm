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
    case confine(display: UInt32, edges: UInt8)
    case edgeExit(edge: Int, along: UInt16, display: UInt32)
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
    /// One encoded packet of the peer's sound (see `AudioHost`).
    case audio(Data)
    /// The peer's screen locked.
    case lockRequested
    /// The peer's MAC address, to wake it later.
    case wakeAddress(Data)
    /// The peer's user took their machine back: control and viewing ended there.
    case takenBack
    /// One encoded packet of the peer's microphone, while it controls this machine.
    case microphone(Data)
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
    /// While our pairing request waits: the code the other machine shows with it.
    case pairCode(id: String, code: String)
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
    /// Re-lay out the host's main display to the viewer instead of only scaling.
    var fitHost = HostFit.off
    /// The viewer decodes HEVC (the host then uses it if it can encode it).
    var hevc = false
    /// Also stream the host's sound.
    var audio = false
    /// How big the host draws its main display while viewed.
    var textSize = TextSize.standard
}

/// The resolution the host's main display has while viewed, relative to its default (see
/// `VideoParams.text_size`): bigger text and everything else, or more room.
enum TextSize: UInt8, CaseIterable {
    case standard = 0
    case smaller = 1
    case larger = 2
    case largest = 3

    /// Smallest to largest, for menus.
    static let ordered: [TextSize] = [.smaller, .standard, .larger, .largest]

    /// Relative to the display's default size in points.
    var scale: CGFloat {
        switch self {
        case .standard: 1
        case .smaller: 0.8
        case .larger: 1.25
        case .largest: 1.5
        }
    }

    var title: String {
        switch self {
        case .standard: "Default"
        case .smaller: "More space"
        case .larger: "Larger"
        case .largest: "Largest"
        }
    }

    /// The resolution, in points, this gives the host's main display `main` (as listed now,
    /// at `current`); nil when the host doesn't say (Windows, Linux). Rounded like the host.
    func resolution(of main: DisplayInfo?, current: TextSize) -> CGSize? {
        guard let b = main?.bounds, b.width > 0 else { return nil }
        if self == current { return b.size }
        return resolution(default: TextSize.defaultResolution(of: main, current: current) ?? b.size)
    }

    /// What this gives a display whose default resolution is `base`.
    func resolution(default base: CGSize) -> CGSize {
        func side(_ v: CGFloat) -> CGFloat { self == .standard ? v : CGFloat(Int(v / scale) & ~1) }
        return CGSize(width: side(base.width), height: side(base.height))
    }

    /// The default resolution of `main`, listed at `current`.
    static func defaultResolution(of main: DisplayInfo?, current: TextSize) -> CGSize? {
        guard let b = main?.bounds, b.width > 0 else { return nil }
        return CGSize(width: (b.width * current.scale).rounded(), height: (b.height * current.scale).rounded())
    }

    /// "1366 × 1024", or the title when the resolution isn't known.
    func label(for main: DisplayInfo?, current: TextSize) -> String {
        resolution(of: main, current: current).map(label) ?? title
    }

    func label(_ r: CGSize) -> String {
        "\(Int(r.width)) × \(Int(r.height))" + (self == .standard ? " (default)" : "")
    }
}

/// How the host's main display follows the viewer (see `VideoParams.fit_host`).
enum HostFit: UInt8 {
    case off = 0
    /// At the viewer's size.
    case size = 1
    /// At the viewer's aspect ratio, keeping about the host's own pixel count.
    case shape = 2
    /// An extra display at the viewer's shape, next to the host's own (like Sidecar).
    case extend = 3
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

/// One of the peer's displays, as it tells us (see docs/protocol.md).
struct DisplayInfo: Codable, Equatable {
    let id: UInt32
    let name: String
    let width: Int
    let height: Int
    let main: Bool
    /// Where it is on the peer's desktop, in points, top-left origin: x, y, width, height.
    /// Missing from peers that don't say (Windows and Linux so far).
    var frame: [Double]? = nil

    var bounds: CGRect? {
        guard let f = frame, f.count == 4 else { return nil }
        return CGRect(x: f[0], y: f[1], width: f[2], height: f[3])
    }
}

/// A side of a display or desktop, numbered as in `edgeExit`.
enum Edge: Int, CaseIterable {
    case left, right, top, bottom

    var opposite: Edge {
        switch self {
        case .left: .right
        case .right: .left
        case .top: .bottom
        case .bottom: .top
        }
    }

    /// This edge for `confine`.
    var mask: UInt8 { 1 << UInt8(rawValue) }
    static let all: UInt8 = 0x0F
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
        Core.useSystemProxy()
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

    /// Apps don't inherit proxy environment variables: hand the system's HTTPS proxy (System
    /// Settings > Network, or the iOS Wi-Fi settings) to the core that way, unless set.
    private static func useSystemProxy() {
        guard getenv("HTTPS_PROXY") == nil, getenv("HTTP_PROXY") == nil,
              let settings = CFNetworkCopySystemProxySettings()?.takeRetainedValue() as? [String: Any] else { return }
        #if os(macOS)
        let (enable, host, port) = (kCFNetworkProxiesHTTPSEnable, kCFNetworkProxiesHTTPSProxy, kCFNetworkProxiesHTTPSPort)
        #else
        let (enable, host, port) = (kCFNetworkProxiesHTTPEnable, kCFNetworkProxiesHTTPProxy, kCFNetworkProxiesHTTPPort)
        #endif
        guard (settings[enable as String] as? Int) == 1, let proxy = settings[host as String] as? String, !proxy.isEmpty else { return }
        let url = "http://\(proxy):\((settings[port as String] as? Int) ?? 8080)"
        setenv("HTTPS_PROXY", url, 0)
        Log.info("using the system proxy \(url)")
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
                          UInt8(clamping: p.fps), UInt32(clamping: p.bitrateKbps), UInt8(p.prefer.rawValue), p.display,
                          p.fitHost.rawValue, p.hevc ? 1 : 0, p.audio, p.textSize.rawValue)
    }

    /// While the peer views this screen with audio: one encoded packet.
    func sendAudio(_ data: Data) {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            kvm_audio_send(node, base, raw.count)
        }
    }

    /// While controlling the peer: one encoded packet of this device's microphone.
    func sendMicrophone(_ data: Data) {
        data.withUnsafeBytes { raw in
            guard let base = raw.bindMemory(to: UInt8.self).baseAddress else { return }
            kvm_microphone_send(node, base, raw.count)
        }
    }

    /// This screen locked: the peer locks too.
    func sendLock() { kvm_send_lock(node) }

    /// Tells the peer how to wake this machine.
    func sendWakeAddress(_ mac: Data) {
        guard mac.count == 6 else { return }
        mac.withUnsafeBytes { raw in kvm_send_wake_address(node, raw.bindMemory(to: UInt8.self).baseAddress) }
    }

    /// The user here takes this machine back from the peer. False if it wasn't using it.
    @discardableResult
    func takeBack() -> Bool { kvm_take_back(node) }

    /// Sends a Wake-on-LAN packet for `mac` on this network.
    @discardableResult
    static func wake(mac: Data) -> Bool {
        guard mac.count == 6 else { return false }
        return mac.withUnsafeBytes { raw in kvm_wake(raw.bindMemory(to: UInt8.self).baseAddress) }
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

    /// While controlling: keep the peer's pointer on one of its displays (`UInt32.max`: on
    /// its whole desktop) and report pushes past `edges` (`Edge` masks). `UInt32.max` with
    /// no edges lifts it.
    func confine(display: UInt32, edges: UInt8) { kvm_confine(node, display, edges) }

    /// While controlled and confined: the pointer was pushed past an edge of `display`.
    func edgeExit(edge: Int, along: UInt16, display: UInt32) { kvm_edge_exit(node, UInt8(edge), along, display) }

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
                display: UInt32(truncatingIfNeeded: e.a >> 32),
                fitHost: HostFit(rawValue: UInt8(truncatingIfNeeded: e.b >> 48)) ?? .off,
                hevc: (e.b >> 56) & 1 == 1, audio: (e.b >> 57) & 1 == 1,
                textSize: TextSize(rawValue: UInt8(truncatingIfNeeded: (e.b >> 58) & 3)) ?? .standard))
        case KVM_EV_VIDEO_STOPPED: self = .videoStopped
        case KVM_EV_KEYFRAME_REQUESTED: self = .keyframeRequested
        case KVM_EV_VIDEO_FRAME:
            let data = e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data()
            self = .videoFrame(data, keyframe: e.flag)
        case KVM_EV_POINTER_ABS:
            self = .pointerAbs(x: UInt16(truncatingIfNeeded: e.a), y: UInt16(truncatingIfNeeded: e.b),
                               display: UInt32(truncatingIfNeeded: e.a >> 16))
        case KVM_EV_DISPLAY_LIST: self = .displayList(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_CONFINE: self = .confine(display: UInt32(truncatingIfNeeded: e.a), edges: UInt8(truncatingIfNeeded: e.b))
        case KVM_EV_EDGE_EXIT:
            self = .edgeExit(edge: Int(e.a & 0xFF), along: UInt16(truncatingIfNeeded: e.b),
                             display: UInt32(truncatingIfNeeded: e.a >> 8))
        case KVM_EV_VIDEO_BITRATE: self = .videoBitrate(kbps: Int(e.a))
        case KVM_EV_CLIPBOARD: self = .clipboard(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_STALLED: self = .stalled
        case KVM_EV_RECOVERED: self = .recovered
        case KVM_EV_AUDIO: self = .audio(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_LOCK: self = .lockRequested
        case KVM_EV_WAKE_ADDRESS: self = .wakeAddress(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
        case KVM_EV_TAKEN_BACK: self = .takenBack
        case KVM_EV_MICROPHONE: self = .microphone(e.data.map { Data(bytes: $0, count: e.data_len) } ?? Data())
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
        case KVM_EV_PAIR_CODE: self = .pairCode(id: id, code: String(format: "%06lld", e.a))
        default: return nil
        }
    }
}
