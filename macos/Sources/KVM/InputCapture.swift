import CoreGraphics
import Foundation
import os

/// The global switch shortcut: a key plus modifiers (command/control/option/shift).
struct Hotkey: Equatable {
    static let relevantFlags: CGEventFlags = [.maskCommand, .maskControl, .maskAlternate, .maskShift]
    static let `default` = Hotkey(keyCode: 0x28, modifiers: [.maskControl, .maskAlternate, .maskCommand], label: "K")

    var keyCode: UInt16
    var modifiers: CGEventFlags
    var label: String

    func matches(keyCode: UInt16, flags: CGEventFlags) -> Bool {
        keyCode == self.keyCode && flags.intersection(Hotkey.relevantFlags) == modifiers
    }

    /// Names for keys whose characters don't print well.
    private static let keyNames: [UInt16: String] = [
        0x30: "⇥", 0x31: "Space", 0x24: "↩", 0x4C: "⌤", 0x33: "⌫", 0x75: "⌦", 0x35: "⎋",
        0x7B: "←", 0x7C: "→", 0x7D: "↓", 0x7E: "↑", 0x73: "↖", 0x77: "↘", 0x74: "⇞", 0x79: "⇟",
        0x7A: "F1", 0x78: "F2", 0x63: "F3", 0x76: "F4", 0x60: "F5", 0x61: "F6", 0x62: "F7",
        0x64: "F8", 0x65: "F9", 0x6D: "F10", 0x67: "F11", 0x6F: "F12",
    ]

    var display: String {
        var s = ""
        if modifiers.contains(.maskControl) { s += "⌃" }
        if modifiers.contains(.maskAlternate) { s += "⌥" }
        if modifiers.contains(.maskShift) { s += "⇧" }
        if modifiers.contains(.maskCommand) { s += "⌘" }
        return s + (Hotkey.keyNames[keyCode] ?? label)
    }
}

/// Session-wide event tap on its own high-priority thread. In local mode it only watches
/// for the hotkey; in remote mode it swallows all input and forwards it to the peer.
final class InputCapture {
    private struct State {
        var remote = false
        var hotkey = Hotkey.default
        /// Modifiers held locally when control moved to the peer. Their releases must
        /// still reach this Mac, or they would stay stuck down here.
        var localHeld: Set<UInt16> = []
        /// Keys whose key-up is swallowed because their key-down was the hotkey.
        var swallowKeyUp: Set<UInt16> = []
        var fracX = 0.0
        var fracY = 0.0
        /// Modifier keys (virtual key codes) the peer currently has held because of us.
        var sentModifiers: Set<UInt16> = []
        /// Of those, the ones pressed only because a key event carried the modifier flag
        /// without a modifier key press (Logi Options+ and other remappers post e.g. ⌃←
        /// as a single key event). Released as soon as no key or button is down.
        var syntheticModifiers: Set<UInt16> = []
        var keysDown: Set<UInt16> = []
        var buttonsDown: Set<Int> = []
        /// The screen viewer's picture (global coordinates) and whether entering it should
        /// take control. Re-armed only once the pointer has been seen outside it.
        var hoverRect: CGRect?
        var hoverArmed = false
        /// Unforwardable keys already logged, so each is reported once.
        var loggedUnknown: Set<Int> = []
        /// The three-finger swipe in progress: direction (1 horizontal, 2 vertical) and how far.
        var swipeMotion: Int64 = 0
        var swipeProgress = 0.0
    }

    private let state = OSAllocatedUnfairLock(initialState: State())
    private let core: Core
    private var tap: CFMachPort?
    /// Called on the tap thread when the hotkey is pressed.
    var onHotkey: (() -> Void)?
    /// Called on the tap thread when the pointer enters the viewer's picture (global point).
    var onHoverEnter: ((CGPoint) -> Void)?

    /// Main thread: where the viewer's picture is, or nil to stop watching.
    func setHoverRect(_ rect: CGRect?) {
        state.withLock {
            if $0.hoverRect == nil, let rect { $0.hoverArmed = !rect.contains(CGEvent(source: nil)?.location ?? .zero) }
            $0.hoverRect = rect
        }
    }

    /// The pointer may still be over the picture (e.g. after the hotkey): don't take control
    /// again until it has left.
    func disarmHover() {
        state.withLock { $0.hoverArmed = false }
    }

    init(core: Core) {
        self.core = core
    }

    var isRunning: Bool { tap != nil }

    func setHotkey(_ hotkey: Hotkey) {
        state.withLock { $0.hotkey = hotkey }
    }

