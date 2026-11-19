import AppKit
import CoreGraphics

/// Replays the peer's input on this Mac. Only used from the core's event thread.
final class Injector {
    /// Tags injected events so this Mac's own event tap ignores them.
    static let marker: Int64 = 0x4B56_4D21

    private let source = CGEventSource(stateID: .hidSystemState)
    private var position = CGPoint.zero
    private var displays: [(id: CGDirectDisplayID, rect: CGRect)] = []
    private var buttons: Set<Int> = []
    private var keys: Set<UInt16> = []
    private var modifiers: Set<UInt16> = []
    private var lastClick: (button: Int, time: TimeInterval, at: CGPoint, count: Int64)?
    /// When control came from the peer's screen viewer: the display the pointer is kept on
    /// (as the peer named it, resolved on each move since displays come and go and resize;
    /// nil: anywhere on the desktop), the edges pushing past which is reported (`Edge`
    /// masks; on the whole desktop only its outer edges), and how far it has been pushed.
    private var confined: UInt32?
    private var exitEdges: UInt8 = 0
    private var overshoot = CGPoint.zero
    private var exitReported = false
    /// How far past an edge (points) counts as leaving, not just reaching it.
    private static let exitPush: CGFloat = 40
    /// Core thread: pushed past an edge (0 left, 1 right, 2 top, 3 bottom) of a display (its
    /// id), `along` it as a fraction 0...65535.
    var onEdgeExit: ((Int, UInt16, UInt32) -> Void)?

    func begin() {
        position = CGEvent(source: nil)?.location ?? .zero
        displays = Displays.bounds()
        confined = nil
        exitEdges = 0
    }

    /// Keeps the pointer on `display` (`UInt32.max`: the whole desktop) and reports pushes
    /// past `edges`.
    func confine(display: UInt32, edges: UInt8) {
        confined = display == UInt32.max ? nil : display
        exitEdges = edges
        overshoot = .zero
        exitReported = false
    }

    /// Releases anything still held. The core already sends releases on leave and on
    /// stalls; this is a second line of defense.
    func end() {
        confined = nil
        exitEdges = 0
        for vk in keys { postKey(vk: vk, down: false) }
        keys.removeAll()
        modifiers.removeAll()
        for b in buttons { postButton(b, down: false) }
        buttons.removeAll()
    }

