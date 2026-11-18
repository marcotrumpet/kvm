import UIKit

/// USB HID keyboard usages (page 0x07), the wire format for keys.
enum HID {
    static let enter: UInt16 = 0x28
    static let escape: UInt16 = 0x29
    static let backspace: UInt16 = 0x2A
    static let tab: UInt16 = 0x2B
    static let space: UInt16 = 0x2C
    static let right: UInt16 = 0x4F
    static let left: UInt16 = 0x50
    static let down: UInt16 = 0x51
    static let up: UInt16 = 0x52
    static let control: UInt16 = 0xE0
    static let shift: UInt16 = 0xE1
    static let option: UInt16 = 0xE2
    static let command: UInt16 = 0xE3

    static func isModifier(_ usage: UInt16) -> Bool { (0xE0...0xE7).contains(usage) }

    /// The key (US positions) and whether it needs shift, for a character typed together
    /// with a modifier: a shortcut like ⌘C must be a key press, not typed text.
    static func key(for c: Character) -> (usage: UInt16, shift: Bool)? {
        guard let ascii = c.asciiValue else { return nil }
        switch ascii {
        case 0x61...0x7A: return (0x04 + UInt16(ascii - 0x61), false) // a-z
        case 0x41...0x5A: return (0x04 + UInt16(ascii - 0x41), true) // A-Z
        case 0x31...0x39: return (0x1E + UInt16(ascii - 0x31), false) // 1-9
        case 0x30: return (0x27, false)
        case 0x20: return (space, false)
        case 0x0A: return (enter, false)
        default: break
        }
        let table: [Character: (UInt16, Bool)] = [
            "-": (0x2D, false), "_": (0x2D, true), "=": (0x2E, false), "+": (0x2E, true),
            "[": (0x2F, false), "{": (0x2F, true), "]": (0x30, false), "}": (0x30, true),
            "\\": (0x31, false), "|": (0x31, true), ";": (0x33, false), ":": (0x33, true),
            "'": (0x34, false), "\"": (0x34, true), "`": (0x35, false), "~": (0x35, true),
            ",": (0x36, false), "<": (0x36, true), ".": (0x37, false), ">": (0x37, true),
            "/": (0x38, false), "?": (0x38, true), "!": (0x1E, true), "@": (0x1F, true),
            "#": (0x20, true), "$": (0x21, true), "%": (0x22, true), "^": (0x23, true),
            "&": (0x24, true), "*": (0x25, true), "(": (0x26, true), ")": (0x27, true),
        ]
        return table[c].map { (usage: $0.0, shift: $0.1) }
    }
}

/// Invisible first responder for the on-screen keyboard. Typed text is sent as text (so it
/// comes out right whatever the computer's keyboard layout); keys with the sticky modifiers
/// of the bar above the keyboard, and everything from a hardware keyboard, as key presses.
final class KeyboardInputView: UIView, UIKeyInput {
    var onText: ((String) -> Void)?
    /// A key press with modifiers (HID usages), pressed and released.
    var onKey: ((UInt16, [UInt16]) -> Void)?
    /// A hardware keyboard key went down or up. Returns false if it wasn't handled.
    var onHardwareKey: ((UIPress, Bool) -> Bool)?
    /// The on-screen keyboard was put away.
    var onResign: (() -> Void)?

    private var sticky: Set<UInt16> = []
    private var modifierButtons: [UInt16: UIButton] = [:]

    // UITextInputTraits: plain keys, no help from iOS.
    var autocorrectionType: UITextAutocorrectionType = .no
    var autocapitalizationType: UITextAutocapitalizationType = .none
    var spellCheckingType: UITextSpellCheckingType = .no
    var smartQuotesType: UITextSmartQuotesType = .no
    var smartDashesType: UITextSmartDashesType = .no
    var smartInsertDeleteType: UITextSmartInsertDeleteType = .no
    var keyboardType: UIKeyboardType = .default
    var inlinePredictionType: UITextInlinePredictionType = .no

    override var canBecomeFirstResponder: Bool { true }
    /// Three-finger swipes and pinches belong to the computer, not to undo and copy here.
    override var editingInteractionConfiguration: UIEditingInteractionConfiguration { .none }

    override func resignFirstResponder() -> Bool {
        let resigned = super.resignFirstResponder()
        if resigned { onResign?() }
        return resigned
    }

    var hasText: Bool { true } // so delete always reaches us

    func insertText(_ text: String) {
        guard !sticky.isEmpty else {
            onText?(text)
            return
        }
        for c in text {
            guard let (usage, shift) = HID.key(for: c) else { continue }
            press(usage, extra: shift ? [HID.shift] : [])
        }
    }

