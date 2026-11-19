import AppKit
import os

enum Mode {
    case local
    /// This Mac's mouse and keyboard drive the peer.
    case controllingPeer
    /// The peer's mouse and keyboard drive this Mac.
    case controlledByPeer
}

/// What the UI shows. Main thread only.
final class AppState: ObservableObject {
    @Published var connected = false
    @Published var peerName: String?
    @Published var direct: Bool?
    @Published var latencyMs: Double?
    @Published var mode = Mode.local
    @Published var stalled = false
    @Published var tapRunning = false
    /// The peer is viewing this screen.
    @Published var sharingScreen = false
    /// This Mac is viewing the peer's screen.
    @Published var viewingPeer = false
}

/// Glues capture, injection and the core together. Input paths run on the tap thread and
/// the core's event thread; the mode lock keeps them consistent.
final class Controller {
    let core: Core
    let capture: InputCapture
    let state = AppState()
    let screenHost: ScreenHost
    let files: FileTransfers
    private let clipboard: ClipboardSync
    private let viewer = ScreenViewer()
    private let audio = AudioPlayer()
    /// This Mac's microphone, sent while controlling the peer (if the user wants that).
    private let microphone = MicrophoneSender()
    private let indicator = ControlIndicator()
    private let injector = Injector()
    private let mode = OSAllocatedUnfairLock(initialState: Mode.local)
    /// Control was taken from the screen viewer (confined to the viewed display).
    private let viaViewer = OSAllocatedUnfairLock(initialState: false)
    private let latency = OSAllocatedUnfairLock(initialState: (ema: 0.0, published: Date.distantPast))

