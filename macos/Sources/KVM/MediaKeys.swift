import AppKit

/// Media and Mac system keys <-> USB HID consumer usages (page 0x0C), the wire format.
///
/// macOS delivers most media keys as "system defined" events (NSEvent subtype 8) carrying
/// an NX_KEYTYPE code. A few Mac-only keys (Mission Control, Spotlight, ...) instead arrive
/// as key events with special virtual key codes.
enum MediaKeys {
    private static let nxPairs: [(nx: Int, usage: UInt16)] = [
        (0, 0xE9),  // NX_KEYTYPE_SOUND_UP
        (1, 0xEA),  // NX_KEYTYPE_SOUND_DOWN
        (2, 0x6F),  // NX_KEYTYPE_BRIGHTNESS_UP
        (3, 0x70),  // NX_KEYTYPE_BRIGHTNESS_DOWN
        (7, 0xE2),  // NX_KEYTYPE_MUTE
        (14, 0xB8), // NX_KEYTYPE_EJECT
        (16, 0xCD), // NX_KEYTYPE_PLAY
        (17, 0xB5), // NX_KEYTYPE_NEXT
        (18, 0xB6), // NX_KEYTYPE_PREVIOUS
        (19, 0xB3), // NX_KEYTYPE_FAST
        (20, 0xB4), // NX_KEYTYPE_REWIND
        (21, 0x79), // NX_KEYTYPE_ILLUMINATION_UP
        (22, 0x7A), // NX_KEYTYPE_ILLUMINATION_DOWN
        (23, 0x7C), // NX_KEYTYPE_ILLUMINATION_TOGGLE
    ]

    private static let specialKeyPairs: [(vk: UInt16, usage: UInt16)] = [
        (0xA0, 0x29F), // Mission Control -> AC Desktop Show All Windows
        (0x83, 0x2A2), // Launchpad -> AC Desktop Show All Applications
        (0xB1, 0x221), // Spotlight -> AC Search
        (0xB0, 0x0CF), // Dictation -> Voice Command
        (0xB2, 0x09B), // Do Not Disturb
    ]

    private static let nxToUsage = Dictionary(uniqueKeysWithValues: nxPairs.map { ($0.nx, $0.usage) })
    private static let usageToNx = Dictionary(uniqueKeysWithValues: nxPairs.map { ($0.usage, $0.nx) })
    private static let vkToUsage = Dictionary(uniqueKeysWithValues: specialKeyPairs.map { ($0.vk, $0.usage) })
    private static let usageToVk = Dictionary(uniqueKeysWithValues: specialKeyPairs.map { ($0.usage, $0.vk) })

    /// Mac system keys that synthetic key events can't trigger: replayed as the action.
    enum Action {
        case openApp([String]) // first that exists
        case shortcut(vk: UInt16, flags: CGEventFlags)
    }

    static func action(for usage: UInt16) -> Action? {
        switch usage {
        case 0x29F: .openApp(["/System/Applications/Mission Control.app"])
        case 0x2A2: .openApp(["/System/Applications/Launchpad.app", "/System/Applications/Apps.app"])
        case 0x221: .shortcut(vk: 0x31, flags: .maskCommand) // Spotlight: ⌘Space
        default: nil
        }
    }

    static let systemDefinedType: UInt32 = 14
    private static let auxControlButtons: Int16 = 8

    /// A media key press or release in a system-defined event, if it is one.
    static func decode(_ event: CGEvent) -> (nx: Int, down: Bool)? {
        guard let ns = NSEvent(cgEvent: event), ns.type == .systemDefined,
              ns.subtype.rawValue == auxControlButtons else { return nil }
        let nx = (ns.data1 & 0xFFFF_0000) >> 16
        let state = (ns.data1 & 0xFF00) >> 8
        return (nx, state == 0x0A)
    }

    static func usage(forNX nx: Int) -> UInt16? { nxToUsage[nx] }
    static func usage(forSpecialVK vk: UInt16) -> UInt16? { vkToUsage[vk] }

    /// Builds the event that replays `usage` on this Mac.
    static func event(usage: UInt16, down: Bool, source: CGEventSource?) -> CGEvent? {
        if let nx = usageToNx[usage] {
            let state = down ? 0x0A : 0x0B
            return NSEvent.otherEvent(
                with: .systemDefined, location: .zero,
                modifierFlags: NSEvent.ModifierFlags(rawValue: UInt(state << 8)),
                timestamp: 0, windowNumber: 0, context: nil,
                subtype: auxControlButtons, data1: (nx << 16) | (state << 8), data2: -1
            )?.cgEvent
        }
        if let vk = usageToVk[usage] {
            return CGEvent(keyboardEventSource: source, virtualKey: vk, keyDown: down)
        }
        return nil
    }
}