    /// Starts swallowing and forwarding input. Call from the tap thread or main thread.
    func beginRemote() {
        let held = KeyMap.heldModifiers(in: CGEventSource.flagsState(.hidSystemState))
        state.withLock {
            $0.remote = true
            $0.localHeld = held
            $0.sentModifiers = []
            $0.syntheticModifiers = []
            $0.keysDown = []
            $0.buttonsDown = []
            $0.fracX = 0
            $0.fracY = 0
        }
    }

    func endRemote() {
        state.withLock { $0.remote = false }
    }

    /// Creates the tap. Fails (returns false) until Accessibility / Input Monitoring are granted.
    func start() -> Bool {
        if tap != nil { return true }
        var mask: CGEventMask = 0
        let types: [UInt32] = [
            CGEventType.mouseMoved, .leftMouseDown, .leftMouseUp, .leftMouseDragged,
            .rightMouseDown, .rightMouseUp, .rightMouseDragged,
            .otherMouseDown, .otherMouseUp, .otherMouseDragged,
            .scrollWheel, .keyDown, .keyUp, .flagsChanged,
        ].map(\.rawValue) + InputCapture.gestureTypes + [MediaKeys.systemDefinedType]
        for t in types { mask |= 1 << CGEventMask(t) }

        guard let tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap,
            place: .headInsertEventTap,
            options: .defaultTap,
            eventsOfInterest: mask,
            callback: { _, type, event, ctx in
                let capture = Unmanaged<InputCapture>.fromOpaque(ctx!).takeUnretainedValue()
                return capture.handle(type: type, event: event)
            },
            userInfo: Unmanaged.passUnretained(self).toOpaque()
        ) else { return false }
        self.tap = tap

