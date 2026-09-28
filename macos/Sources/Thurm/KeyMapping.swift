import AppKit

/// Translation from macOS virtual key codes (Carbon `kVK_*`, hard coded to avoid depending on
/// how HIToolbox constants are imported) to Thurm key codes.
enum KeyMapping {
    // THURM_KEY_* values from thurm.h.
    static let escape: UInt32 = 1
    static let enter: UInt32 = 2
    static let tab: UInt32 = 3
    static let backspace: UInt32 = 4
    static let insert: UInt32 = 5
    static let delete: UInt32 = 6
    static let left: UInt32 = 7
    static let right: UInt32 = 8
    static let up: UInt32 = 9
    static let down: UInt32 = 10
    static let pageUp: UInt32 = 11
    static let pageDown: UInt32 = 12
    static let home: UInt32 = 13
    static let end: UInt32 = 14
    static let capsLock: UInt32 = 15
    static let scrollLock: UInt32 = 16
    static let numLock: UInt32 = 17
    static let printScreen: UInt32 = 18
    static let pause: UInt32 = 19
    static let menu: UInt32 = 20
    static let kp0: UInt32 = 21 // KP_0 ... KP_9 = 21 ... 30
    static let kpDecimal: UInt32 = 31
    static let kpDivide: UInt32 = 32
    static let kpMultiply: UInt32 = 33
    static let kpSubtract: UInt32 = 34
    static let kpAdd: UInt32 = 35
    static let kpEnter: UInt32 = 36
    static let kpEqual: UInt32 = 37
    static let leftShift: UInt32 = 38
    static let leftControl: UInt32 = 39
    static let leftAlt: UInt32 = 40
    static let leftSuper: UInt32 = 41
    static let rightShift: UInt32 = 42
    static let rightControl: UInt32 = 43
    static let rightAlt: UInt32 = 44
    static let rightSuper: UInt32 = 45
    static let volumeDown: UInt32 = 46
    static let volumeUp: UInt32 = 47
    static let volumeMute: UInt32 = 48
    static let f1: UInt32 = 100

    /// Virtual key code → named key.
    static let named: [UInt16: UInt32] = [
        0x24: enter,
        0x30: tab,
        0x33: backspace,
        0x35: escape,
        0x39: capsLock,
        0x72: insert, // "Help" on Apple keyboards sits where Insert is
        0x75: delete, // forward delete
        0x7B: left,
        0x7C: right,
        0x7D: down,
        0x7E: up,
        0x74: pageUp,
        0x79: pageDown,
        0x73: home,
        0x77: end,
        0x6E: menu, // PC keyboard context-menu key
        0x48: volumeUp,
        0x49: volumeDown,
        0x4A: volumeMute,
        // Function keys.
        0x7A: f1 + 0,  // F1
        0x78: f1 + 1,  // F2
        0x63: f1 + 2,  // F3
        0x76: f1 + 3,  // F4
        0x60: f1 + 4,  // F5
        0x61: f1 + 5,  // F6
        0x62: f1 + 6,  // F7
        0x64: f1 + 7,  // F8
        0x65: f1 + 8,  // F9
        0x6D: f1 + 9,  // F10
        0x67: f1 + 10, // F11
        0x6F: f1 + 11, // F12
        0x69: f1 + 12, // F13
        0x6B: f1 + 13, // F14
        0x71: f1 + 14, // F15
        0x6A: f1 + 15, // F16
        0x40: f1 + 16, // F17
        0x4F: f1 + 17, // F18
        0x50: f1 + 18, // F19
        0x5A: f1 + 19, // F20
        // Keypad.
        0x52: kp0 + 0,
        0x53: kp0 + 1,
        0x54: kp0 + 2,
        0x55: kp0 + 3,
        0x56: kp0 + 4,
        0x57: kp0 + 5,
        0x58: kp0 + 6,
        0x59: kp0 + 7,
        0x5B: kp0 + 8,
        0x5C: kp0 + 9,
        0x41: kpDecimal,
        0x4B: kpDivide,
        0x43: kpMultiply,
        0x4E: kpSubtract,
        0x45: kpAdd,
        0x4C: kpEnter,
        0x51: kpEqual,
        0x47: numLock, // keypad "Clear"
    ]

    /// Key codes of the keypad (their text is still sent, the daemon picks the encoding).
    static func isKeypad(_ keyCode: UInt16) -> Bool {
        switch keyCode {
        case 0x41, 0x43, 0x45, 0x47, 0x4B, 0x4C, 0x4E, 0x51, 0x52...0x59, 0x5B, 0x5C: return true
        default: return false
        }
    }