    func deleteBackward() {
        press(HID.backspace)
    }

    /// Sends `usage` with the sticky modifiers (then releases them, like iOS's shift).
    private func press(_ usage: UInt16, extra: [UInt16] = []) {
        onKey?(usage, Array(sticky.union(extra)).sorted())
        if !sticky.isEmpty {
            sticky.removeAll()
            updateModifierButtons()
        }
    }

    // MARK: Hardware keyboard

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !(onHardwareKey?($0, true) ?? false) }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !(onHardwareKey?($0, false) ?? false) }
        if !unhandled.isEmpty { super.pressesEnded(unhandled, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !(onHardwareKey?($0, false) ?? false) }
        if !unhandled.isEmpty { super.pressesCancelled(unhandled, with: event) }
    }

    // MARK: Key bar

    private lazy var bar: UIView = makeBar()
    override var inputAccessoryView: UIView? { bar }

    private func makeBar() -> UIView {
        let bar = UIInputView(frame: CGRect(x: 0, y: 0, width: 320, height: 48), inputViewStyle: .keyboard)
        bar.allowsSelfSizing = true
        let keys: [(String, UInt16, Bool)] = [ // title, usage, sticky modifier
            ("esc", HID.escape, false), ("tab", HID.tab, false),
            ("⌃", HID.control, true), ("⌥", HID.option, true), ("⌘", HID.command, true), ("⇧", HID.shift, true),
            ("←", HID.left, false), ("↑", HID.up, false), ("↓", HID.down, false), ("→", HID.right, false),
        ]
        let buttons: [UIView] = keys.map { title, usage, isModifier in
            var config = UIButton.Configuration.gray()
            config.title = title
            config.contentInsets = NSDirectionalEdgeInsets(top: 6, leading: 6, bottom: 6, trailing: 6)
            let button = UIButton(configuration: config, primaryAction: UIAction { [weak self] _ in
                guard let self else { return }
                if isModifier {
                    if sticky.remove(usage) == nil { sticky.insert(usage) }
                    updateModifierButtons()
                } else {
                    press(usage)
                }
            })
            if isModifier { modifierButtons[usage] = button }
            button.widthAnchor.constraint(greaterThanOrEqualToConstant: 52).isActive = true
            return button
        }
        var hideConfig = UIButton.Configuration.plain()
        hideConfig.image = UIImage(systemName: "keyboard.chevron.compact.down")
        let hide = UIButton(configuration: hideConfig, primaryAction: UIAction { [weak self] _ in
            _ = self?.resignFirstResponder()
        })
        hide.translatesAutoresizingMaskIntoConstraints = false
        let stack = UIStackView(arrangedSubviews: buttons)
        stack.axis = .horizontal
        stack.distribution = .fillEqually
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        // Keys scroll sideways when they don't fit, and spread out when they do; the hide
        // button stays put.
        let scroller = UIScrollView()
        scroller.showsHorizontalScrollIndicator = false
        scroller.translatesAutoresizingMaskIntoConstraints = false
        scroller.addSubview(stack)
        bar.addSubview(scroller)
        bar.addSubview(hide)
        NSLayoutConstraint.activate([
            stack.leadingAnchor.constraint(equalTo: scroller.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scroller.contentLayoutGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: scroller.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scroller.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scroller.frameLayoutGuide.heightAnchor),
            stack.widthAnchor.constraint(greaterThanOrEqualTo: scroller.frameLayoutGuide.widthAnchor),
            scroller.leadingAnchor.constraint(equalTo: bar.layoutMarginsGuide.leadingAnchor),
            scroller.topAnchor.constraint(equalTo: bar.topAnchor, constant: 4),
            scroller.bottomAnchor.constraint(equalTo: bar.bottomAnchor, constant: -4),
            hide.leadingAnchor.constraint(equalTo: scroller.trailingAnchor, constant: 4),
            hide.trailingAnchor.constraint(equalTo: bar.layoutMarginsGuide.trailingAnchor),
            hide.centerYAnchor.constraint(equalTo: scroller.centerYAnchor),
            bar.heightAnchor.constraint(equalToConstant: 48),
        ])
        return bar
    }

    private func updateModifierButtons() {
        for (usage, button) in modifierButtons {
            var config: UIButton.Configuration = sticky.contains(usage) ? .filled() : .gray()
            config.title = button.configuration?.title
            config.contentInsets = button.configuration?.contentInsets ?? .zero
            button.configuration = config
        }
    }
}
