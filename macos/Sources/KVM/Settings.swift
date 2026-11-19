import AppKit
import ApplicationServices
import CryptoKit
import ServiceManagement

enum Settings {
    private static let defaults = UserDefaults.standard

    static var peerID: String? {
        get { defaults.string(forKey: "peerID") }
        set { defaults.set(newValue, forKey: "peerID") }
    }

    /// Paired remote controls (iPhone, iPad): id -> name.
    static var remotes: [String: String] {
        get { defaults.dictionary(forKey: "remotes") as? [String: String] ?? [:] }
        set { defaults.set(newValue, forKey: "remotes") }
    }

    static var hotkey: Hotkey {
        get {
            guard defaults.object(forKey: "hotkeyKeyCode") != nil else { return .default }
            return Hotkey(
                keyCode: UInt16(defaults.integer(forKey: "hotkeyKeyCode")),
                modifiers: CGEventFlags(rawValue: UInt64(defaults.integer(forKey: "hotkeyModifiers"))),
                label: defaults.string(forKey: "hotkeyLabel") ?? "?"
            )
        }
        set {
            defaults.set(Int(newValue.keyCode), forKey: "hotkeyKeyCode")
            defaults.set(Int(newValue.modifiers.rawValue), forKey: "hotkeyModifiers")
            defaults.set(newValue.label, forKey: "hotkeyLabel")
        }
    }

    /// Keep AWDL (AirDrop's radio hopping) off while connected.
    static var pauseAwdl: Bool {
        get { defaults.bool(forKey: "pauseAwdl") }
        set { defaults.set(newValue, forKey: "pauseAwdl") }
    }

    /// The first-connection offer to pause AWDL was shown.
    static var awdlPrompted: Bool {
        get { defaults.bool(forKey: "awdlPrompted") }
        set { defaults.set(newValue, forKey: "awdlPrompted") }
    }

    /// Share what's copied with the paired computer. On by default.
    static var shareClipboard: Bool {
        get { defaults.object(forKey: "shareClipboard") as? Bool ?? true }
        set { defaults.set(newValue, forKey: "shareClipboard") }
    }

    /// Files copied on the paired computer, cached until pasted here.
    static var clipboardCacheDirectory: URL {
        FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("com.marcogaletta.kvm/Clipboard", isDirectory: true)
    }

    /// Copied files up to this size are sent to the paired computer straight away, so they
    /// can be pasted there; bigger copies are left to Send Files. 0 turns it off.
    static var clipboardFileLimit: Int64 {
        get { (defaults.object(forKey: "clipboardFileLimit") as? NSNumber)?.int64Value ?? 500 * 1024 * 1024 }
        set { defaults.set(NSNumber(value: newValue), forKey: "clipboardFileLimit") }
    }

    static let clipboardFileLimitChoices: [(label: String, bytes: Int64)] = [
        ("Off", 0), ("100 MB", 100 << 20), ("500 MB", 500 << 20), ("1 GB", 1 << 30), ("5 GB", 5 << 30),
        ("No limit", Int64.max),
    ]

    /// Where files from the paired computer are saved.
    static var downloadDirectory: URL {
        get {
            defaults.string(forKey: "downloadDirectory").map { URL(fileURLWithPath: $0) }
                ?? FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask)[0]
        }
        set {
            defaults.set(newValue.path, forKey: "downloadDirectory")
            Core.setDownloadDirectory(newValue)
        }
    }

    /// Screen viewer quality preset.
    static var viewerQuality: VideoQuality {
        get { defaults.string(forKey: "viewerQuality").flatMap(VideoQuality.init) ?? .auto }
        set { defaults.set(newValue.rawValue, forKey: "viewerQuality") }
    }

    /// Stream the peer's screen at the viewer window's size rather than full size.
    static var viewerMatchesWindow: Bool {
        get { defaults.object(forKey: "viewerMatchesWindow") as? Bool ?? true }
        set { defaults.set(newValue, forKey: "viewerMatchesWindow") }
    }

    /// Re-lay out the peer's main display at the viewer window's size and shape, instead of
    /// only scaling its picture.
    static var viewerResizesHost: Bool {
        get { defaults.bool(forKey: "viewerResizesHost") }
        set { defaults.set(newValue, forKey: "viewerResizesHost") }
    }

    /// The screen viewer's how-to-control tip was shown.
    static var viewerTipShown: Bool {
        get { defaults.bool(forKey: "viewerTipShown") }
        set { defaults.set(newValue, forKey: "viewerTipShown") }
    }

    /// Take control of the peer when the pointer enters its picture in the screen viewer.
    static var viewerHoverControl: Bool {
        get { defaults.object(forKey: "viewerHoverControl") as? Bool ?? true }
        set { defaults.set(newValue, forKey: "viewerHoverControl") }
    }

    static var launchAtLogin: Bool {
        get { SMAppService.mainApp.status == .enabled }
        set {
            do {
                if newValue { try SMAppService.mainApp.register() } else { try SMAppService.mainApp.unregister() }
            } catch {
                NSLog("KVM: launch at login: \(error)")
            }
        }
    }

    static var appSupportDir: URL {
        FileManager.default.urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("KVM", isDirectory: true)
    }

    static var logFile: URL {
        let dir = FileManager.default.urls(for: .libraryDirectory, in: .userDomainMask)[0]
            .appendingPathComponent("Logs/KVM", isDirectory: true)
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir.appendingPathComponent("kvm.log")
    }
}

