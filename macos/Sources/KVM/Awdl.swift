import AppKit
import Combine

/// Pauses AWDL (AirDrop's Wi-Fi channel hopping, the source of 80-100 ms lag spikes) while
/// the computers are connected, through a root helper daemon installed once with an admin
/// password. The app only asks by writing its PID to a request file; the helper restores
/// AWDL as soon as the file is gone or the app is no longer running.
final class Awdl {
    static let label = "com.marcogaletta.kvm.awdl-helper"
    private static let helperPath = "/Library/PrivilegedHelperTools/\(label)"
    private static let plistPath = "/Library/LaunchDaemons/\(label).plist"
    private static var requestFile: URL { Settings.appSupportDir.appendingPathComponent("awdl-request") }
    /// Brief disconnects shouldn't flap AWDL on and off.
    private static let restoreDelay: TimeInterval = 5

    private var subscription: AnyCancellable?
    private var pendingRestore: DispatchWorkItem?
    private(set) var requested = false

    /// Follows the connection state. Main thread only.
    func follow(_ state: AppState) {
        subscription = state.$connected.removeDuplicates().sink { [weak self] connected in
            self?.connectionChanged(connected)
        }
    }

    static var isInstalled: Bool {
        FileManager.default.fileExists(atPath: plistPath) && FileManager.default.fileExists(atPath: helperPath)
    }

    /// Installed and running the same code as the helper bundled with this build. Compares
    /// code hashes rather than bytes: re-signing changes the bytes but not the code.
    static var isCurrent: Bool {
        guard isInstalled, let bundled = bundledHelper,
              let installedHash = codeHash(helperPath) else { return false }
        return codeHash(bundled) == installedHash
    }

    private static func codeHash(_ path: String) -> Data? {
        var code: SecStaticCode?
        guard SecStaticCodeCreateWithPath(URL(fileURLWithPath: path) as CFURL, [], &code) == errSecSuccess,
              let code else { return nil }
        var info: CFDictionary?
        guard SecCodeCopySigningInformation(code, [], &info) == errSecSuccess,
              let dict = info as? [String: Any] else { return nil }
        return dict[kSecCodeInfoUnique as String] as? Data
    }

    /// nil when this Mac has no awdl0 interface.
    static var isUp: Bool? {
        var list: UnsafeMutablePointer<ifaddrs>?
        guard getifaddrs(&list) == 0 else { return nil }
        defer { freeifaddrs(list) }
        var found = false
        var entry = list
        while let e = entry {
            if String(cString: e.pointee.ifa_name) == "awdl0" {
                found = true
                if Int32(e.pointee.ifa_flags) & IFF_UP != 0 { return true }
            }
            entry = e.pointee.ifa_next
        }
        return found ? false : nil
    }

    /// Turns the setting on (installing or updating the helper) or off. Returns false if
    /// the install was cancelled or failed.
    func setEnabled(_ enabled: Bool, connected: Bool) -> Bool {
        if enabled, !Awdl.isCurrent {
            do {
                try Awdl.install()
            } catch {
                Log.warn("installing AWDL helper: \(error)")
                return false
            }
        }
        Settings.pauseAwdl = enabled
        setRequested(enabled && connected)
        return true
    }

    func uninstall() throws {
        setRequested(false)
        Settings.pauseAwdl = false
        try Awdl.runAsAdmin("""
            (/bin/launchctl bootout system/\(Awdl.label) 2>/dev/null || true) && \
            /bin/rm -f \(Awdl.quote(Awdl.helperPath)) \(Awdl.quote(Awdl.plistPath))
            """)
    }

    /// Withdraws the request immediately, e.g. when quitting.
    func stop() {
        setRequested(false)
    }

