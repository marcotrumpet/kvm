import Combine
import os
import UIKit

/// A computer this device can control.
struct Computer: Codable, Identifiable, Hashable {
    let id: String
    var name: String
}

/// Paired computers, the pairing flow and the live link. Main thread only, except where
/// noted. This device is only ever a remote control: it never takes input from the computer.
final class AppModel: ObservableObject {
    enum LinkState: Equatable {
        case idle, connecting, connected, stalled
    }

    struct PendingPairing: Equatable {
        let id: String
        let name: String
        let code: String
    }

    @Published private(set) var computers: [Computer] = Store.computers
    @Published private(set) var pairing: PendingPairing?
    @Published var message: String?
    /// The computer being controlled (connected or reconnecting).
    @Published private(set) var active: Computer?
    @Published private(set) var link = LinkState.idle
    @Published private(set) var direct: Bool?
    @Published private(set) var latencyMs: Double?
    /// The computer's displays, while viewing its screen.
    @Published private(set) var displays: [DisplayInfo] = []
    /// The display viewed and pointed at (0: the computer's main display).
    @Published var selectedDisplay: UInt32 = 0 {
        didSet {
            guard oldValue != selectedDisplay, var p = videoParams else { return }
            p.display = selectedDisplay
            requestVideo(p)
            confineToViewedDisplay()
        }
    }

    let core: Core?
    let startError: String?
    /// Set while viewing; called on the core's thread with each encoded frame.
    private let frameSink = OSAllocatedUnfairLock<((Data) -> Void)?>(initialState: nil)
    /// What we asked the computer to stream, re-sent after reconnecting.
    private var videoParams: VideoParams?
    private let audio = AudioPlayer()
    private let latency = OSAllocatedUnfairLock(initialState: (ema: 0.0, published: Date.distantPast))