        let thread = Thread {
            let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
            CGEvent.tapEnable(tap: tap, enable: true)
            CFRunLoopRun()
        }
        thread.name = "kvm-event-tap"
        thread.qualityOfService = .userInteractive
        thread.start()
        return true
    }

    /// Trackpad gesture event types (NSEvent types without CGEventType cases): rotate,
    /// begin/end gesture, gesture, magnify, swipe, smart magnify, pressure.
    private static let gestureTypes: [UInt32] = [18, 19, 20, 29, 30, 31, 32, 34]

    private func handle(type: CGEventType, event: CGEvent) -> Unmanaged<CGEvent>? {
        let pass = Unmanaged.passUnretained(event)
        if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
            Log.warn("event tap disabled by macOS (\(type == .tapDisabledByTimeout ? "timeout" : "user input")), re-enabling")
            if let tap { CGEvent.tapEnable(tap: tap, enable: true) }
            return pass
        }
        if event.getIntegerValueField(.eventSourceUserData) == Injector.marker {
            return pass
        }

        if type == .keyDown || type == .keyUp {
            let vk = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
            if type == .keyDown {
                let isHotkey = state.withLock { s -> Bool in
                    guard s.hotkey.matches(keyCode: vk, flags: event.flags) else { return false }
                    s.swallowKeyUp.insert(vk)
                    return true
                }
                if isHotkey {
                    if event.getIntegerValueField(.keyboardEventAutorepeat) == 0 { onHotkey?() }
                    return nil
                }
            } else if state.withLock({ $0.swallowKeyUp.remove(vk) != nil }) {
                return nil
            }
        }

        guard state.withLock({ $0.remote }) else {
            if type == .mouseMoved { watchHover(event.location) }
            return pass
        }
        if type.rawValue == MediaKeys.systemDefinedType {
            // Only media keys are forwarded; other system-defined events stay on this Mac.
            guard let (nx, down) = MediaKeys.decode(event) else { return pass }
            guard let usage = MediaKeys.usage(forNX: nx) else {
                logUnknown(nx | 0x10000, "media key NX \(nx)")
                return pass
            }
            core.sendConsumer(usage: usage, down: down)
            return nil
        }
        forward(type: type, event: event)
        return forwardedPassThrough(type: type, event: event) ? pass : nil
    }

    /// Modifier releases of keys held locally at the switch go to this Mac, not the peer.
    private func forwardedPassThrough(type: CGEventType, event: CGEvent) -> Bool {
        guard type == .flagsChanged else { return false }
        let vk = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
        guard let m = KeyMap.modifiers[vk], event.flags.rawValue & m.deviceMask == 0 else { return false }
        return state.withLock { $0.localHeld.remove(vk) != nil }
    }

    private func watchHover(_ location: CGPoint) {
        let entered = state.withLock { s -> Bool in
            guard let rect = s.hoverRect else { return false }
            // A few points inside, so brushing the window edge doesn't count.
            guard rect.insetBy(dx: 6, dy: 6).contains(location) else {
                if !rect.contains(location) { s.hoverArmed = true }
                return false
            }
            guard s.hoverArmed else { return false }
            s.hoverArmed = false
            return true
        }
        if entered { onHoverEnter?(location) }
    }

    private func logUnknown(_ id: Int, _ what: String) {
        guard state.withLock({ $0.loggedUnknown.insert(id).inserted }) else { return }
        Log.warn("not forwarded: \(what) (no mapping yet)")
    }

    /// The generic modifier flags and the modifier key pressed to produce each.
    private static let modifierGroups: [(flag: CGEventFlags, press: UInt16)] = [
        (.maskControl, 0x3B), (.maskShift, 0x38), (.maskAlternate, 0x3A), (.maskCommand, 0x37),
    ]

    /// Makes the peer's held modifiers match `flags` before a key or click is sent.
    private func syncModifiers(to flags: CGEventFlags) {
        let (presses, releases) = state.withLock { s -> ([UInt16], [UInt16]) in
            var presses: [UInt16] = []
            var releases: [UInt16] = []
            for (flag, press) in InputCapture.modifierGroups {
                let keys = Set(KeyMap.modifiers.filter { $0.value.flag == flag }.keys)
                let held = s.sentModifiers.intersection(keys)
                if flags.contains(flag), held.isEmpty {
                    presses.append(press)
                    s.sentModifiers.insert(press)
                    s.syntheticModifiers.insert(press)
                } else if !flags.contains(flag) {
                    let synthetic = held.intersection(s.syntheticModifiers)
                    releases += synthetic
                    s.sentModifiers.subtract(synthetic)
                    s.syntheticModifiers.subtract(synthetic)
                }
            }
            return (presses, releases)
        }
        for vk in releases { if let hid = KeyMap.hid(fromVK: vk) { core.sendKey(hid: hid, down: false) } }
        for vk in presses { if let hid = KeyMap.hid(fromVK: vk) { core.sendKey(hid: hid, down: true) } }
    }

    private func releaseSyntheticModifiersIfIdle() {
        let releases = state.withLock { s -> Set<UInt16> in
            guard s.keysDown.isEmpty, s.buttonsDown.isEmpty else { return [] }
            let synthetic = s.syntheticModifiers
            s.sentModifiers.subtract(synthetic)
            s.syntheticModifiers.removeAll()
            return synthetic
        }
        for vk in releases { if let hid = KeyMap.hid(fromVK: vk) { core.sendKey(hid: hid, down: false) } }
    }

    private func forward(type: CGEventType, event: CGEvent) {
        switch type {
        case .mouseMoved, .leftMouseDragged, .rightMouseDragged, .otherMouseDragged:
            let (dx, dy) = state.withLock { s -> (Int64, Int64) in
                s.fracX += event.getDoubleValueField(.mouseEventDeltaX)
                s.fracY += event.getDoubleValueField(.mouseEventDeltaY)
                let dx = s.fracX.rounded(.towardZero), dy = s.fracY.rounded(.towardZero)
                s.fracX -= dx
                s.fracY -= dy
                return (Int64(dx), Int64(dy))
            }
            if dx != 0 || dy != 0 { core.sendMotion(dx: dx, dy: dy) }
        case .leftMouseDown, .leftMouseUp, .rightMouseDown, .rightMouseUp, .otherMouseDown, .otherMouseUp:
            let button = switch type {
            case .leftMouseDown, .leftMouseUp: 0
            case .rightMouseDown, .rightMouseUp: 1
            default: Int(event.getIntegerValueField(.mouseEventButtonNumber))
            }
            let down = type == .leftMouseDown || type == .rightMouseDown || type == .otherMouseDown
            if down { syncModifiers(to: event.flags) }
            core.sendButton(button, down: down)
            state.withLock { if down { $0.buttonsDown.insert(button) } else { $0.buttonsDown.remove(button) } }
            if !down { releaseSyntheticModifiersIfIdle() }
        case .scrollWheel:
            if event.getIntegerValueField(.scrollWheelEventIsContinuous) == 0 {
                // A mouse wheel: forward notches so the peer scrolls like a local wheel.
                let dy = event.getIntegerValueField(.scrollWheelEventDeltaAxis1)
                let dx = event.getIntegerValueField(.scrollWheelEventDeltaAxis2)
                if dx != 0 || dy != 0 { core.sendWheel(dx: Int16(clamping: dx), dy: Int16(clamping: dy)) }
            } else {
                let sy = event.getIntegerValueField(.scrollWheelEventPointDeltaAxis1)
                let sx = event.getIntegerValueField(.scrollWheelEventPointDeltaAxis2)
                if sx != 0 || sy != 0 { core.sendMotion(dx: 0, dy: 0, scrollX: sx, scrollY: sy) }
            }
        case .keyDown, .keyUp:
            let vk = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
            let down = type == .keyDown
            if let hid = KeyMap.hid(fromVK: vk) {
                if down { syncModifiers(to: event.flags) }
                core.sendKey(hid: hid, down: down)
                state.withLock { if down { $0.keysDown.insert(vk) } else { $0.keysDown.remove(vk) } }
                if !down { releaseSyntheticModifiersIfIdle() }
            } else if let usage = MediaKeys.usage(forSpecialVK: vk) {
                core.sendConsumer(usage: usage, down: type == .keyDown)
            } else {
                logUnknown(Int(vk), String(format: "key code 0x%02X", vk))
            }
        case .flagsChanged:
            let vk = UInt16(event.getIntegerValueField(.keyboardEventKeycode))
            guard let hid = KeyMap.hid(fromVK: vk) else {
                logUnknown(Int(vk), String(format: "modifier key code 0x%02X", vk))
                return
            }
            if vk == KeyMap.capsLockVK {
                // Each caps lock flag change is one press of the key.
                core.sendKey(hid: hid, down: true)
                core.sendKey(hid: hid, down: false)
            } else if let m = KeyMap.modifiers[vk] {
                let down = event.flags.rawValue & m.deviceMask != 0
                if !down, state.withLock({ $0.localHeld.contains(vk) }) { return }
                core.sendKey(hid: hid, down: down)
                state.withLock {
                    if down { $0.sentModifiers.insert(vk) } else { $0.sentModifiers.remove(vk) }
                    $0.syntheticModifiers.remove(vk)
                }
            }
        default:
            // Gestures are swallowed so they don't act on this Mac; some go to the peer.
            switch type.rawValue {
            case 29: forwardGesture(event)
            case 30: forwardSwipe(event)
            default: break
            }
        }
    }

    private static let gestureSubtype = CGEventField(rawValue: 110)!
    private static let gestureMagnification = CGEventField(rawValue: 113)!
    private static let gestureRotation = CGEventField(rawValue: 114)!
    private static let swipeMotion = CGEventField(rawValue: 123)!
    private static let swipeProgress = CGEventField(rawValue: 124)!
    private static let gesturePhase = CGEventField(rawValue: 132)!

    /// Pinch and rotation, replayed as the same gestures on the peer.
    private func forwardGesture(_ e: CGEvent) {
        let kind: Gesture.Kind
        switch e.getIntegerValueField(InputCapture.gestureSubtype) {
        case 8: kind = .magnify
        case 5: kind = .rotate
        default: return
        }
        let phase: Gesture.Phase = switch e.getIntegerValueField(InputCapture.gesturePhase) {
        case 1: .began
        case 4, 8: .ended // ended, cancelled
        default: .changed
        }
        let field = kind == .magnify ? InputCapture.gestureMagnification : InputCapture.gestureRotation
        core.sendGesture(Gesture(kind: kind, phase: phase, value: e.getDoubleValueField(field)))
    }

    /// Three-finger swipes (dock swipe events). macOS ignores synthetic ones, so the peer
    /// gets what they do once the swipe completes: switch Space, Mission Control, App Exposé.
    private func forwardSwipe(_ e: CGEvent) {
        guard e.getIntegerValueField(InputCapture.gestureSubtype) == 23 else { return }
        let phase = e.getIntegerValueField(InputCapture.gesturePhase)
        let motion = e.getIntegerValueField(InputCapture.swipeMotion)
        let progress = e.getDoubleValueField(InputCapture.swipeProgress)
        let finished = state.withLock { s -> (motion: Int64, progress: Double)? in
            if phase == 1 { (s.swipeMotion, s.swipeProgress) = (0, 0) }
            if motion != 0 { s.swipeMotion = motion }
            if progress != 0 { s.swipeProgress = progress }
            guard phase == 4 else { return nil } // ended (8: cancelled)
            return (s.swipeMotion, s.swipeProgress)
        }
        guard let finished else { return }
        Log.info(String(format: "three-finger swipe: motion %lld, progress %.2f", finished.motion, finished.progress))
        guard abs(finished.progress) > 0.1 else { return }
        switch finished.motion {
        case 1: // fingers left: the Space on the right, as here
            pressOnPeer(finished.progress < 0 ? 0x4F : 0x50, modifiers: [0xE0])
        case 2:
            if finished.progress < 0 {
                core.sendConsumer(usage: 0x29F, down: true) // Mission Control
                core.sendConsumer(usage: 0x29F, down: false)
            } else {
                pressOnPeer(0x51, modifiers: [0xE0]) // App Exposé: ⌃↓
            }
        default:
            break
        }
    }

    /// A key press (HID usages) with modifiers, on the peer.
    private func pressOnPeer(_ usage: UInt16, modifiers: [UInt16]) {
        for m in modifiers { core.sendKey(hid: m, down: true) }
        core.sendKey(hid: usage, down: true)
        core.sendKey(hid: usage, down: false)
        for m in modifiers.reversed() { core.sendKey(hid: m, down: false) }
    }
}