enum Permissions {
    /// Needed to post (inject) events and for the event tap to modify input.
    static var accessibility: Bool { AXIsProcessTrusted() }
    /// Needed for the event tap to see keyboard input.
    static var inputMonitoring: Bool { CGPreflightListenEventAccess() }

    /// macOS ties permission grants to the code signature. A build signed differently
    /// (another certificate, or ad-hoc) keeps its old entries in System Settings, switched
    /// on but not applying, and turning them on again does nothing. So when a permission is
    /// missing and the signature changed since we last looked, remove our entries for the
    /// missing permissions, so the next request adds a fresh one. Granted ones stay as
    /// they are; at most once per signature.
    static func resetStaleGrants() {
        guard let bundleID = Bundle.main.bundleIdentifier, let signature = signingIdentity else { return }
        let defaults = UserDefaults.standard
        let key = "permissionsCheckedFor"
        guard defaults.string(forKey: key) != signature else { return }
        defaults.set(signature, forKey: key)
        let missing = [("Accessibility", accessibility), ("ListenEvent", inputMonitoring), ("ScreenCapture", screenRecording)]
            .filter { !$0.1 }.map(\.0)
        for service in missing {
            let tccutil = Process()
            tccutil.executableURL = URL(fileURLWithPath: "/usr/bin/tccutil")
            tccutil.arguments = ["reset", service, bundleID]
            do {
                try tccutil.run()
                tccutil.waitUntilExit()
                Log.info("signature changed: reset \(service) entry (status \(tccutil.terminationStatus))")
            } catch {
                Log.warn("tccutil reset \(service): \(error)")
            }
        }
    }

    /// What macOS matches grants against: the signing certificate, or for an ad-hoc
    /// signature (no certificate) the code itself.
    private static var signingIdentity: String? {
        var code: SecStaticCode?
        var info: CFDictionary?
        guard SecStaticCodeCreateWithPath(Bundle.main.bundleURL as CFURL, [], &code) == errSecSuccess, let code,
              SecCodeCopySigningInformation(code, SecCSFlags(rawValue: kSecCSSigningInformation), &info) == errSecSuccess,
              let info = info as? [String: Any] else { return nil }
        if let leaf = (info[kSecCodeInfoCertificates as String] as? [SecCertificate])?.first {
            let der = SecCertificateCopyData(leaf) as Data
            return SHA256.hash(data: der).map { String(format: "%02x", $0) }.joined()
        }
        return (info[kSecCodeInfoUnique as String] as? Data)?.map { String(format: "%02x", $0) }.joined()
    }

    static func requestAccessibility() {
        let key = kAXTrustedCheckOptionPrompt.takeUnretainedValue() as String
        _ = AXIsProcessTrustedWithOptions([key: true] as CFDictionary)
        open("Privacy_Accessibility")
    }

    static var screenRecording: Bool { CGPreflightScreenCaptureAccess() }

    static func requestScreenRecording() {
        _ = CGRequestScreenCaptureAccess()
        open("Privacy_ScreenCapture")
    }

    static func requestInputMonitoring() {
        _ = CGRequestListenEventAccess()
        open("Privacy_ListenEvent")
    }

    private static func open(_ pane: String) {
        if let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?\(pane)") {
            NSWorkspace.shared.open(url)
        }
    }
}