    init() {
        let fm = FileManager.default
        let support = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0].appendingPathComponent("KVM")
        let caches = fm.urls(for: .cachesDirectory, in: .userDomainMask)[0]
        do {
            let core = try Core(stateDir: support, logFile: caches.appendingPathComponent("kvm.log"),
                                name: UIDevice.current.name)
            self.core = core
            startError = nil
            Core.setDownloadDirectory(fm.urls(for: .documentDirectory, in: .userDomainMask)[0])
            Core.setClipboardDirectory(caches.appendingPathComponent("Clipboard"))
        } catch {
            core = nil
            startError = "\(error)"
            return
        }
        core?.onEvent = { [weak self] in self?.handle($0) }
        core?.onMachineEvent = { [weak self] event in DispatchQueue.main.async { self?.handle(event) } }
        core?.watchMachines()
    }

    // MARK: Pairing

    /// Asks the computer to accept this device. Its KVM Settings must be open.
    func pair(id rawID: String, name: String?) {
        let id = rawID.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        guard let core, id.count == 64, id.allSatisfy(\.isHexDigit), id != core.id else {
            message = "That isn't a KVM computer ID."
            return
        }
        guard core.requestPairing(id, remote: true) else { return }
        message = nil
        pairing = PendingPairing(id: id, name: name ?? "the computer", code: core.pairingCode(id) ?? "")
    }

    /// Opens `kvm://pair?id=…&name=…` (the code in the Mac's KVM Settings). False if it isn't one.
    @discardableResult
    func open(_ url: URL) -> Bool {
        guard url.scheme == "kvm", url.host == "pair",
              let items = URLComponents(url: url, resolvingAgainstBaseURL: false)?.queryItems,
              let id = items.first(where: { $0.name == "id" })?.value else { return false }
        pair(id: id, name: items.first(where: { $0.name == "name" })?.value)
        return true
    }

    /// Stops waiting for the answer (a late "allow" on the computer is ignored).
    func cancelPairing() {
        pairing = nil
    }

    func remove(_ computer: Computer) {
        if active?.id == computer.id { disconnect() }
        computers.removeAll { $0.id == computer.id }
        Store.computers = computers
    }

    private func handle(_ event: MachineEvent) {
        guard case .pairResult(let id, let accepted, let detail, let failed) = event, let pending = pairing, pending.id == id else { return }
        pairing = nil
        if accepted {
            let computer = Computer(id: id, name: detail.isEmpty ? pending.name : detail)
            computers.removeAll { $0.id == id }
            computers.append(computer)
            computers.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            Store.computers = computers
            message = nil
            connect(computer)
            MainActor.assumeIsolated { RemotePresenter.present(self) }
        } else if !failed {
            // Declined, or not in pairing mode (it declines without asking).
            message = "\(detail.isEmpty ? pending.name : detail) declined. Open KVM Settings on it and try again."
        } else {
            message = "Couldn't pair with \(pending.name): \(detail)"
        }
    }

    // MARK: Link

    func connect(_ computer: Computer) {
        guard let core else { return }
        if active?.id == computer.id, link != .idle { return }
        disconnect()
        active = computer
        link = .connecting
        _ = core.dialPeer(computer.id)
    }

    func disconnect() {
        core?.leave()
        core?.clearPeer()
        active = nil
        resetLink(to: .idle)
        videoParams = nil
        selectedDisplay = 0
    }

    /// The app went to the background: iOS suspends it, so drop the link cleanly.
    func suspend() {
        guard active != nil else { return }
        core?.leave()
        core?.clearPeer()
        resetLink(to: .connecting)
    }

    func resume() {
        guard let active, let core, link == .connecting, !core.isConnected else { return }
        _ = core.dialPeer(active.id)
    }

    private func resetLink(to state: LinkState) {
        link = state
        direct = nil
        latencyMs = nil
        displays = []
        latency.withLock { $0.ema = 0 }
    }

    // MARK: Screen

    /// Starts (or updates) the stream; `sink` gets each frame on the core's thread.
    func view(_ params: VideoParams, sink: @escaping (Data) -> Void) {
        frameSink.withLock { $0 = sink }
        var params = params
        params.display = selectedDisplay
        requestVideo(params)
    }

    /// Silences the computer's sound until it is asked for again.
    func stopAudio() {
        audio.stop()
    }

    func stopViewing() {
        frameSink.withLock { $0 = nil }
        audio.stop()
        videoParams = nil
        displays = []
        core?.stopVideo()
        core?.confine(display: UInt32.max) // without a picture, the pointer roams freely
    }

    /// With several displays, keep the pointer on the one shown, so it can't vanish off the
    /// picture; pushing past an edge switches displays (`edgeExit`).
    private func confineToViewedDisplay() {
        guard videoParams != nil, displays.count > 1,
              let shown = displays.first(where: { selectedDisplay == 0 ? $0.main : $0.id == selectedDisplay }) else { return }
        core?.confine(display: shown.id)
    }

    /// The pointer was pushed past an edge of the display shown: follow it onto the display
    /// next to it (the list is ordered left to right), entering at the matching spot.
    private func edgeExit(edge: Int, along: UInt16) {
        guard edge == 0 || edge == 1,
              let i = displays.firstIndex(where: { selectedDisplay == 0 ? $0.main : $0.id == selectedDisplay }) else {
            confineToViewedDisplay() // re-arm: no display that way
            return
        }
        let j = edge == 0 ? i - 1 : i + 1
        guard displays.indices.contains(j) else {
            confineToViewedDisplay()
            return
        }
        let next = displays[j]
        core?.pointerAbs(x: edge == 0 ? 0.99 : 0.01, y: Double(along) / 65535, display: next.id)
        selectedDisplay = next.main ? 0 : next.id // re-requests the stream and confines
    }

    /// After a decoding failure: ask again, which starts with a keyframe.
    func restartVideo() {
        if let videoParams { core?.requestVideo(videoParams) }
    }

    private func requestVideo(_ params: VideoParams) {
        videoParams = params
        core?.requestVideo(params)
    }

    // MARK: Clipboard

    /// Puts this device's clipboard on the computer's, for pasting there.
    func sendClipboard() {
        guard let payload = Clipboard.encodeGeneral() else {
            message = "The clipboard is empty."
            return
        }
        core?.sendClipboard(payload)
    }

    // MARK: Core events (core thread)

    private func handle(_ event: CoreEvent) {
        switch event {
        case .videoFrame(let data, _):
            frameSink.withLock { $0 }?(data)
        case .audio(let data):
            if videoParams?.audio == true { audio.handle(data) }
        case .takenBack:
            DispatchQueue.main.async { [self] in
                message = "\(active?.name ?? "The computer") took back control."
                disconnect()
            }
        case .connected(let name, let peer):
            core?.enter()
            DispatchQueue.main.async { [self] in
                link = .connected
                if let i = computers.firstIndex(where: { $0.id == peer }), computers[i].name != name, !name.isEmpty {
                    computers[i].name = name
                    Store.computers = computers
                    if active?.id == peer { active = computers[i] }
                }
                if let videoParams { core?.requestVideo(videoParams) }
            }
        case .recovered:
            core?.enter() // a stall hands control back; take it again
            DispatchQueue.main.async { self.link = .connected }
        case .stalled:
            DispatchQueue.main.async { self.link = .stalled }
        case .disconnected:
            audio.stop()
            DispatchQueue.main.async { self.resetLink(to: self.active == nil ? .idle : .connecting) }
        case .path(let direct, _):
            DispatchQueue.main.async { self.direct = direct }
        case .latency(let micros):
            let ms = Double(micros) / 1000
            let now = Date()
            let value = latency.withLock { l -> Double? in
                l.ema = l.ema == 0 ? ms : l.ema * 0.9 + ms * 0.1
                guard now.timeIntervalSince(l.published) >= 1 else { return nil }
                l.published = now
                return l.ema
            }
            if let value { DispatchQueue.main.async { self.latencyMs = value } }
        case .displayList(let data):
            let list = (try? JSONDecoder().decode([DisplayInfo].self, from: data)) ?? []
            DispatchQueue.main.async { [self] in
                displays = list
                if selectedDisplay != 0, !list.contains(where: { $0.id == selectedDisplay }) { selectedDisplay = 0 }
                confineToViewedDisplay()
            }
        case .edgeExit(let edge, let along):
            DispatchQueue.main.async { self.edgeExit(edge: edge, along: along) }
        case .clipboard(let payload):
            DispatchQueue.main.async { Clipboard.write(payload) }
        default:
            break // this device is never controlled and never hosts its screen
        }
    }
}

private enum Store {
    static var computers: [Computer] {
        get {
            UserDefaults.standard.data(forKey: "computers").flatMap { try? JSONDecoder().decode([Computer].self, from: $0) } ?? []
        }
        set { UserDefaults.standard.set(try? JSONEncoder().encode(newValue), forKey: "computers") }
    }
}
