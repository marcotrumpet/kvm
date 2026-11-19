import AppKit
import Sparkle

/// Updates from the GitHub releases, with Sparkle (feed and public key in Info.plist; see
/// release.sh). Checks daily and asks before installing, with the release notes. A found
/// update waits in the menu rather than taking focus, since this is a menu bar app.
/// Main thread only.
final class Updates: NSObject, SPUStandardUserDriverDelegate {
    static let shared = Updates()

    private var controller: SPUStandardUpdaterController!
    private var started = false
    /// The version found by a scheduled check, until its update session ends.
    private(set) var available: String?

    private override init() {
        super.init()
        controller = SPUStandardUpdaterController(startingUpdater: false, updaterDelegate: nil, userDriverDelegate: self)
    }

    /// A development build (no feed in its Info.plist) has nothing to check.
    func start() {
        guard Bundle.main.object(forInfoDictionaryKey: "SUFeedURL") != nil else { return }
        controller.startUpdater()
        started = true
    }

    var canCheck: Bool { started && controller.updater.canCheckForUpdates }

    /// Checks now, or brings up the update already found.
    func checkNow() {
        NSApp.activate(ignoringOtherApps: true)
        controller.checkForUpdates(nil)
    }

    var automaticallyChecks: Bool {
        get { controller.updater.automaticallyChecksForUpdates }
        set { controller.updater.automaticallyChecksForUpdates = newValue }
    }

    var version: String { Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "?" }

    // MARK: SPUStandardUserDriverDelegate

    var supportsGentleScheduledUpdateReminders: Bool { true }

    func standardUserDriverShouldHandleShowingScheduledUpdate(_ update: SUAppcastItem, andInImmediateFocus immediateFocus: Bool) -> Bool {
        immediateFocus // otherwise it waits in the menu
    }

    func standardUserDriverWillHandleShowingUpdate(_ handleShowingUpdate: Bool, forUpdate update: SUAppcastItem, state: SPUUserUpdateState) {
        if !state.userInitiated { available = update.displayVersionString }
    }

    func standardUserDriverWillFinishUpdateSession() {
        available = nil
    }
}
