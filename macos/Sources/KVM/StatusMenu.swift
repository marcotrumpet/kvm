import AppKit
import Combine

/// The menu bar icon and its menu.
final class StatusMenu: NSObject, NSMenuDelegate, NSWindowDelegate {
    private let item = NSStatusBar.system.statusItem(withLength: NSStatusItem.squareLength)
    private let controller: Controller
    private let awdl: Awdl
    private let openSettings: () -> Void
    private var subscriptions = Set<AnyCancellable>()

    init(controller: Controller, awdl: Awdl, openSettings: @escaping () -> Void) {
        self.controller = controller
        self.awdl = awdl
        self.openSettings = openSettings
        super.init()
        let menu = NSMenu()
        menu.delegate = self
        item.menu = menu
        controller.state.objectWillChange
            .receive(on: RunLoop.main)
            .sink { [weak self] in self?.updateIcon() }
            .store(in: &subscriptions)
        // Files dropped onto the menu bar icon are sent to the peer. The status item's
        // window forwards drag events to its delegate.
        item.button?.window?.registerForDraggedTypes([.fileURL])
        item.button?.window?.delegate = self
        updateIcon()
    }

    private func updateIcon() {
        let s = controller.state
        let active = s.mode != .local || s.sharingScreen
        item.button?.image = active ? StatusMenu.activeIcon : StatusMenu.idleIcon
        item.button?.appearsDisabled = !s.connected || s.stalled
    }

    private static let idleIcon = icon(active: false)
    private static let activeIcon = icon(active: true)

    /// The app icon's two screens as a menu bar template; `active` (controlling, controlled
    /// or sharing the screen) fills the front one.
    private static func icon(active: Bool) -> NSImage {
        let image = NSImage(size: NSSize(width: 18, height: 18), flipped: false) { _ in
            let line: CGFloat = 1.5
            let front = NSRect(x: 6, y: 5, width: 11, height: 7.5)
            let back = NSRect(x: 1, y: 8, width: 11, height: 8)
            let frontPath = NSBezierPath(roundedRect: front, xRadius: 1.5, yRadius: 1.5)
            // The back screen stops short of the front one, as in the app icon.
            NSGraphicsContext.saveGraphicsState()
            let clip = NSBezierPath(rect: NSRect(x: 0, y: 0, width: 18, height: 18))
            clip.append(NSBezierPath(roundedRect: front.insetBy(dx: -line, dy: -line), xRadius: 2.5, yRadius: 2.5))
            clip.windingRule = .evenOdd
            clip.addClip()
            let backPath = NSBezierPath(roundedRect: back, xRadius: 1.5, yRadius: 1.5)
            backPath.lineWidth = line
            NSColor.black.withAlphaComponent(0.6).setStroke()
            backPath.stroke()
            NSGraphicsContext.restoreGraphicsState()

            NSColor.black.set()
            frontPath.lineWidth = line
            frontPath.stroke()
            if active { frontPath.fill() }
            let stand = NSBezierPath()
            stand.lineWidth = line
            stand.lineCapStyle = .round
            stand.move(to: NSPoint(x: front.midX, y: front.minY))
            stand.line(to: NSPoint(x: front.midX, y: 2.5))
            stand.move(to: NSPoint(x: front.midX - 2.5, y: 2.5))
            stand.line(to: NSPoint(x: front.midX + 2.5, y: 2.5))
            stand.stroke()
            return true
        }
        image.isTemplate = true
        image.accessibilityDescription = "KVM"
        return image
    }

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let s = controller.state
        let peer = s.peerName ?? "peer"

