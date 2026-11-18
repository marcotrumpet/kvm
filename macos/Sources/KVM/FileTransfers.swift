import AppKit
import UserNotifications

/// Sending files to the peer and receiving its files. Main thread only.
final class FileTransfers: NSObject, ObservableObject, UNUserNotificationCenterDelegate {
    struct Progress {
        var name: String
        var done: Int64 = 0
        var total: Int64 = 0

        var percent: Int { total > 0 ? Int(done * 100 / total) : 0 }
    }

    @Published private(set) var sending: [UInt64: Progress] = [:]
    @Published private(set) var receiving: [UInt64: Progress] = [:]
    /// Top-level items received most recently, to reveal in Finder.
    @Published private(set) var recent: [URL] = []
    @Published private(set) var problem: String?

    private let core: Core
    /// Per incoming transfer: files received so far and the top-level items they belong to.
    private var arrived: [UInt64: (count: Int, items: [URL])] = [:]

    init(core: Core) {
        self.core = core
        super.init()
        Core.setDownloadDirectory(Settings.downloadDirectory)
        UNUserNotificationCenter.current().delegate = self
    }

    func chooseAndSend(to peer: String) {
        let panel = NSOpenPanel()
        panel.title = "Send to \(peer)"
        panel.prompt = "Send"
        panel.canChooseFiles = true
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = true
        NSApp.activate(ignoringOtherApps: true)
        guard panel.runModal() == .OK else { return }
        send(panel.urls)
    }

    func send(_ urls: [URL]) {
        guard !urls.isEmpty else { return }
        guard let transfer = core.sendFiles(urls) else {
            problem = "Not connected: files not sent."
            NSSound.beep()
            return
        }
        problem = nil
        let name = urls.count == 1 ? urls[0].lastPathComponent : "\(urls.count) items"
        sending[transfer] = Progress(name: name)
        Log.info("sending \(urls.count) item(s)")
    }

    func revealRecent() {
        NSWorkspace.shared.activateFileViewerSelecting(recent)
    }

    /// Shows a message in the menu.
    func report(_ message: String) {
        problem = message
    }

    func clearProblem() {
        problem = nil
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .fileSendProgress(let transfer, let sent, let total):
            sending[transfer]?.done = sent
            sending[transfer]?.total = total
        case .fileSendDone(let transfer, let error):
            let name = sending.removeValue(forKey: transfer)?.name ?? "Files"
            if let error { problem = "Couldn't send \(name): \(error)" }
        case .fileReceiveProgress(let transfer, let name, let received, let total, _):
            let top = name.split(separator: "/").first.map(String.init) ?? name
            receiving[transfer, default: Progress(name: top)].done = received
            receiving[transfer]?.total = total
        case .fileReceived(let transfer, let path, _, let count, _):
            var state = arrived[transfer, default: (0, [])]
            state.count += 1
            if let item = topLevelItem(of: URL(fileURLWithPath: path)), !state.items.contains(item) {
                state.items.append(item)
            }
            arrived[transfer] = state
            if state.count == count {
                arrived[transfer] = nil
                receiving[transfer] = nil
                recent = state.items
                notifyReceived(state.items, files: count)
            }
        case .fileReceiveFailed(let transfer, let message, _):
            receiving[transfer] = nil
            arrived[transfer] = nil
            problem = "Receiving failed: \(message)"
        default:
            break
        }
    }

    /// The sent item a received file belongs to: the file itself, or its top folder.
    private func topLevelItem(of file: URL) -> URL? {
        let root = Settings.downloadDirectory.standardizedFileURL.pathComponents
        let parts = file.standardizedFileURL.pathComponents
        guard parts.count > root.count, Array(parts.prefix(root.count)) == root else { return file }
        return URL(fileURLWithPath: NSString.path(withComponents: Array(parts.prefix(root.count + 1))))
    }

    private func notifyReceived(_ items: [URL], files: Int) {
        let center = UNUserNotificationCenter.current()
        center.requestAuthorization(options: [.alert, .sound]) { granted, _ in
            guard granted else { return }
            let content = UNMutableNotificationContent()
            content.title = items.count == 1 ? "Received \(items[0].lastPathComponent)" : "Received \(items.count) items"
            content.body = files == 1 ? "Saved to \(Settings.downloadDirectory.lastPathComponent). Click to show it."
                : "\(files) files saved to \(Settings.downloadDirectory.lastPathComponent). Click to show them."
            center.add(UNNotificationRequest(identifier: UUID().uuidString, content: content, trigger: nil))
        }
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse,
                                withCompletionHandler completionHandler: @escaping () -> Void) {
        DispatchQueue.main.async { self.revealRecent() }
        completionHandler()
    }

    func userNotificationCenter(_ center: UNUserNotificationCenter, willPresent notification: UNNotification,
                                withCompletionHandler completionHandler: @escaping (UNNotificationPresentationOptions) -> Void) {
        completionHandler([.banner, .sound])
    }
}
