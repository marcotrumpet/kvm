import AppKit

/// One of this Mac's displays, as told to a viewer (see docs/protocol.md).
struct DisplayInfo: Codable, Equatable {
    let id: UInt32
    let name: String
    let width: Int
    let height: Int
    let main: Bool
}

enum Displays {
    /// Active displays, left to right.
    static func current() -> [DisplayInfo] {
        let key = NSDeviceDescriptionKey("NSScreenNumber")
        return NSScreen.screens
            .sorted { $0.frame.minX < $1.frame.minX }
            .compactMap { screen in
                guard let id = (screen.deviceDescription[key] as? NSNumber)?.uint32Value else { return nil }
                let mode = CGDisplayCopyDisplayMode(id)
                return DisplayInfo(id: id, name: screen.localizedName,
                                   width: mode?.pixelWidth ?? Int(screen.frame.width),
                                   height: mode?.pixelHeight ?? Int(screen.frame.height),
                                   main: id == CGMainDisplayID())
            }
    }

    static func encode(_ list: [DisplayInfo]) -> Data {
        (try? JSONEncoder().encode(list)) ?? Data("[]".utf8)
    }

    static func decode(_ data: Data) -> [DisplayInfo] {
        (try? JSONDecoder().decode([DisplayInfo].self, from: data)) ?? []
    }

    /// `id` if it is an active display, else the main display (also for 0).
    static func resolve(_ id: UInt32) -> CGDirectDisplayID {
        guard id != 0 else { return CGMainDisplayID() }
        var count: UInt32 = 0
        CGGetActiveDisplayList(0, nil, &count)
        var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
        CGGetActiveDisplayList(count, &ids, &count)
        return ids.contains(id) ? id : CGMainDisplayID()
    }
}