    init(core: Core) {
        self.core = core
        capture = InputCapture(core: core)
        screenHost = ScreenHost(core: core)
        files = FileTransfers(core: core)
        clipboard = ClipboardSync(core: core)
        capture.setHotkey(Settings.hotkey)
        capture.onHotkey = { [unowned self] in toggle() }
        core.onEvent = { [unowned self] in handle($0) }
        microphone.onPayload = { [core] in core.sendMicrophone($0) }
        screenHost.onSharingChanged = { [unowned self] in
            state.sharingScreen = $0
            updateIndicator()
        }
        indicator.onTakeBack = { [unowned self] in
            if self.core.takeBack() { Log.info("took this Mac back from the peer") }
        }
        ScreenLock.observe { [unowned self] in
            if state.connected, Settings.lockTogether { self.core.sendLock() }
        }
        viewer.onClose = { [unowned self] in
            self.core.stopVideo()
            audio.stop()
            state.viewingPeer = false
        }
        viewer.onClick = { [unowned self] point in controlFromViewer(at: point) }
        viewer.onPictureMoved = { [unowned self] in updateHoverRect() }
        capture.onHoverEnter = { [unowned self] location in
            DispatchQueue.main.async { self.hoverEntered(at: location) }
        }
        injector.onEdgeExit = { [unowned self] edge, along in self.core.edgeExit(edge: edge, along: along) }
        viewer.onDecodeFailure = { [unowned self] in self.core.requestVideo(videoParams()) }
        viewer.onStreamSettingsChanged = { [unowned self] in
            if !Settings.viewerAudio { audio.stop() }
            if state.viewingPeer { self.core.requestVideo(videoParams()) }
        }
        viewer.onDrop = { [unowned self] urls in files.send(urls) }
        clipboard.onNotice = { [unowned self] message in files.report(message) }
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [unowned self] _ in screensChanged() }
        clipboard.start()
    }

    /// Opens the window with the peer's screen. Main thread only.
    func viewPeerScreen() {
        guard state.connected else { NSSound.beep(); return }
        let peer = state.peerName ?? "Other computer"
        viewer.show(title: "\(peer) — click to control, \(Settings.hotkey.display) to return")
        state.viewingPeer = true
        core.requestVideo(videoParams())
    }

    /// What to ask the peer for, from the viewer's settings and window. Main thread only.
    private func videoParams() -> VideoParams {
        // Without matching, ask for up to 4K; the peer never sends more than its display has.
        let size = Settings.viewerMatchesWindow || viewer.fitsHost ? (viewer.videoPixelSize ?? CGSize(width: 1920, height: 1200))
                                                                   : CGSize(width: 3840, height: 2400)
        var params = Settings.viewerQuality.params(maxSize: size)
        params.display = viewer.selectedDisplay
        params.fitHost = viewer.fitsHost ? .size : .off
        params.hevc = VideoFrames.decodesHEVC
        params.audio = Settings.viewerAudio
        return params
    }

    /// Displays plugged in, removed or rearranged. Main thread.
    private func screensChanged() {
        guard state.sharingScreen else { return }
        core.sendDisplayList(Displays.encode(Displays.current()))
        screenHost.displaysChanged()
    }

    func stopViewing() {
        viewer.close()
    }

    /// Main thread: tell the event tap where the viewer's picture is.
    private func updateHoverRect() {
        capture.setHoverRect(Settings.viewerHoverControl && state.connected ? viewer.pictureRect : nil)
    }

    /// Main thread: the pointer entered the picture.
    private func hoverEntered(at location: CGPoint) {
        guard Settings.viewerHoverControl, NSEvent.pressedMouseButtons == 0,
              viewer.isFrontmost(at: location), let point = viewer.fraction(at: location) else { return }
        controlFromViewer(at: point)
    }

    /// Takes control like the hotkey does, from a point in the viewer's picture: the peer's
    /// pointer starts there, stays on the viewed display, and leaving it hands control back.
    private func controlFromViewer(at point: CGPoint) {
        if mode.withLock({ $0 }) == .local {
            controlPeer()
        }
        guard mode.withLock({ $0 }) == .controllingPeer else { return }
        viaViewer.withLock { $0 = true }
        core.pointerAbs(x: point.x, y: point.y, display: viewer.selectedDisplay)
        core.confine(display: viewer.selectedDisplay)
    }

    func toggle() {
        let current = mode.withLock { $0 }
        Log.info("switch requested: mode=\(current) connected=\(core.isConnected)")
        switch current {
        case .local: controlPeer()
        case .controllingPeer: returnToLocal(notifyPeer: true)
        case .controlledByPeer: break // the hotkey belongs to the machine in control
        }
    }

    func controlPeer() {
        guard core.isConnected else {
            // Asleep, perhaps: try waking it (it connects back once it is up).
            if Settings.wakePeer, let mac = Settings.peerWakeAddress, Core.wake(mac: mac) {
                Log.info("peer not connected: sent it a wake-on-LAN packet")
            } else {
                Log.warn("can't control the peer: not connected")
                NSSound.beep()
            }
            return
        }
        let switched = mode.withLock { m -> Bool in
            guard m == .local else { return false }
            m = .controllingPeer
            return true
        }
        guard switched else { return }
        capture.beginRemote()
        core.enter()
        Cursor.detach()
        Log.info("controlling the peer")
        publish(mode: .controllingPeer)
    }

    func returnToLocal(notifyPeer: Bool) {
        let switched = mode.withLock { m -> Bool in
            guard m == .controllingPeer else { return false }
            m = .local
            return true
        }
        guard switched else { return }
        capture.endRemote()
        capture.disarmHover()
        viaViewer.withLock { $0 = false }
        if notifyPeer { core.leave() }
        Cursor.attach()
        Log.info("back to this Mac")
        publish(mode: .local)
    }

    private func releaseControl() {
        let wasControlled = mode.withLock { m -> Bool in
            guard m == .controlledByPeer else { return false }
            m = .local
            return true
        }
        if wasControlled {
            injector.end()
            Microphone.sink.stop()
            publish(mode: .local)
        }
    }

    private func handle(_ event: CoreEvent) {
        switch event {
        case .move(let dx, let dy):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.move(dx: dx, dy: dy) }
        case .scroll(let dx, let dy):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.scroll(dx: dx, dy: dy) }
        case .key(let hid, let down):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.key(hid: hid, down: down) }
        case .button(let b, let down):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.button(b, down: down) }
        case .wheel(let dx, let dy):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.wheel(dx: dx, dy: dy) }
        case .consumer(let usage, let down):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.consumer(usage: usage, down: down) }
        case .text(let text):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.type(text) }
        case .gesture(let g):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.gesture(g) }
        case .entered:
            Log.info("peer took control (accessibility=\(Permissions.accessibility))")
            if !Permissions.accessibility {
                Log.warn("Accessibility not granted: macOS drops the peer's input on this Mac")
            }
            returnToLocal(notifyPeer: true)
            mode.withLock { $0 = .controlledByPeer }
            injector.begin()
            Wake.display()
            publish(mode: .controlledByPeer)
        case .left:
            Log.info("peer released control")
            releaseControl()
        case .videoRequested(let params):
            Log.info("peer asked to view this screen")
            DispatchQueue.main.async {
                self.screenHost.start(params)
                self.core.sendDisplayList(Displays.encode(Displays.current()))
            }
        case .videoStopped:
            DispatchQueue.main.async { self.screenHost.stop() }
        case .keyframeRequested:
            screenHost.requestKeyframe()
        case .videoBitrate(let kbps):
            screenHost.setBitrate(kbps)
        case .clipboard(let payload):
            DispatchQueue.main.async { self.clipboard.received(payload) }
        // Copied files are tracked on this thread: a paste waiting for them blocks main.
        case .fileReceived(let transfer, _, _, let count, true):
            clipboard.fileArrived(transfer: transfer, count: count)
        case .fileReceiveFailed(let transfer, let message, true):
            clipboard.fileFailed(transfer: transfer, message: message)
        case .fileReceiveProgress(_, _, _, _, true):
            break
        case .fileSendProgress, .fileSendDone, .fileReceiveProgress, .fileReceived, .fileReceiveFailed:
            DispatchQueue.main.async { self.files.handle(event) }
        case .videoFrame(let data, let keyframe):
            viewer.handle(frame: data, keyframe: keyframe)
        case .audio(let data):
            if Settings.viewerAudio { audio.handle(data) }
        case .microphone(let data):
            if mode.withLock({ $0 }) == .controlledByPeer { Microphone.sink.handle(data) }
        case .lockRequested:
            guard Settings.lockTogether else { break }
            returnToLocal(notifyPeer: true) // nothing to control on a locked screen
            DispatchQueue.main.async { ScreenLock.lock() }
        case .wakeAddress(let mac):
            DispatchQueue.main.async { Settings.peerWakeAddress = mac }
        case .takenBack:
            Log.info("the peer took their computer back")
            returnToLocal(notifyPeer: false)
            DispatchQueue.main.async { self.stopViewing() }
        case .pointerAbs(let x, let y, let display):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.warp(x: x, y: y, display: display) }
        case .confine(let display):
            if mode.withLock({ $0 }) == .controlledByPeer { injector.confine(display: display) }
        case .edgeExit(let edge, let along):
            guard viaViewer.withLock({ $0 }) else { break }
            returnToLocal(notifyPeer: true)
            DispatchQueue.main.async { self.viewer.placeCursorOutside(edge: edge, along: along) }
        case .displayList(let data):
            DispatchQueue.main.async { self.viewer.setDisplays(Displays.decode(data)) }
        case .latency(let micros):
            let ms = Double(micros) / 1000
            let now = Date()
            let publishValue = latency.withLock { l -> Double? in
                l.ema = l.ema == 0 ? ms : l.ema * 0.9 + ms * 0.1
                guard now.timeIntervalSince(l.published) >= 1 else { return nil }
                l.published = now
                return l.ema
            }
            if let value = publishValue {
                DispatchQueue.main.async { self.state.latencyMs = value }
            }
        case .connected(let name, _):
            DispatchQueue.main.async {
                self.state.connected = true
                self.state.peerName = name
                self.state.stalled = false
                self.clipboard.connected = true
                self.updateHoverRect()
                if let mac = Wake.macAddress() { self.core.sendWakeAddress(mac) }
                if self.state.viewingPeer {
                    self.viewer.setStatus("Waiting for the screen…")
                    self.core.requestVideo(self.videoParams())
                }
            }
        case .path(let direct, _):
            DispatchQueue.main.async { self.state.direct = direct }
        case .stalled:
            returnToLocal(notifyPeer: false)
            releaseControl()
            DispatchQueue.main.async { self.state.stalled = true }
        case .recovered:
            DispatchQueue.main.async { self.state.stalled = false }
        case .disconnected:
            returnToLocal(notifyPeer: false)
            releaseControl()
            latency.withLock { $0.ema = 0 }
            DispatchQueue.main.async {
                self.state.connected = false
                self.state.direct = nil
                self.state.latencyMs = nil
                self.state.stalled = false
                self.clipboard.connected = false
                self.updateHoverRect()
                self.screenHost.stop()
                self.audio.stop()
                if self.state.viewingPeer { self.viewer.setStatus("Disconnected, reconnecting…") }
            }
        }
    }

    private func publish(mode: Mode) {
        DispatchQueue.main.async {
            self.state.mode = mode
            self.updateIndicator()
            self.updateMicrophone()
        }
    }

    /// Main thread: the microphone goes to the peer while controlling it, if wanted.
    func updateMicrophone() {
        if Settings.sendMicrophone, state.mode == .controllingPeer {
            microphone.start { granted in
                if !granted { Log.warn("microphone access denied: allow KVM in System Settings > Privacy & Security > Microphone") }
            }
        } else {
            microphone.stop()
        }
    }

    /// Main thread: show who uses this Mac, if anyone.
    private func updateIndicator() {
        indicator.update(name: state.peerName ?? "The other computer",
                         controlling: state.mode == .controlledByPeer, viewing: state.sharingScreen)
    }

    /// Forgets the peer. Main thread only.
    func clearPeer() {
        stopViewing()
        screenHost.stop()
        returnToLocal(notifyPeer: true)
        releaseControl()
        core.clearPeer()
        state.connected = false
        state.peerName = nil
        state.direct = nil
        state.latencyMs = nil
    }

    /// Re-pairs with a new peer id. Main thread only.
    func setPeer(_ id: String) -> Bool {
        guard id.count == 64, id.allSatisfy(\.isHexDigit), id != core.id else { return false }
        stopViewing()
        screenHost.stop()
        returnToLocal(notifyPeer: true)
        releaseControl()
        core.clearPeer()
        state.connected = false
        state.peerName = nil
        state.direct = nil
        state.latencyMs = nil
        return core.setPeer(id)
    }
}

