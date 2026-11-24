import AppKit
import SwiftUI

final class AppDelegate: NSObject, NSApplicationDelegate, NSWindowDelegate {
    private var controller: Controller!
    private var remotes: RemoteHost!
    private var menu: StatusMenu!
    private let awdl = Awdl()
    private var pairing: Pairing!
    private var settingsWindow: NSWindow?
    private var permissionTimer: Timer?
    private var activity: NSObjectProtocol?

    func applicationDidFinishLaunching(_ notification: Notification) {
        NSApp.setActivationPolicy(.accessory)
        NSApp.mainMenu = Self.makeMainMenu()
        followWindowsForDockPresence()
        // Keep App Nap and timer coalescing away from the input path.
        activity = ProcessInfo.processInfo.beginActivity(
            options: [.userInitiatedAllowingIdleSystemSleep, .latencyCritical],
            reason: "Forwarding mouse and keyboard input"
        )

        let core: Core
        do {
            core = try Core(stateDir: Settings.appSupportDir, logFile: Settings.logFile,
                            name: Host.current().localizedName ?? "Mac")
        } catch {
            let alert = NSAlert()
            alert.messageText = "KVM couldn't start"
            alert.informativeText = "\(error)"
            alert.runModal()
            NSApp.terminate(nil)
            return
        }
        let version = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?"
        Log.info("KVM \(version) started: accessibility=\(Permissions.accessibility) inputMonitoring=\(Permissions.inputMonitoring)")
        Permissions.resetStaleGrants()
        Updates.shared.start()
        controller = Controller(core: core)
        remotes = RemoteHost(core: Core(remotesOf: core))
        menu = StatusMenu(controller: controller, awdl: awdl, openSettings: { [unowned self] in showSettings() })
        awdl.follow(controller.state)
        pairing = Pairing(controller: controller, remotes: remotes)

        if let peer = Settings.peerID, !controller.setPeer(peer) {
            Settings.peerID = nil
        }
        startCaptureWhenPermitted()
        if Settings.peerID == nil || !controller.state.tapRunning {
            showSettings()
        }
    }

    /// A menu-bar app (.accessory) is left out of ⌘-Tab and the Dock. While one of our
    /// windows (the screen viewer, Settings) is open, act as a regular app so it can be
    /// switched to like any other; go back to the menu bar once they are all closed.
    private func followWindowsForDockPresence() {
        let center = NotificationCenter.default
        center.addObserver(forName: NSWindow.didBecomeKeyNotification, object: nil, queue: .main) { note in
            guard let window = note.object as? NSWindow, window.styleMask.contains(.titled),
                  NSApp.activationPolicy() != .regular else { return }
            NSApp.setActivationPolicy(.regular)
        }
        center.addObserver(forName: NSWindow.willCloseNotification, object: nil, queue: .main) { note in
            let closing = note.object as? NSWindow
            // The closing window is still listed as visible; check once it is gone.
            DispatchQueue.main.async {
                let open = NSApp.windows.contains {
                    $0 !== closing && $0.styleMask.contains(.titled) && ($0.isVisible || $0.isMiniaturized)
                }
                if !open, NSApp.activationPolicy() != .accessory {
                    NSApp.setActivationPolicy(.accessory)
                }
            }
        }
    }