        let status: String
        if Settings.peerID == nil {
            status = "Not paired"
        } else if !s.connected {
            status = "Connecting…"
        } else if s.stalled {
            status = "\(peer): connection stalled"
        } else {
            var parts = ["Connected to \(peer)"]
            if let direct = s.direct { parts.append(direct ? "direct" : "relayed") }
            if let ms = s.latencyMs { parts.append(String(format: "%.0f ms", ms)) }
            status = parts.joined(separator: " · ")
        }
        menu.addItem(withTitle: status, action: nil, keyEquivalent: "").isEnabled = false
        if awdl.requested, Awdl.isUp == false {
            menu.addItem(withTitle: "AirDrop paused to reduce lag", action: nil, keyEquivalent: "").isEnabled = false
        }
        if !Permissions.accessibility || !Permissions.inputMonitoring || !s.tapRunning {
            let missing = [
                Permissions.accessibility ? nil : "Accessibility",
                Permissions.inputMonitoring ? nil : "Input Monitoring",
            ].compactMap { $0 }
            let title = missing.isEmpty
                ? "⚠︎ Input capture not running: reopen KVM"
                : "⚠︎ Missing permission: \(missing.joined(separator: ", "))"
            let item = NSMenuItem(title: title, action: #selector(showSettings), keyEquivalent: "")
            item.target = self
            menu.addItem(item)
        }
        menu.addItem(.separator())

        let hotkey = Settings.hotkey.display
        let toggle: NSMenuItem
        switch s.mode {
        case .local:
            toggle = NSMenuItem(title: "Control \(peer)   \(hotkey)", action: #selector(toggleControl), keyEquivalent: "")
            toggle.isEnabled = s.connected && !s.stalled
        case .controllingPeer:
            toggle = NSMenuItem(title: "Return to this Mac   \(hotkey)", action: #selector(toggleControl), keyEquivalent: "")
        case .controlledByPeer:
            toggle = NSMenuItem(title: "Controlled by \(peer)", action: nil, keyEquivalent: "")
            toggle.isEnabled = false
        }
        toggle.target = self
        menu.addItem(toggle)

        let view = s.viewingPeer
            ? NSMenuItem(title: "Close \(peer)'s Screen", action: #selector(toggleViewing), keyEquivalent: "")
            : NSMenuItem(title: "View \(peer)'s Screen", action: #selector(toggleViewing), keyEquivalent: "")
        view.target = self
        view.isEnabled = s.connected || s.viewingPeer
        menu.addItem(view)
        let sendFiles = NSMenuItem(title: "Send Files to \(peer)…", action: #selector(chooseFiles), keyEquivalent: "")
        sendFiles.target = self
        sendFiles.isEnabled = s.connected
        menu.addItem(sendFiles)
        let files = controller.files
        for p in files.sending.values {
            menu.addItem(withTitle: "Sending \(p.name)… \(p.percent)%", action: nil, keyEquivalent: "").isEnabled = false
        }
        for p in files.receiving.values {
            menu.addItem(withTitle: "Receiving \(p.name)… \(p.percent)%", action: nil, keyEquivalent: "").isEnabled = false
        }
        if let problem = files.problem {
            let item = NSMenuItem(title: "⚠︎ \(problem)", action: #selector(clearFileProblem), keyEquivalent: "")
            item.target = self
            menu.addItem(item)
        }
        if !files.recent.isEmpty {
            let reveal = NSMenuItem(title: "Show Received Files", action: #selector(revealReceived), keyEquivalent: "")
            reveal.target = self
            menu.addItem(reveal)
        }
        if s.sharingScreen {
            let sharing = NSMenuItem(title: "Stop Sharing This Screen with \(peer)", action: #selector(stopSharing), keyEquivalent: "")
            sharing.target = self
            menu.addItem(sharing)
        }
        menu.addItem(.separator())

        let settings = NSMenuItem(title: "Settings…", action: #selector(showSettings), keyEquivalent: ",")
        settings.target = self
        menu.addItem(settings)
        menu.addItem(withTitle: "Quit KVM", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
    }

    @objc private func chooseFiles() {
        controller.files.chooseAndSend(to: controller.state.peerName ?? "the other computer")
    }

    @objc private func revealReceived() {
        controller.files.revealRecent()
    }

    @objc private func clearFileProblem() {
        controller.files.clearProblem()
    }

    func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation {
        controller.state.connected ? .copy : []
    }

    func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard let urls = sender.draggingPasteboard.readObjects(
            forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty else { return false }
        controller.files.send(urls)
        return true
    }

    @objc private func toggleViewing() {
        if controller.state.viewingPeer { controller.stopViewing() } else { controller.viewPeerScreen() }
    }

    @objc private func stopSharing() {
        controller.screenHost.stop()
    }

    @objc private func toggleControl() {
        controller.toggle()
    }

    @objc private func showSettings() {
        openSettings()
    }
}