    /// Characters of the US ANSI layout by virtual key code (kitty "base layout key").
    static let usLayout: [UInt16: Character] = [
        0x00: "a", 0x01: "s", 0x02: "d", 0x03: "f", 0x04: "h", 0x05: "g", 0x06: "z", 0x07: "x",
        0x08: "c", 0x09: "v", 0x0B: "b", 0x0C: "q", 0x0D: "w", 0x0E: "e", 0x0F: "r", 0x10: "y",
        0x11: "t", 0x12: "1", 0x13: "2", 0x14: "3", 0x15: "4", 0x16: "6", 0x17: "5", 0x18: "=",
        0x19: "9", 0x1A: "7", 0x1B: "-", 0x1C: "8", 0x1D: "0", 0x1E: "]", 0x1F: "o", 0x20: "u",
        0x21: "[", 0x22: "i", 0x23: "p", 0x25: "l", 0x26: "j", 0x27: "'", 0x28: "k", 0x29: ";",
        0x2A: "\\", 0x2B: ",", 0x2C: "/", 0x2D: "n", 0x2E: "m", 0x2F: ".", 0x32: "`", 0x31: " ",
    ]

    static func baseLayoutScalar(_ keyCode: UInt16) -> UInt32 {
        guard let ch = usLayout[keyCode], let scalar = ch.unicodeScalars.first else { return 0 }
        return scalar.value
    }

    /// Modifier keys reported through `flagsChanged`: virtual key code → (Thurm key,
    /// device-dependent mask bit in `modifierFlags.rawValue` telling whether it is down).
    static let modifierKeys: [UInt16: (key: UInt32, mask: UInt)] = [
        0x38: (leftShift, 0x02),
        0x3C: (rightShift, 0x04),
        0x3B: (leftControl, 0x01),
        0x3E: (rightControl, 0x2000),
        0x3A: (leftAlt, 0x20),
        0x3D: (rightAlt, 0x40),
        0x37: (leftSuper, 0x08),
        0x36: (rightSuper, 0x10),
    ]

    /// `NX_DEVICELALTKEYMASK` / `NX_DEVICERALTKEYMASK`.
    static let leftOptionMask: UInt = 0x20
    static let rightOptionMask: UInt = 0x40

    /// True when the text is something the terminal should receive as typed text (not a
    /// control character or a function-key private-use character).
    static func isPrintable(_ text: String) -> Bool {
        guard !text.isEmpty else { return false }
        for scalar in text.unicodeScalars {
            let v = scalar.value
            if v < 0x20 || v == 0x7F { return false }
            if v >= 0xF700 && v <= 0xF8FF { return false }
        }
        return true
    }

    /// First scalar of `text`, lowercased (the unshifted key code for text keys).
    static func lowercasedScalar(_ text: String?) -> UInt32? {
        guard let text = text, !text.isEmpty else { return nil }
        let lower = text.lowercased()
        guard let scalar = lower.unicodeScalars.first else { return nil }
        let v = scalar.value
        if v < 0x20 || v == 0x7F || (v >= 0xF700 && v <= 0xF8FF) { return nil }
        return v
    }

    /// Kitty-protocol modifier bits for `flags`. Option only counts as Alt when requested.
    static func mods(_ flags: NSEvent.ModifierFlags, optionIsAlt: Bool) -> UInt8 {
        var m: UInt8 = 0
        if flags.contains(.shift) { m |= KeyMods.shift }
        if flags.contains(.control) { m |= KeyMods.ctrl }
        if flags.contains(.option) && optionIsAlt { m |= KeyMods.alt }
        if flags.contains(.command) { m |= KeyMods.superKey }
        if flags.contains(.capsLock) { m |= KeyMods.capsLock }
        return m
    }

    /// Whether Option acts as Alt for this event according to `config.window.option_as_alt`.
    static func optionActsAsAlt(_ event: NSEvent, setting: OptionAsAlt) -> Bool {
        let flags = event.modifierFlags
        guard flags.contains(.option) else { return false }
        let raw = flags.rawValue
        let leftDown = raw & leftOptionMask != 0
        let rightDown = raw & rightOptionMask != 0
        switch setting {
        case .none: return false
        case .both: return true
        case .left: return leftDown || (!leftDown && !rightDown)
        case .right: return rightDown
        }
    }
}
