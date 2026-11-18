import AppKit
import os

/// What the UI shows about remote controls. Main thread only.
final class RemoteState: ObservableObject {
    /// Paired remote controls: id -> name.
    @Published var paired = Settings.remotes
    /// The one connected now.
    @Published var connectedID: String?
    @Published var sharingScreen = false
}

/// Lets paired remote controls (iPhone, iPad) drive this Mac and view its screen, on their
/// own link next to the one with the paired computer. They only ever control: this Mac
/// never sends them input. Events arrive on the core's event thread.
final class RemoteHost {
    let core: Core
    let state = RemoteState()
    private let injector = Injector()
    private let screenHost: ScreenHost
    private let clipboard: ClipboardSync
    private let controlled = OSAllocatedUnfairLock(initialState: false)

    /// `core`: a `Core(remotesOf:)` handle.
    init(core: Core) {
        self.core = core
        screenHost = ScreenHost(core: core)
        clipboard = ClipboardSync(core: core)
        clipboard.shareFiles = false
        core.onEvent = { [unowned self] in handle($0) }
        injector.onEdgeExit = { [unowned self] edge, along in self.core.edgeExit(edge: edge, along: along) }
        screenHost.onSharingChanged = { [unowned self] in state.sharingScreen = $0 }
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [unowned self] _ in
            guard state.sharingScreen else { return }
            self.core.sendDisplayList(Displays.encode(Displays.current()))
            screenHost.displaysChanged()
        }
        clipboard.start()
        listen()
    }

    /// Trusts a remote control from now on. Main thread only.
    func add(id: String, name: String) {
        state.paired[id] = name
        Settings.remotes = state.paired
        listen()
    }

    /// Main thread only.
    func remove(id: String) {
        state.paired[id] = nil
        Settings.remotes = state.paired
        listen()
    }

    /// Stops screen sharing and drops the connected remote control. Main thread only.
    func stop() {
        screenHost.stop()
        _ = core.setRemotes([])
    }

    private func listen() {
        if !core.setRemotes(Array(state.paired.keys)) {
            Log.warn("invalid remote control id in settings")
        }
        if state.paired.isEmpty { disconnected() }
    }

    private func release() {
        let was = controlled.withLock { c -> Bool in
            defer { c = false }
            return c
        }
        if was { injector.end() }
    }

    private func disconnected() {
        release()
        DispatchQueue.main.async { [self] in
            state.connectedID = nil
            clipboard.connected = false
            screenHost.stop()
        }
    }

    private func handle(_ event: CoreEvent) {
        let isControlled = { self.controlled.withLock { $0 } }
        switch event {
        case .entered:
            Log.info("remote control took control")
            controlled.withLock { $0 = true }
            injector.begin()
        case .left:
            release()
        case .move(let dx, let dy):
            if isControlled() { injector.move(dx: dx, dy: dy) }
        case .scroll(let dx, let dy):
            if isControlled() { injector.scroll(dx: dx, dy: dy) }
        case .key(let hid, let down):
            if isControlled() { injector.key(hid: hid, down: down) }
        case .button(let b, let down):
            if isControlled() { injector.button(b, down: down) }
        case .wheel(let dx, let dy):
            if isControlled() { injector.wheel(dx: dx, dy: dy) }
        case .consumer(let usage, let down):
            if isControlled() { injector.consumer(usage: usage, down: down) }
        case .text(let text):
            if isControlled() { injector.type(text) }
        case .gesture(let g):
            if isControlled() { injector.gesture(g) }
        case .pointerAbs(let x, let y, let display):
            if isControlled() { injector.warp(x: x, y: y, display: display) }
        case .confine(let display):
            if isControlled() { injector.confine(display: display) }
        case .videoRequested(let params):
            Log.info("remote control asked to view this screen")
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
        case .connected(_, let peer):
            DispatchQueue.main.async { [self] in
                state.connectedID = peer
                clipboard.connected = true
            }
        case .stalled:
            release()
        case .disconnected:
            disconnected()
        case .path, .latency, .recovered, .videoFrame, .displayList, .edgeExit, .fileSendProgress, .fileSendDone,
             .fileReceiveProgress, .fileReceived, .fileReceiveFailed:
            break
        }
    }
}