    /// Shown while the app is regular (see `followWindowsForDockPresence`).
    private static func makeMainMenu() -> NSMenu {
        let main = NSMenu()
        let appItem = NSMenuItem()
        let appMenu = NSMenu()
        appMenu.addItem(withTitle: "Hide KVM", action: #selector(NSApplication.hide(_:)), keyEquivalent: "h")
        appMenu.addItem(.separator())
        appMenu.addItem(withTitle: "Quit KVM", action: #selector(NSApplication.terminate(_:)), keyEquivalent: "q")
        appItem.submenu = appMenu
        main.addItem(appItem)

        let windowItem = NSMenuItem()
        let windowMenu = NSMenu(title: "Window")
        windowMenu.addItem(withTitle: "Minimize", action: #selector(NSWindow.performMiniaturize(_:)), keyEquivalent: "m")
        windowMenu.addItem(withTitle: "Close", action: #selector(NSWindow.performClose(_:)), keyEquivalent: "w")
        let fullScreen = windowMenu.addItem(withTitle: "Enter Full Screen",
                                            action: #selector(NSWindow.toggleFullScreen(_:)), keyEquivalent: "f")
        fullScreen.keyEquivalentModifierMask = [.control, .command]
        windowItem.submenu = windowMenu
        main.addItem(windowItem)
        NSApp.windowsMenu = windowMenu
        return main
    }

    func applicationWillTerminate(_ notification: Notification) {
        awdl.stop()
        remotes?.stop()
        controller?.returnToLocal(notifyPeer: true)
        controller?.core.clearPeer()
    }

    /// The event tap can only be created once permissions are granted; retry until then.
    private func startCaptureWhenPermitted() {
        if controller.capture.start() {
            controller.state.tapRunning = true
            Log.info("event tap started")
            return
        }
        Log.warn("event tap unavailable until permissions are granted")
        permissionTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] timer in
            guard let self, Permissions.accessibility, controller.capture.start() else { return }
            controller.state.tapRunning = true
            Log.info("event tap started")
            timer.invalidate()
        }
    }

    private func showSettings() {
        if settingsWindow == nil {
            let view = SettingsView(
                myID: controller.core.id,
                state: controller.state,
                pairing: pairing,
                remotes: remotes.state,
                saveHotkey: { [unowned self] hotkey in
                    Settings.hotkey = hotkey
                    controller.capture.setHotkey(hotkey)
                },
                setPauseAwdl: { [unowned self] on in
                    awdl.setEnabled(on, connected: controller.state.connected)
                },
                uninstallAwdlHelper: { [unowned self] in
                    do {
                        try awdl.uninstall()
                        return nil
                    } catch {
                        return "\(error)"
                    }
                },
                edgeChanged: { [unowned self] in controller.updateEdge() },
                setSendMicrophone: { [unowned self] on in
                    Settings.sendMicrophone = on
                    controller.updateMicrophone()
                }
            )
            let hosting = NSHostingController(rootView: view)
            hosting.sizingOptions = [.minSize] // the user sizes the window; the form scrolls
            let window = NSWindow(contentViewController: hosting)
            window.title = "KVM Settings"
            window.styleMask = [.titled, .closable, .resizable, .miniaturizable]
            window.setContentSize(NSSize(width: 580, height: 720))
            window.contentMinSize = NSSize(width: 460, height: 360)
            window.setFrameAutosaveName("KVMSettings")
            window.isReleasedWhenClosed = false
            window.delegate = self
            window.center()
            settingsWindow = window
        }
        NSApp.activate(ignoringOtherApps: true)
        settingsWindow?.makeKeyAndOrderFront(nil)
        setPairingMode(true)
    }

    // Pairing mode lasts while Settings is in use, like Bluetooth's: on while it is the
    // active window, and for a few minutes after, but not for days in the background.
    private static let pairingModeLinger: TimeInterval = 5 * 60
    private var pairingModeEnd: DispatchWorkItem?

    private func setPairingMode(_ on: Bool) {
        pairingModeEnd?.cancel()
        pairingModeEnd = nil
        controller.core.setDiscoverable(on)
    }

    func windowDidBecomeKey(_ notification: Notification) {
        if notification.object as? NSWindow === settingsWindow { setPairingMode(true) }
    }

    func windowDidResignKey(_ notification: Notification) {
        guard notification.object as? NSWindow === settingsWindow else { return }
        pairingModeEnd?.cancel()
        let end = DispatchWorkItem { [weak self] in self?.setPairingMode(false) }
        pairingModeEnd = end
        DispatchQueue.main.asyncAfter(deadline: .now() + Self.pairingModeLinger, execute: end)
    }

    func windowWillClose(_ notification: Notification) {
        if notification.object as? NSWindow === settingsWindow {
            setPairingMode(false)
        }
    }
}

let app = NSApplication.shared
let delegate = AppDelegate()
app.delegate = delegate
app.run()
