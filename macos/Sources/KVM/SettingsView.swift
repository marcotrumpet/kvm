import AppKit
import Combine
import SwiftUI

struct SettingsView: View {
    let myID: String
    @ObservedObject var state: AppState
    @ObservedObject var pairing: Pairing
    @ObservedObject var remotes: RemoteState
    let saveHotkey: (Hotkey) -> Void
    /// Returns false if the helper install was cancelled or failed.
    let setPauseAwdl: (Bool) -> Bool
    /// Returns an error message on failure.
    let uninstallAwdlHelper: () -> String?

    @State private var peerID = Settings.peerID ?? ""
    @State private var peerError: String?
    @State private var hotkey = Settings.hotkey
    @State private var recording = false
    @State private var monitor: Any?
    @State private var launchAtLogin = Settings.launchAtLogin
    @State private var shareClipboard = Settings.shareClipboard
    @State private var clipboardFileLimit = Settings.clipboardFileLimit
    @State private var downloadDirectory = Settings.downloadDirectory
    @State private var pauseAwdl = Settings.pauseAwdl
    @State private var awdlInstalled = Awdl.isInstalled
    @State private var awdlUp = Awdl.isUp
    @State private var awdlError: String?
    @State private var accessibility = Permissions.accessibility
    @State private var inputMonitoring = Permissions.inputMonitoring
    @State private var screenRecording = Permissions.screenRecording
    private let refresh = Timer.publish(every: 1, on: .main, in: .common).autoconnect()

