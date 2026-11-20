import AppKit

/// Nearby KVM machines and the pairing flow. Main thread only.
final class Pairing: ObservableObject {
    struct Machine: Identifiable, Equatable {
        let id: String
        var name: String
    }

    struct Outgoing: Equatable {
        let id: String
        let name: String
        /// Known once the other machine has the request.
        var code: String?
    }

    @Published private(set) var nearby: [Machine] = []
    @Published private(set) var pairedID = Settings.peerID
    @Published private(set) var outgoing: Outgoing?
    @Published var message: String?

    private let controller: Controller
    let remotes: RemoteHost
    private var core: Core { controller.core }

    init(controller: Controller, remotes: RemoteHost) {
        self.controller = controller
        self.remotes = remotes
        core.onMachineEvent = { [weak self] event in
            DispatchQueue.main.async { self?.handle(event) }
        }
        core.watchMachines()
    }

    /// Name of the paired machine: from the live session, else from discovery.
    var pairedName: String? {
        controller.state.peerName ?? nearby.first(where: { $0.id == pairedID })?.name
    }

    func requestPairing(with machine: Machine) {
        guard core.requestPairing(machine.id) else { return }
        message = nil
        outgoing = Outgoing(id: machine.id, name: machine.name)
    }

    /// Pairs with `id` without asking the other machine (it must do the same with ours).
    /// Returns false if the id is invalid.
    @discardableResult
    func adopt(_ id: String) -> Bool {
        guard controller.setPeer(id) else { return false }
        Settings.peerID = id
        pairedID = id
        return true
    }

    func unpair() {
        controller.clearPeer()
        Settings.peerID = nil
        pairedID = nil
    }

    private func handle(_ event: MachineEvent) {
        switch event {
        case .discovered(let id, let name):
            if let i = nearby.firstIndex(where: { $0.id == id }) {
                nearby[i].name = name
            } else {
                nearby.append(Machine(id: id, name: name))
                nearby.sort { $0.name.localizedStandardCompare($1.name) == .orderedAscending }
            }
        case .expired(let id):
            nearby.removeAll { $0.id == id }
        case .pairRequest(let id, let name, let code, let remote):
            if remote {
                askToAcceptRemote(id: id, name: name, code: code)
            } else {
                askToAccept(id: id, name: name, code: code)
            }
        case .pairCode(let id, let code):
            if outgoing?.id == id { outgoing?.code = code }
        case .pairResult(let id, let accepted, let detail, let failed):
            guard outgoing?.id == id else { return }
            let name = outgoing?.name ?? detail
            outgoing = nil
            if accepted {
                adopt(id)
                message = "Paired with \(name)."
            } else if !failed {
                message = "\(name) declined."
            } else {
                message = "Couldn't pair with \(name): \(detail)"
            }
        }
    }

    private func askToAccept(id: String, name: String, code: String) {
        let alert = NSAlert()
        alert.messageText = "\(name) wants to pair with this Mac"
        var info = "Check that \(name) shows the same code:\n\n\(Pairing.spaced(code))"
        if let current = pairedName ?? pairedID.map({ String($0.prefix(8)) }), pairedID != id {
            info += "\n\nThis replaces the pairing with \(current)."
        }
        alert.informativeText = info
        alert.addButton(withTitle: "Pair")
        alert.addButton(withTitle: "Decline")
        NSApp.activate(ignoringOtherApps: true)
        let accept = alert.runModal() == .alertFirstButtonReturn
        if core.respondPairing(id, accept: accept) {
            if accept {
                adopt(id)
                message = "Paired with \(name)."
            }
        } else if accept {
            message = "The request from \(name) expired. Ask again from \(name)."
        }
    }

    private func askToAcceptRemote(id: String, name: String, code: String) {
        let alert = NSAlert()
        alert.messageText = "\(name) wants to control this Mac"
        alert.informativeText = "Check that \(name) shows the same code:\n\n\(Pairing.spaced(code))\n\nIt will be able to use this Mac's mouse and keyboard and see its screen. The pairing with your other computer stays as it is."
        alert.addButton(withTitle: "Allow")
        alert.addButton(withTitle: "Decline")
        NSApp.activate(ignoringOtherApps: true)
        let accept = alert.runModal() == .alertFirstButtonReturn
        if core.respondPairing(id, accept: accept) {
            if accept {
                remotes.add(id: id, name: name)
                message = "\(name) can now control this Mac."
            }
        } else if accept {
            message = "The request from \(name) expired. Try again on \(name)."
        }
    }

    /// What a phone or tablet scans to pair with this Mac.
    func pairingLink(myID: String) -> String {
        var c = URLComponents()
        c.scheme = "kvm"
        c.host = "pair"
        c.queryItems = [URLQueryItem(name: "id", value: myID),
                        URLQueryItem(name: "name", value: Host.current().localizedName ?? "Mac")]
        return c.string ?? "kvm://pair?id=\(myID)"
    }

    /// "823342" -> "823 342"
    static func spaced(_ code: String) -> String {
        guard code.count == 6 else { return code }
        return "\(code.prefix(3)) \(code.suffix(3))"
    }
}
