import AppKit
import os

enum Displays {
    /// A virtual display added as an extra screen for a viewer: what "the main display"
    /// (id 0) means to it while it exists. 0 when there is none. Any thread.
    static let extended = OSAllocatedUnfairLock<CGDirectDisplayID>(initialState: 0)

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
                                   main: id == resolve(0))
            }
    }

    static func encode(_ list: [DisplayInfo]) -> Data {
        (try? JSONEncoder().encode(list)) ?? Data("[]".utf8)
    }

    static func decode(_ data: Data) -> [DisplayInfo] {
        (try? JSONDecoder().decode([DisplayInfo].self, from: data)) ?? []
    }

    /// `id` if it is an active display, else the main display (also for 0; the extra
    /// screen made for a viewer, while there is one).
    static func resolve(_ id: UInt32) -> CGDirectDisplayID {
        let extra = extended.withLock { $0 }
        let main = extra != 0 ? extra : CGMainDisplayID()
        guard id != 0 else { return main }
        var count: UInt32 = 0
        CGGetActiveDisplayList(0, nil, &count)
        var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
        CGGetActiveDisplayList(count, &ids, &count)
        return ids.contains(id) ? id : main
    }
}