    var body: some View {
        Form {
            Section("Pairing") {
                if let pairedID = pairing.pairedID {
                    LabeledContent(pairing.pairedName ?? String(pairedID.prefix(12)) + "…") {
                        HStack {
                            Text(connectionText).foregroundStyle(.secondary)
                            Button("Unpair") { pairing.unpair() }
                        }
                    }
                } else {
                    Text("Not paired. Pick the other computer below.").foregroundStyle(.secondary)
                }
                if let message = pairing.message {
                    Text(message).foregroundStyle(.secondary)
                }
            }

            Section {
                if pairing.nearby.isEmpty {
                    HStack(spacing: 8) {
                        ProgressView().controlSize(.small)
                        Text("Looking for computers running KVM on this network…")
                            .foregroundStyle(.secondary)
                    }
                }
                ForEach(pairing.nearby) { machine in
                    LabeledContent(machine.name) {
                        if machine.id == pairing.pairedID {
                            Label("Paired", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
                        } else if let outgoing = pairing.outgoing, outgoing.id == machine.id {
                            HStack(spacing: 8) {
                                ProgressView().controlSize(.small)
                                Text("Accept on \(machine.name) · code \(Pairing.spaced(outgoing.code))")
                                    .foregroundStyle(.secondary)
                            }
                        } else {
                            Button("Pair") { pairing.requestPairing(with: machine) }
                                .disabled(pairing.outgoing != nil)
                        }
                    }
                }
            } header: {
                Text("On this network")
            } footer: {
                Text("While this window is open, nearby computers can see this Mac. Open KVM Settings on the other computer too; it asks you to confirm.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section {
                DisclosureGroup("Pair over the internet (by ID)") {
                    LabeledContent("This Mac's ID") {
                        HStack {
                            Text(myID)
                                .font(.system(.caption, design: .monospaced))
                                .textSelection(.enabled)
                                .lineLimit(2)
                            Button("Copy") {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString(myID, forType: .string)
                            }
                        }
                    }
                    TextField("Other computer's ID", text: $peerID, prompt: Text("Paste the other computer's ID"))
                        .font(.system(.body, design: .monospaced))
                    HStack {
                        Text("Enter each computer's ID on the other one.")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                        Spacer()
                        Button("Save") {
                            let id = peerID.trimmingCharacters(in: .whitespacesAndNewlines)
                            peerError = pairing.adopt(id) ? nil : "That doesn't look like a valid ID."
                        }
                        .disabled(peerID.trimmingCharacters(in: .whitespacesAndNewlines) == (pairing.pairedID ?? ""))
                    }
                    if let peerError {
                        Text(peerError).foregroundStyle(.red)
                    }
                }
            }

            Section {
                HStack(alignment: .top, spacing: 16) {
                    if let qr = QRCode.image(pairing.pairingLink(myID: myID)) {
                        Image(nsImage: qr)
                            .interpolation(.none)
                            .resizable()
                            .frame(width: 132, height: 132)
                            .accessibilityLabel("Pairing code for KVM on iPhone or iPad")
                    }
                    Text("Scan this with the Camera or the KVM app on your iPhone or iPad to control this Mac from it. It's added next to the computer paired above, not instead of it.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
                ForEach(remotes.paired.sorted(by: { $0.value.localizedStandardCompare($1.value) == .orderedAscending }), id: \.key) { id, name in
                    LabeledContent(name) {
                        HStack {
                            if remotes.connectedID == id {
                                Text("Connected").foregroundStyle(.secondary)
                            }
                            Button("Remove") { pairing.remotes.remove(id: id) }
                        }
                    }
                }
            } header: {
                Text("iPhone and iPad")
            }

            Section("Switching") {
                LabeledContent("Switch shortcut") {
                    Button(recording ? "Press a shortcut…" : hotkey.display) {
                        recording ? stopRecording() : startRecording()
                    }
                }
                Text("Press the shortcut on the computer whose keyboard you're using. Use at least one of ⌃ ⌥ ⌘.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if awdlUp != nil || awdlInstalled {
                Section("Wi-Fi") {
                    Toggle("Pause AirDrop while connected", isOn: $pauseAwdl)
                        .onChange(of: pauseAwdl) { on in
                            guard on != Settings.pauseAwdl else { return }
                            if setPauseAwdl(on) {
                                awdlError = nil
                            } else {
                                awdlError = "The helper wasn't installed."
                                pauseAwdl = false
                            }
                            awdlInstalled = Awdl.isInstalled
                        }
                    Text("AirDrop's Wi-Fi radio hopping (AWDL) causes lag spikes. While the computers are connected, KVM keeps it off\(awdlUp == false ? " (paused now)" : ""). AirDrop, Sidecar and Universal Control don't work during that time. Installing the helper needs your admin password once.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                    if let awdlError {
                        Text(awdlError).foregroundStyle(.red)
                    }
                    if awdlInstalled {
                        Button("Remove Helper") {
                            awdlError = uninstallAwdlHelper()
                            awdlInstalled = Awdl.isInstalled
                            pauseAwdl = Settings.pauseAwdl
                        }
                    }
                }
            }

            Section("Sharing") {
                Toggle("Share clipboard", isOn: $shareClipboard)
                    .onChange(of: shareClipboard) { Settings.shareClipboard = $0 }
                Text("Copy on one computer, paste on the other: text, rich text and images. Passwords copied from password managers are never shared.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                Picker("Copied files", selection: $clipboardFileLimit) {
                    ForEach(Settings.clipboardFileLimitChoices, id: \.bytes) { choice in
                        Text(choice.bytes == 0 || choice.bytes == .max ? choice.label : "Up to \(choice.label)")
                            .tag(choice.bytes)
                    }
                }
                .disabled(!shareClipboard)
                .onChange(of: clipboardFileLimit) { Settings.clipboardFileLimit = $0 }
                Text("Files you copy in Finder on this Mac are sent to the other computer right away, so they can be pasted there. Bigger copies are left to Send Files.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
                LabeledContent("Save received files to") {
                    HStack {
                        Text(downloadDirectory.lastPathComponent)
                        Button("Change…") {
                            let panel = NSOpenPanel()
                            panel.canChooseFiles = false
                            panel.canChooseDirectories = true
                            panel.canCreateDirectories = true
                            panel.directoryURL = downloadDirectory
                            if panel.runModal() == .OK, let url = panel.url {
                                Settings.downloadDirectory = url
                                downloadDirectory = url
                            }
                        }
                    }
                }
                Text("To send files, drop them on the KVM menu bar icon or the screen viewer, or use Send Files in the menu.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("General") {
                Toggle("Open at login", isOn: $launchAtLogin)
                    .onChange(of: launchAtLogin) { Settings.launchAtLogin = $0 }
            }

            Section("Permissions") {
                permissionRow("Accessibility", granted: accessibility, request: Permissions.requestAccessibility)
                permissionRow("Input Monitoring", granted: inputMonitoring, request: Permissions.requestInputMonitoring)
                permissionRow("Screen Recording (to share this screen)", granted: screenRecording, request: Permissions.requestScreenRecording)
                if !(accessibility && inputMonitoring) {
                    Text("KVM needs both to capture and replay the mouse and keyboard. If you just granted them and nothing changes, quit and reopen KVM.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .formStyle(.grouped)
        .frame(minWidth: 460, idealWidth: 580, minHeight: 360, idealHeight: 720)
        .onReceive(refresh) { _ in
            accessibility = Permissions.accessibility
            inputMonitoring = Permissions.inputMonitoring
            screenRecording = Permissions.screenRecording
            awdlUp = Awdl.isUp
        }
        .onDisappear(perform: stopRecording)
    }

    private var connectionText: String {
        guard state.connected else { return "Connecting…" }
        var parts = ["Connected"]
        if let direct = state.direct { parts.append(direct ? "direct" : "relayed") }
        if let ms = state.latencyMs { parts.append(String(format: "%.0f ms", ms)) }
        return parts.joined(separator: " · ")
    }

    private func permissionRow(_ title: String, granted: Bool, request: @escaping () -> Void) -> some View {
        LabeledContent(title) {
            if granted {
                Label("Granted", systemImage: "checkmark.circle.fill").foregroundStyle(.green)
            } else {
                Button("Grant…", action: request)
            }
        }
    }

    private func startRecording() {
        recording = true
        monitor = NSEvent.addLocalMonitorForEvents(matching: .keyDown) { event in
            if event.keyCode == 0x35 { // escape
                stopRecording()
                return nil
            }
            let flags = CGEventFlags(rawValue: UInt64(event.modifierFlags.rawValue))
                .intersection(Hotkey.relevantFlags)
            guard !flags.intersection([.maskCommand, .maskControl, .maskAlternate]).isEmpty else {
                NSSound.beep()
                return nil
            }
            let label = event.charactersIgnoringModifiers?.uppercased() ?? "?"
            hotkey = Hotkey(keyCode: event.keyCode, modifiers: flags, label: label.isEmpty ? "?" : label)
            saveHotkey(hotkey)
            stopRecording()
            return nil
        }
    }

    private func stopRecording() {
        recording = false
        if let monitor { NSEvent.removeMonitor(monitor) }
        monitor = nil
    }
}
