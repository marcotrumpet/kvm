import AppKit
import IOKit.pwr_mgt

/// Locking this Mac's screen, and noticing when it locks.
enum ScreenLock {
    /// When we locked because the peer asked: our own lock notice then isn't sent back.
    private static var requestedAt = Date.distantPast

    /// Calls `onLocked` (main thread) when the user locks this screen, not when the peer did.
    static func observe(_ onLocked: @escaping () -> Void) {
        DistributedNotificationCenter.default().addObserver(
            forName: NSNotification.Name("com.apple.screenIsLocked"), object: nil, queue: .main
        ) { _ in
            guard Date().timeIntervalSince(requestedAt) > 5 else { return }
            onLocked()
        }
    }

    /// Locks the screen now. Main thread.
    static func lock() {
        requestedAt = Date()
        // Private login.framework call behind the Lock Screen menu item; ⌃⌘Q otherwise.
        if let handle = dlopen("/System/Library/PrivateFrameworks/login.framework/Versions/Current/login", RTLD_LAZY),
           let symbol = dlsym(handle, "SACLockScreenImmediate") {
            typealias LockFunction = @convention(c) () -> Int32
            _ = unsafeBitCast(symbol, to: LockFunction.self)()
            return
        }
        let source = CGEventSource(stateID: .hidSystemState)
        for down in [true, false] {
            let event = CGEvent(keyboardEventSource: source, virtualKey: 0x0C, keyDown: down) // Q
            event?.flags = [.maskCommand, .maskControl]
            event?.post(tap: .cghidEventTap)
        }
        Log.info("locked the screen (peer locked theirs)")
    }
}

/// Waking: this Mac's address for Wake-on-LAN, and waking its display for the peer.
enum Wake {
    /// The MAC address of the primary network interface (en0), 6 bytes.
    static func macAddress() -> Data? {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0, let first = list else { return nil }
        defer { freeifaddrs(list) }
        for entry in sequence(first: first, next: { $0.pointee.ifa_next }) {
            let ifa = entry.pointee
            guard String(cString: ifa.ifa_name) == "en0", let addr = ifa.ifa_addr,
                  addr.pointee.sa_family == UInt8(AF_LINK) else { continue }
            return addr.withMemoryRebound(to: sockaddr_dl.self, capacity: 1) { dl -> Data? in
                let d = dl.pointee
                guard d.sdl_alen == 6 else { return nil }
                return withUnsafeBytes(of: dl.pointee.sdl_data) { raw in
                    Data(raw[Int(d.sdl_nlen)..<Int(d.sdl_nlen) + 6])
                }
            }
        }
        return nil
    }

    /// Wakes the display (and keeps it awake a while), as if the user touched this Mac.
    static func display() {
        var id: IOPMAssertionID = 0
        IOPMAssertionDeclareUserActivity("KVM peer input" as CFString, kIOPMUserActiveLocal, &id)
    }
}

/// While someone else uses this Mac (controls it or views its screen): a small panel at
/// the top right saying who, with a button to take the Mac back. Main thread only.
final class ControlIndicator {
    /// Main thread: the user clicked Take Back.
    var onTakeBack: (() -> Void)?
    private var panel: NSPanel?
    private let label = NSTextField(labelWithString: "")
    /// Stacking position, so two indicators (computer and phone) don't overlap.
    private let slot: Int

    init(slot: Int = 0) {
        self.slot = slot
    }

    /// `controlling`/`viewing`: what `name` is doing; neither hides the panel.
    func update(name: String, controlling: Bool, viewing: Bool) {
        guard Settings.showControlIndicator, controlling || viewing else {
            panel?.orderOut(nil)
            return
        }
        let what = controlling && viewing ? "is controlling and viewing this Mac"
            : controlling ? "is controlling this Mac" : "is viewing this screen"
        label.stringValue = "\(name) \(what)"
        let panel = self.panel ?? makePanel()
        self.panel = panel
        panel.layoutIfNeeded()
        panel.setContentSize(panel.contentView!.fittingSize)
        if let screen = NSScreen.main {
            let f = screen.visibleFrame
            panel.setFrameTopLeftPoint(NSPoint(x: f.maxX - panel.frame.width - 12, y: f.maxY - 12 - CGFloat(slot) * 48))
        }
        panel.orderFrontRegardless()
    }

    private func makePanel() -> NSPanel {
        let panel = NSPanel(contentRect: .zero, styleMask: [.nonactivatingPanel, .borderless, .hudWindow],
                            backing: .buffered, defer: false)
        panel.level = .statusBar
        panel.isFloatingPanel = true
        panel.hidesOnDeactivate = false
        panel.collectionBehavior = [.canJoinAllSpaces, .stationary, .ignoresCycle, .fullScreenAuxiliary]
        panel.backgroundColor = .clear
        panel.isOpaque = false
        panel.hasShadow = true

        let background = NSVisualEffectView()
        background.material = .hudWindow
        background.state = .active
        background.wantsLayer = true
        background.layer?.cornerRadius = 10
        let icon = NSImageView(image: NSImage(systemSymbolName: "dot.radiowaves.left.and.right",
                                              accessibilityDescription: nil)!)
        icon.contentTintColor = .systemOrange
        label.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .medium)
        let button = NSButton(title: "Take Back", target: self, action: #selector(takeBack))
        button.controlSize = .small
        button.bezelStyle = .rounded
        button.toolTip = "Stop the other device controlling and viewing this Mac"
        let stack = NSStackView(views: [icon, label, button])
        stack.spacing = 8
        stack.edgeInsets = NSEdgeInsets(top: 8, left: 12, bottom: 8, right: 10)
        stack.translatesAutoresizingMaskIntoConstraints = false
        background.addSubview(stack)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: background.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: background.trailingAnchor),
            stack.topAnchor.constraint(equalTo: background.topAnchor),
            stack.bottomAnchor.constraint(equalTo: background.bottomAnchor),
        ])
        panel.contentView = background
        return panel
    }

    @objc private func takeBack() {
        panel?.orderOut(nil)
        onTakeBack?()
    }
}
