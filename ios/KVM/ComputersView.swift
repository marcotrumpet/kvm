import SwiftUI
import VisionKit

/// The paired computers, and pairing new ones.
struct ComputersView: View {
    @ObservedObject var model: AppModel
    @State private var scanning = false
    @State private var enteringID = false
    @State private var typedID = ""

    var body: some View {
        NavigationStack {
            List {
                if let error = model.startError {
                    Section {
                        Label("KVM couldn't start: \(error)", systemImage: "exclamationmark.triangle")
                            .foregroundStyle(.red)
                    }
                }
                if let message = model.message {
                    Section {
                        Text(message).foregroundStyle(.secondary)
                    }
                }
                if model.computers.isEmpty {
                    Section {
                        howToPair
                    }
                } else {
                    Section("Computers") {
                        ForEach(model.computers) { computer in
                            Button {
                                model.connect(computer)
                                RemotePresenter.present(model)
                            } label: {
                                Label(computer.name, systemImage: "desktopcomputer")
                            }
                        }
                        .onDelete { offsets in offsets.map { model.computers[$0] }.forEach(model.remove) }
                    }
                }
            }
            .navigationTitle("KVM")
            .toolbar {
                ToolbarItem(placement: .primaryAction) {
                    Menu {
                        Button("Scan Pairing Code", systemImage: "qrcode.viewfinder") { scanning = true }
                            .disabled(!ScannerView.isAvailable)
                        Button("Enter Computer ID", systemImage: "keyboard") { enteringID = true }
                    } label: {
                        Label("Add Computer", systemImage: "plus")
                    }
                    .disabled(model.core == nil)
                }
            }
            .sheet(isPresented: $scanning) {
                ScannerView { text in
                    scanning = false
                    if let url = URL(string: text), model.open(url) { return }
                    model.pair(id: text, name: nil) // a bare ID
                }
                .ignoresSafeArea()
            }
            .alert("Computer ID", isPresented: $enteringID) {
                TextField("64 characters", text: $typedID)
                    .textInputAutocapitalization(.never)
                    .autocorrectionDisabled()
                Button("Pair") {
                    model.pair(id: typedID, name: nil)
                    typedID = ""
                }
                Button("Cancel", role: .cancel) { typedID = "" }
            } message: {
                Text("In KVM Settings on the computer, open \"Pair over the internet\" and copy its ID.")
            }
            .sheet(item: Binding(get: { model.pairing.map(PairingSheet.Item.init) }, set: { if $0 == nil { model.cancelPairing() } })) { item in
                PairingSheet(pairing: item.pairing) { model.cancelPairing() }
                    .presentationDetents([.medium])
            }
        }
    }

    private var howToPair: some View {
        VStack(alignment: .leading, spacing: 12) {
            Label("Control a Mac from here", systemImage: "desktopcomputer").font(.headline)
            Text("1. On the Mac, open KVM Settings from the menu bar.")
            Text("2. Scan the code under \"iPhone and iPad\" with the Camera, or tap + here.")
            Text("3. Allow it on the Mac.")
            if ScannerView.isAvailable {
                Button("Scan Pairing Code") { scanning = true }
                    .buttonStyle(.borderedProminent)
                    .padding(.top, 4)
            }
        }
        .padding(.vertical, 6)
    }
}

private struct PairingSheet: View {
    struct Item: Identifiable {
        let pairing: AppModel.PendingPairing
        var id: String { pairing.id }
    }

    let pairing: AppModel.PendingPairing
    let cancel: () -> Void

    var body: some View {
        VStack(spacing: 20) {
            ProgressView()
            Text("Allow this device on \(pairing.name)").font(.title3.bold()).multilineTextAlignment(.center)
            Text("Check that it shows the same code:").foregroundStyle(.secondary)
            Text(spaced(pairing.code))
                .font(.system(size: 40, weight: .semibold, design: .monospaced))
            Button("Cancel", role: .cancel, action: cancel)
        }
        .padding(32)
    }

    private func spaced(_ code: String) -> String {
        code.count == 6 ? "\(code.prefix(3)) \(code.suffix(3))" : code
    }
}

/// Reads one QR code with the camera.
struct ScannerView: UIViewControllerRepresentable {
    let onCode: (String) -> Void

    static var isAvailable: Bool { DataScannerViewController.isSupported && DataScannerViewController.isAvailable }

    func makeUIViewController(context: Context) -> DataScannerViewController {
        let scanner = DataScannerViewController(
            recognizedDataTypes: [.barcode(symbologies: [.qr])], qualityLevel: .balanced,
            isHighlightingEnabled: true)
        scanner.delegate = context.coordinator
        try? scanner.startScanning()
        return scanner
    }

    func updateUIViewController(_ controller: DataScannerViewController, context: Context) {}

    func makeCoordinator() -> Coordinator { Coordinator(onCode: onCode) }

    final class Coordinator: NSObject, DataScannerViewControllerDelegate {
        let onCode: (String) -> Void
        private var done = false

        init(onCode: @escaping (String) -> Void) { self.onCode = onCode }

        func dataScanner(_ scanner: DataScannerViewController, didAdd items: [RecognizedItem], allItems: [RecognizedItem]) {
            for case .barcode(let code) in items {
                guard !done, let text = code.payloadStringValue else { continue }
                done = true
                scanner.stopScanning()
                onCode(text)
            }
        }
    }
}
