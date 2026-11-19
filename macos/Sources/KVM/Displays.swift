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
                let b = CGDisplayBounds(id)
                return DisplayInfo(id: id, name: screen.localizedName,
                                   width: mode?.pixelWidth ?? Int(screen.frame.width),
                                   height: mode?.pixelHeight ?? Int(screen.frame.height),
                                   main: id == resolve(0),
                                   frame: [b.minX, b.minY, b.width, b.height])
            }
    }

    /// Active displays and where they are (global coordinates, top-left origin). Any thread.
    static func bounds() -> [(id: CGDirectDisplayID, rect: CGRect)] {
        var count: UInt32 = 0
        CGGetActiveDisplayList(0, nil, &count)
        var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
        CGGetActiveDisplayList(count, &ids, &count)
        return ids.prefix(Int(count)).map { ($0, CGDisplayBounds($0)) }
    }

    /// Whether `p`, on the edge of `rect`, is on the desktop's outer edge there: nothing
    /// beyond it on that side.
    static func isOuter(_ p: CGPoint, of rect: CGRect, edge: Edge, among rects: [CGRect]) -> Bool {
        let beyond = switch edge {
        case .left: CGPoint(x: rect.minX - 1, y: p.y)
        case .right: CGPoint(x: rect.maxX, y: p.y)
        case .top: CGPoint(x: p.x, y: rect.minY - 1)
        case .bottom: CGPoint(x: p.x, y: rect.maxY)
        }
        return !rects.contains { $0.contains(beyond) }
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
