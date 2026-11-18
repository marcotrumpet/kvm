import SwiftUI

@main
struct KVMApp: App {
    @StateObject private var model = AppModel()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ComputersView(model: model)
                .onOpenURL { model.open($0) }
                #if DEBUG
                // Testing without the camera: launch with `-pairURL kvm://pair?id=…`.
                .task {
                    if let link = UserDefaults.standard.string(forKey: "pairURL"), let url = URL(string: link) {
                        model.open(url)
                    }
                }
                #endif
                .onChange(of: scenePhase) { _, phase in
                    switch phase {
                    case .background: model.suspend()
                    case .active: model.resume()
                    default: break
                    }
                }
        }
    }
}

/// Shows the control screen. It is presented by UIKit rather than SwiftUI so that iPadOS
/// asks it about pointer lock (a mouse then moves the computer's pointer, not this one's).
@MainActor
enum RemotePresenter {
    static func present(_ model: AppModel) {
        let scenes = UIApplication.shared.connectedScenes.compactMap { $0 as? UIWindowScene }
        let scene = scenes.first { $0.activationState == .foregroundActive } ?? scenes.first
        // No key window while a system alert has focus.
        guard let root = (scene?.keyWindow ?? scene?.windows.first)?.rootViewController else { return }
        let show = { root.present(RemoteViewController(model: model), animated: true) }
        // Not on top of a sheet (the pairing one): dismissing it would take this along.
        if let presented = root.presentedViewController {
            guard !(presented is RemoteViewController) else { return }
            presented.dismiss(animated: true, completion: show)
        } else {
            show()
        }
    }
}
