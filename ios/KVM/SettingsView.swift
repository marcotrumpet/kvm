import SwiftUI

/// Defaults for controlling a computer, kept on this device. The control bar changes the
/// same settings while connected, so this shows whatever was picked last.
struct SettingsView: View {
    @Environment(\.dismiss) private var dismiss
    @AppStorage("touchMode") private var touchMode = RemoteViewController.TouchMode.trackpad.rawValue
    @AppStorage("pointerSpeed") private var pointerSpeed = 1.2
    @AppStorage("screenOn") private var screenOn = true
    @AppStorage("hostFit") private var hostFit = Int(Prefs.hostFit.rawValue)
    @AppStorage("textSize") private var textSize = Int(Prefs.textSize.rawValue)
    @AppStorage("videoQuality") private var quality = VideoQuality.auto.rawValue
    @AppStorage("sound") private var sound = true
    @AppStorage("sendMicrophone") private var sendMicrophone = false
    @AppStorage("viewOnly") private var viewOnly = false
    @State private var tipsReset = false
    private let lastResolution = Prefs.lastDefaultResolution

    var body: some View {
        NavigationStack {
            Form {
                Section {
                    Picker("Touches", selection: $touchMode) {
                        Text("Trackpad").tag(RemoteViewController.TouchMode.trackpad.rawValue)
                        Text("Direct touch").tag(RemoteViewController.TouchMode.touch.rawValue)
                    }
                    VStack(alignment: .leading) {
                        Text("Pointer speed")
                        Slider(value: $pointerSpeed, in: 0.5...3) {
                            Text("Pointer speed")
                        } minimumValueLabel: {
                            Image(systemName: "tortoise")
                        } maximumValueLabel: {
                            Image(systemName: "hare")
                        }
                    }
                } header: {
                    Text("Control")
                } footer: {
                    Text("Trackpad moves the pointer like a laptop's trackpad; direct touch clicks where you tap on the picture. Pointer speed is for trackpad mode and for a mouse or trackpad connected to an iPad.")
                }

                Section {
                    Toggle("Show the computer's screen", isOn: $screenOn)
                    Picker("Computer's screen", selection: $hostFit) {
                        Text("Keep its resolution").tag(Int(HostFit.off.rawValue))
                        Text("Fit it to this screen").tag(Int(HostFit.shape.rawValue))
                        Text("Use this as an extra screen").tag(Int(HostFit.extend.rawValue))
                    }
                    Picker("Resolution", selection: $textSize) {
                        ForEach(TextSize.ordered, id: \.rawValue) { size in
                            Text(lastResolution.map { size.label(size.resolution(default: $0.size)) } ?? size.title)
                                .tag(Int(size.rawValue))
                        }
                    }
                    Picker("Quality", selection: $quality) {
                        ForEach(VideoQuality.allCases, id: \.rawValue) { q in
                            Text(q.title).tag(q.rawValue)
                        }
                    }
                    Toggle("Play the computer's sound", isOn: $sound)
                    Toggle("View only", isOn: $viewOnly)
                } header: {
                    Text("Screen")
                } footer: {
                    if let last = lastResolution, !last.computer.isEmpty {
                        Text("Resolutions as on \(last.computer), the last computer shown. A lower one makes everything bigger and easier to tap. You can also change it while connected, with the aA button.")
                    } else {
                        Text("A lower resolution makes everything on the computer bigger and easier to tap. You can also change it while connected, with the aA button.")
                    }
                }

                Section {
                    Toggle("Send my microphone", isOn: $sendMicrophone)
                } header: {
                    Text("Microphone")
                } footer: {
                    Text("While you control a Mac, call apps there can use this device's microphone: choose \"KVM Microphone\" in them. The Mac needs the KVM Microphone driver (KVM Settings > Microphone).")
                }

                Section {
                    Button(tipsReset ? "Tips will show again" : "Show tips again") {
                        Prefs.tipsShown = []
                        tipsReset = true
                    }
                    .disabled(tipsReset)
                }
            }
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }
}