    private func connectionChanged(_ connected: Bool) {
        pendingRestore?.cancel()
        pendingRestore = nil
        if connected {
            if Settings.pauseAwdl {
                // A newer build may bundle a newer helper; reinstalling asks for the password.
                if !Awdl.isCurrent, !setEnabled(true, connected: true) {
                    Settings.pauseAwdl = false
                    return
                }
                setRequested(true)
            } else if !Settings.awdlPrompted, Awdl.isUp == true {
                Settings.awdlPrompted = true
                offerToPause()
            }
        } else if requested {
            let restore = DispatchWorkItem { [weak self] in self?.setRequested(false) }
            pendingRestore = restore
            DispatchQueue.main.asyncAfter(deadline: .now() + Awdl.restoreDelay, execute: restore)
        }
    }

    private func offerToPause() {
        let alert = NSAlert()
        alert.messageText = "Reduce Wi-Fi lag?"
        alert.informativeText = """
            AirDrop's Wi-Fi radio hopping causes noticeable mouse lag. KVM can pause it while \
            the computers are connected and turn it back on when they disconnect.

            While paused, AirDrop, Sidecar and Universal Control don't work. You can change \
            this in Settings. KVM needs your admin password once to install a small helper.
            """
        alert.addButton(withTitle: "Pause While Connected")
        alert.addButton(withTitle: "Not Now")
        NSApp.activate(ignoringOtherApps: true)
        if alert.runModal() == .alertFirstButtonReturn {
            _ = setEnabled(true, connected: true)
        }
    }

    private func setRequested(_ on: Bool) {
        requested = on
        if on {
            try? FileManager.default.createDirectory(at: Settings.appSupportDir, withIntermediateDirectories: true)
            try? "\(ProcessInfo.processInfo.processIdentifier)\n".write(to: Awdl.requestFile, atomically: true, encoding: .utf8)
        } else {
            try? FileManager.default.removeItem(at: Awdl.requestFile)
        }
    }

    private static var bundledHelper: String? {
        Bundle.main.url(forResource: "kvm-awdl-helper", withExtension: nil)?.path
    }

    private static func install() throws {
        guard let bundled = bundledHelper else {
            throw CoreError(description: "helper missing from the app bundle")
        }
        let plist: [String: Any] = [
            "Label": label,
            "ProgramArguments": [helperPath, requestFile.path],
            "RunAtLoad": true,
            "KeepAlive": true,
            "StandardErrorPath": "/Library/Logs/\(label).log",
        ]
        let data = try PropertyListSerialization.data(fromPropertyList: plist, format: .xml, options: 0)
        let staged = FileManager.default.temporaryDirectory.appendingPathComponent("\(label).plist")
        try data.write(to: staged)
        defer { try? FileManager.default.removeItem(at: staged) }

        try runAsAdmin([
            "/bin/mkdir -p /Library/PrivilegedHelperTools",
            "(/bin/launchctl bootout system/\(label) 2>/dev/null || true)",
            "/bin/cp \(quote(bundled)) \(quote(helperPath))",
            "/usr/sbin/chown root:wheel \(quote(helperPath))",
            "/bin/chmod 755 \(quote(helperPath))",
            "/bin/cp \(quote(staged.path)) \(quote(plistPath))",
            "/usr/sbin/chown root:wheel \(quote(plistPath))",
            "/bin/chmod 644 \(quote(plistPath))",
            "/bin/launchctl bootstrap system \(quote(plistPath))",
        ].joined(separator: " && "))
        Log.info("AWDL helper installed")
    }

    /// Runs a shell command as root after the standard macOS admin password prompt.
    private static func runAsAdmin(_ command: String) throws {
        let escaped = command
            .replacingOccurrences(of: "\\", with: "\\\\")
            .replacingOccurrences(of: "\"", with: "\\\"")
        var error: NSDictionary?
        NSAppleScript(source: "do shell script \"\(escaped)\" with administrator privileges")?
            .executeAndReturnError(&error)
        if let error {
            throw CoreError(description: error[NSAppleScript.errorMessage] as? String ?? "\(error)")
        }
    }

    private static func quote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }
}