/// Freezes and hides the cursor while input goes to the peer.
enum Cursor {
    private static var hidden = false

    static func detach() {
        CGAssociateMouseAndMouseCursorPosition(0)
        DispatchQueue.main.async {
            guard !hidden else { return }
            hidden = true
            allowBackgroundCursorChanges()
            CGDisplayHideCursor(CGMainDisplayID())
        }
    }

    static func attach() {
        CGAssociateMouseAndMouseCursorPosition(1)
        DispatchQueue.main.async {
            guard hidden else { return }
            hidden = false
            CGDisplayShowCursor(CGMainDisplayID())
        }
    }

    /// Without this, a background (menu bar) app can't hide the cursor. Private API, used
    /// the same way by other input-sharing tools.
    private static func allowBackgroundCursorChanges() {
        _ = CGSSetConnectionProperty(_CGSDefaultConnection(), _CGSDefaultConnection(),
                                     "SetsCursorInBackground" as CFString, kCFBooleanTrue)
    }
}

@_silgen_name("_CGSDefaultConnection")
private func _CGSDefaultConnection() -> Int32

@_silgen_name("CGSSetConnectionProperty")
private func CGSSetConnectionProperty(_ cid: Int32, _ targetCID: Int32, _ key: CFString, _ value: CFTypeRef) -> Int32
