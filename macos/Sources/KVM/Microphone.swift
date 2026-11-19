import AppKit
import CoreAudio
import os

/// The controlling device's microphone, offered to this Mac's apps (calls) as the input
/// device "KVM Microphone": a Core Audio driver (macos/Microphone) whose hidden "KVM
/// Microphone Feed" output device loops into it. The received sound is played into the
/// feed. The driver is installed once with an admin password, like the AWDL helper.
enum Microphone {
    private static let installedPath = "/Library/Audio/Plug-Ins/HAL/KVMMicrophone.driver"
    private static let feedUID = "KVMMicrophoneFeed_UID"

    private static var bundledDriver: String? {
        Bundle.main.url(forResource: "KVMMicrophone", withExtension: "driver")?.path
    }

    static var isInstalled: Bool { FileManager.default.fileExists(atPath: installedPath) }

    /// Installed and the same code as the driver in this build.
    static var isCurrent: Bool {
        guard isInstalled, let bundled = bundledDriver, let installed = Admin.codeHash(installedPath) else { return false }
        return Admin.codeHash(bundled) == installed
    }

    /// Copies the driver into place and restarts Core Audio to load it (sound stops for a
    /// moment). Main thread: shows the admin password prompt.
    static func install() throws {
        guard let bundled = bundledDriver else { throw CoreError(description: "driver missing from the app bundle") }
        try Admin.run([
            "/bin/mkdir -p /Library/Audio/Plug-Ins/HAL",
            "/bin/rm -rf \(Admin.quote(installedPath))",
            "/bin/cp -R \(Admin.quote(bundled)) \(Admin.quote(installedPath))",
            "/usr/sbin/chown -R root:wheel \(Admin.quote(installedPath))",
            "/usr/bin/killall coreaudiod", // launchd starts it again (kickstart is refused under SIP)
        ].joined(separator: " && "))
        Log.info("microphone driver installed")
    }

    static func uninstall() throws {
        try Admin.run([
            "/bin/rm -rf \(Admin.quote(installedPath))",
            "/usr/bin/killall coreaudiod", // launchd starts it again (kickstart is refused under SIP)
        ].joined(separator: " && "))
        Log.info("microphone driver removed")
    }

    /// The feed device, while the driver is loaded.
    static var feedDevice: AudioDeviceID? {
        var uid = feedUID as CFString
        var device = AudioDeviceID(kAudioObjectUnknown)
        var size = UInt32(MemoryLayout<AudioDeviceID>.size)
        var address = AudioObjectPropertyAddress(
            mSelector: kAudioHardwarePropertyTranslateUIDToDevice,
            mScope: kAudioObjectPropertyScopeGlobal, mElement: kAudioObjectPropertyElementMain)
        let status = withUnsafeMutablePointer(to: &uid) { qualifier in
            AudioObjectGetPropertyData(AudioObjectID(kAudioObjectSystemObject), &address,
                                       UInt32(MemoryLayout<CFString>.size), qualifier, &size, &device)
        }
        return status == noErr && device != kAudioObjectUnknown ? device : nil
    }

    /// Plays received microphone packets into the feed; nothing without the driver.
    static let sink = AudioPlayer(outputDevice: {
        guard let device = feedDevice else {
            warnMissingOnce()
            return nil
        }
        return device
    })

    private static let warned = OSAllocatedUnfairLock(initialState: false)

    private static func warnMissingOnce() {
        guard !warned.withLock({ w in defer { w = true }; return w }) else { return }
        Log.warn("microphone received, but the KVM Microphone driver isn't installed (Settings > Microphone)")
    }
}