    func move(dx: Int64, dy: Int64) {
        let target = CGPoint(x: position.x + CGFloat(dx), y: position.y + CGFloat(dy))
        if let confined {
            let id = Displays.resolve(confined)
            position = confine(target, to: CGDisplayBounds(id), id: id, outerOnly: false)
        } else {
            position = clamp(target)
            if exitEdges != 0, let d = displays.first(where: { $0.rect.contains(position) }) {
                _ = confine(target, to: d.rect, id: d.id, outerOnly: true)
            }
        }
        let type: CGEventType
        let button: CGMouseButton
        if buttons.contains(0) {
            (type, button) = (.leftMouseDragged, .left)
        } else if buttons.contains(1) {
            (type, button) = (.rightMouseDragged, .right)
        } else if let other = buttons.first {
            (type, button) = (.otherMouseDragged, CGMouseButton(rawValue: UInt32(other)) ?? .center)
        } else {
            (type, button) = (.mouseMoved, .left)
        }
        guard let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: button) else { return }
        e.setIntegerValueField(.mouseEventDeltaX, value: dx)
        e.setIntegerValueField(.mouseEventDeltaY, value: dy)
        post(e)
    }

    /// Moves the pointer to a point on a display (0: main), as fractions 0...65535.
    func warp(x: UInt16, y: UInt16, display: UInt32) {
        let bounds = CGDisplayBounds(Displays.resolve(display))
        position = CGPoint(
            x: bounds.minX + CGFloat(x) / 65535 * (bounds.width - 1),
            y: bounds.minY + CGFloat(y) / 65535 * (bounds.height - 1)
        )
        guard let e = CGEvent(mouseEventSource: source, mouseType: .mouseMoved, mouseCursorPosition: position, mouseButton: .left) else { return }
        post(e)
    }

    func scroll(dx: Int64, dy: Int64) {
        guard let e = CGEvent(
            scrollWheelEvent2Source: source, units: .pixel, wheelCount: 2,
            wheel1: Int32(clamping: dy), wheel2: Int32(clamping: dx), wheel3: 0
        ) else { return }
        post(e)
    }

    /// Mouse wheel notches, replayed as line scrolling like a local wheel.
    func wheel(dx: Int16, dy: Int16) {
        guard let e = CGEvent(
            scrollWheelEvent2Source: source, units: .line, wheelCount: 2,
            wheel1: Int32(dy), wheel2: Int32(dx), wheel3: 0
        ) else { return }
        post(e)
    }

    func button(_ b: Int, down: Bool) {
        if down { buttons.insert(b) } else { buttons.remove(b) }
        postButton(b, down: down)
    }

    func consumer(usage: UInt16, down: Bool) {
        if let action = MediaKeys.action(for: usage) {
            if down { perform(action) }
            return
        }
        guard let e = MediaKeys.event(usage: usage, down: down, source: source) else {
            Log.warn(String(format: "can't replay consumer usage 0x%03X", usage))
            return
        }
        post(e)
    }

    private func perform(_ action: MediaKeys.Action) {
        switch action {
        case .openApp(let paths):
            guard let path = paths.first(where: FileManager.default.fileExists(atPath:)) else {
                Log.warn("none of \(paths) exists")
                return
            }
            DispatchQueue.main.async {
                NSWorkspace.shared.openApplication(at: URL(fileURLWithPath: path), configuration: .init())
            }
        case .shortcut(let vk, let flags):
            for down in [true, false] {
                guard let e = CGEvent(keyboardEventSource: source, virtualKey: vk, keyDown: down) else { continue }
                post(e, extraFlags: flags.union(KeyMap.intrinsicFlags(vk: vk)))
            }
        }
    }

    func key(hid: UInt16, down: Bool) {
        guard let vk = KeyMap.vk(fromHID: hid) else { return }
        postKey(vk: vk, down: down)
    }

    /// Replays a pinch or rotation as the gesture events a trackpad produces (event type
    /// 29, fields as macOS's private gesture events use them), at the pointer.
    func gesture(_ g: Gesture) {
        guard let e = CGEvent(source: source), let type = CGEventType(rawValue: 29) else { return }
        e.type = type
        e.location = position
        e.setIntegerValueField(Injector.gestureSubtype, value: g.kind == .magnify ? 8 : 5)
        e.setDoubleValueField(g.kind == .magnify ? Injector.gestureMagnification : Injector.gestureRotation, value: g.value)
        e.setIntegerValueField(Injector.gesturePhase, value: Int64(g.phase.rawValue))
        post(e)
    }

    private static let gestureSubtype = CGEventField(rawValue: 110)!
    private static let gestureMagnification = CGEventField(rawValue: 113)!
    private static let gestureRotation = CGEventField(rawValue: 114)!
    private static let gesturePhase = CGEventField(rawValue: 132)!

    /// Types `text` as Unicode, whatever this Mac's keyboard layout; line breaks and tabs
    /// as the Return and Tab keys, which apps treat differently from typed characters.
    func type(_ text: String) {
        var chunk: [UniChar] = []
        func flush() {
            // Events carry at most 20 UTF-16 units of text.
            for start in stride(from: 0, to: chunk.count, by: 20) {
                let part = Array(chunk[start..<min(start + 20, chunk.count)])
                for down in [true, false] {
                    guard let e = CGEvent(keyboardEventSource: source, virtualKey: 0, keyDown: down) else { continue }
                    e.keyboardSetUnicodeString(stringLength: part.count, unicodeString: part)
                    post(e)
                }
            }
            chunk.removeAll()
        }
        for c in text {
            let vk: UInt16? = switch c {
            case "\n", "\r", "\r\n": 0x24
            case "\t": 0x30
            default: nil
            }
            if let vk {
                flush()
                postKey(vk: vk, down: true)
                postKey(vk: vk, down: false)
            } else {
                chunk.append(contentsOf: c.utf16)
            }
        }
        flush()
    }

    private func postKey(vk: UInt16, down: Bool) {
        let repeated = down && keys.contains(vk)
        if down { keys.insert(vk) } else { keys.remove(vk) }
        let isModifier = KeyMap.modifiers[vk] != nil
        if isModifier {
            if down { modifiers.insert(vk) } else { modifiers.remove(vk) }
        }
        guard let e = CGEvent(keyboardEventSource: source, virtualKey: vk, keyDown: down) else { return }
        if isModifier { e.type = .flagsChanged }
        if repeated { e.setIntegerValueField(.keyboardEventAutorepeat, value: 1) }
        post(e, extraFlags: KeyMap.intrinsicFlags(vk: vk))
    }

    private func postButton(_ b: Int, down: Bool) {
        let (type, button): (CGEventType, CGMouseButton) = switch b {
        case 0: (down ? .leftMouseDown : .leftMouseUp, .left)
        case 1: (down ? .rightMouseDown : .rightMouseUp, .right)
        default: (down ? .otherMouseDown : .otherMouseUp, CGMouseButton(rawValue: UInt32(b)) ?? .center)
        }
        guard let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: position, mouseButton: button) else { return }
        e.setIntegerValueField(.mouseEventButtonNumber, value: Int64(b))
        e.setIntegerValueField(.mouseEventClickState, value: clickCount(button: b, down: down))
        post(e)
    }

    /// Double/triple clicks: consecutive presses of one button, close in time and space.
    private func clickCount(button: Int, down: Bool) -> Int64 {
        let now = ProcessInfo.processInfo.systemUptime
        guard down else { return lastClick?.count ?? 1 }
        var count: Int64 = 1
        if let last = lastClick, last.button == button,
           now - last.time <= NSEvent.doubleClickInterval,
           abs(last.at.x - position.x) <= 4, abs(last.at.y - position.y) <= 4 {
            count = last.count + 1
        }
        lastClick = (button, now, position, count)
        return count
    }

    private func post(_ e: CGEvent, extraFlags: CGEventFlags = []) {
        e.flags = KeyMap.flags(forHeld: modifiers).union(extraFlags)
        e.setIntegerValueField(.eventSourceUserData, value: Injector.marker)
        e.post(tap: .cghidEventTap)
    }

    /// Clamps to display `r`, accumulating pushes past its edges until they count as
    /// leaving, if `exitEdges` has that edge (and with `outerOnly`, nothing is beyond it).
    /// Not while dragging: dropping something at the edge isn't leaving.
    private func confine(_ p: CGPoint, to r: CGRect, id: CGDirectDisplayID, outerOnly: Bool) -> CGPoint {
        let clamped = CGPoint(x: min(max(p.x, r.minX), r.maxX - 1), y: min(max(p.y, r.minY), r.maxY - 1))
        let pushX = p.x - clamped.x, pushY = p.y - clamped.y
        // Keep accumulating only while pushing the same way; moving back along resets.
        overshoot.x = pushX == 0 ? 0 : (overshoot.x.sign == pushX.sign ? overshoot.x : 0) + pushX
        overshoot.y = pushY == 0 ? 0 : (overshoot.y.sign == pushY.sign ? overshoot.y : 0) + pushY
        if !exitReported, buttons.isEmpty, max(abs(overshoot.x), abs(overshoot.y)) >= Injector.exitPush {
            let horizontal = abs(overshoot.x) >= abs(overshoot.y)
            let edge: Edge = horizontal ? (overshoot.x < 0 ? .left : .right) : (overshoot.y < 0 ? .top : .bottom)
            if exitEdges & edge.mask != 0,
               !outerOnly || Displays.isOuter(clamped, of: r, edge: edge, among: displays.map(\.rect)) {
                exitReported = true
                let fraction = horizontal ? (clamped.y - r.minY) / r.height : (clamped.x - r.minX) / r.width
                onEdgeExit?(edge.rawValue, UInt16(clamping: Int((min(max(fraction, 0), 1) * 65535).rounded())), id)
            }
        }
        return clamped
    }

    /// Keeps the cursor on a display: inside one as-is, otherwise clamped into the display
    /// it was on, so it can't get lost in the gaps of an uneven multi-monitor layout.
    private func clamp(_ p: CGPoint) -> CGPoint {
        if displays.contains(where: { $0.rect.contains(p) }) { return p }
        // Maybe the layout changed since control began (a display resized to the viewer's
        // shape, added or removed): look again before holding the pointer back.
        displays = Displays.bounds()
        if displays.contains(where: { $0.rect.contains(p) }) { return p }
        let current = displays.first(where: { $0.rect.contains(position) })?.rect ?? displays.first?.rect ?? .zero
        return CGPoint(
            x: min(max(p.x, current.minX), current.maxX - 1),
            y: min(max(p.y, current.minY), current.maxY - 1)
        )
    }

}
