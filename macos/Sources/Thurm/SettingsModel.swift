import Foundation

/// The settings window's model: `thurm_settings_json` decoded, and the pure rules the window
/// follows (search, value sources, theme and color text). Nothing here touches AppKit.

/// How a setting is shown and edited (`kind`).
enum SettingKind: String {
    case bool, int, float, text, font, color, theme, list
    case `enum`
    /// A table or a list of tables: shown as a summary, edited in the file.
    case table
}

/// Where a setting's value comes from (`source`).
enum SettingSource: String {
    case `default`, file, overlay

    /// The name shown beside a value.
    var label: String {
        switch self {
        case .default: return "Default"
        case .file: return "config.toml"
        case .overlay: return "settings.toml"
        }
    }
}

/// One option of the config (an entry of `settings` in `thurm_settings_json`).
struct SettingItem {
    let key: String
    let section: String
    let label: String
    let kind: SettingKind
    /// The value as JSON (`NSNull` when unset).
    let value: Any?
    let defaultValue: Any?
    let source: SettingSource
    let doc: String
    let options: [String]
    let min: Double?
    let max: Double?
    /// For a cell of the notification matrix: the state (row) and the field (column).
    let row: String?
    let column: String?
    /// Why settings.toml's value for this setting was left out, if it was.
    let warning: String?

    init?(json: [String: Any]) {
        guard let key = jsonString(json["key"]),
              let kind = jsonString(json["kind"]).flatMap(SettingKind.init(rawValue:))
        else { return nil }
        self.key = key
        self.kind = kind
        section = jsonString(json["section"]) ?? ""
        label = jsonString(json["label"]) ?? key
        value = json["value"]
        defaultValue = json["default"]
        source = jsonString(json["source"]).flatMap(SettingSource.init(rawValue:)) ?? .default
        doc = jsonString(json["doc"]) ?? ""
        options = (json["options"] as? [String]) ?? []
        min = jsonDouble(json["min"])
        max = jsonDouble(json["max"])
        row = jsonString(json["row"])
        column = jsonString(json["column"])
        warning = jsonString(json["warning"])
    }

    /// Whether settings.toml sets this option, so that Reset has something to remove.
    var isOverridden: Bool { source == .overlay }

    /// Whether the option may be unset (its default is null), as the font faces are.
    var isNullable: Bool { defaultValue is NSNull }

    /// The value as the text field or popup shows it.
    var text: String {
        if let s = value as? String { return s }
        if let list = value as? [String] { return list.joined(separator: ", ") }
        if let n = value as? NSNumber, kind != .bool { return n.stringValue }
        return ""
    }
}

struct SettingSection: Equatable {
    let id: String
    let label: String
}

/// The whole window: `thurm_settings_json`.
struct SettingsDocument {
    let configPath: String
    let overlayPath: String
    let configReadOnly: Bool
    let overlayExists: Bool
    let sections: [SettingSection]
    let settings: [SettingItem]
    let warnings: [String]
    /// Why `config.toml` does not load, if it does not.
    let error: String?

    init?(json text: String) {
        guard let root = JSON.decode(text) as? [String: Any] else { return nil }
        configPath = jsonString(root["config_path"]) ?? ""
        overlayPath = jsonString(root["overlay_path"]) ?? ""
        configReadOnly = jsonBool(root["config_read_only"]) ?? false
        overlayExists = jsonBool(root["overlay_exists"]) ?? false
        sections = (root["sections"] as? [[String: Any]] ?? []).compactMap { s in
            guard let id = jsonString(s["id"]) else { return nil }
            return SettingSection(id: id, label: jsonString(s["label"]) ?? id)
        }
        settings = (root["settings"] as? [[String: Any]] ?? []).compactMap(SettingItem.init(json:))
        warnings = root["warnings"] as? [String] ?? []
        error = jsonString(root["error"])
    }

    func label(ofSection id: String) -> String {
        sections.first { $0.id == id }?.label ?? id
    }

    /// The sections that have settings, in the window's order.
    var populatedSections: [SettingSection] {
        sections.filter { section in settings.contains { $0.section == section.id } }
    }

    /// The settings matching `query` (see `SettingsSearch`), grouped by section in order.
    func groups(matching query: String) -> [(section: SettingSection, items: [SettingItem])] {
        let matches = SettingsSearch.matches(settings, query: query) { self.label(ofSection: $0) }
        return populatedSections.compactMap { section in
            let items = matches.filter { $0.section == section.id }
            return items.isEmpty ? nil : (section: section, items: items)
        }
    }
}

/// Search over settings: a query matches when each of its words appears in the setting's label,
/// key, comment or section (case-insensitively).
enum SettingsSearch {
    static func matches(_ items: [SettingItem], query: String, sectionLabel: (String) -> String) -> [SettingItem] {
        let words = query.lowercased().components(separatedBy: .whitespaces).filter { !$0.isEmpty }
        guard !words.isEmpty else { return items }
        return items.filter { item in
            let haystack = [item.label, item.key, item.doc, sectionLabel(item.section)]
                .joined(separator: " ")
                .lowercased()
            return words.allSatisfy { haystack.contains($0) }
        }
    }
}

/// `colors.theme`: one theme, or `light:NAME,dark:NAME` (the same syntax as the Rust side).
enum ColorThemeSpec {
    static func parse(_ text: String, fallback: String = "thurm") -> (light: String, dark: String) {
        var light: String?
        var dark: String?
        var plain: String?
        for part in text.split(separator: ",").map({ $0.trimmingCharacters(in: .whitespaces) }) where !part.isEmpty {
            if part.hasPrefix("light:") {
                light = String(part.dropFirst("light:".count)).trimmingCharacters(in: .whitespaces)
            } else if part.hasPrefix("dark:") {
                dark = String(part.dropFirst("dark:".count)).trimmingCharacters(in: .whitespaces)
            } else {
                plain = part
            }
        }
        let single = plain ?? dark ?? light ?? fallback
        return (light ?? single, dark ?? single)
    }

    static func encode(light: String, dark: String) -> String {
        light == dark ? dark : "light:\(light),dark:\(dark)"
    }
}

/// The numbers a number control takes and gives back.
enum SettingNumber {
    /// The number typed into a field (`,` or `.` as the decimal separator); nil unless it is finite.
    static func parse(_ text: String) -> Double? {
        let normalized = text.trimmingCharacters(in: .whitespaces).replacingOccurrences(of: ",", with: ".")
        guard let value = Double(normalized), value.isFinite else { return nil }
        return value
    }

    /// The value saved for `number` in a control of `kind`: whole numbers for `int` (rounded),
    /// full precision for `float`. Nil when the number cannot be saved for that kind.
    static func value(_ number: Double, kind: SettingKind) -> Any? {
        guard number.isFinite else { return nil }
        switch kind {
        case .int:
            let whole = number.rounded()
            // Stay well inside Int, so the conversion cannot trap.
            guard abs(whole) < 9.0e18 else { return nil }
            return Int(whole)
        default:
            return number
        }
    }
}

/// `#rrggbb`, `rrggbb`, `0xrrggbb` or `#rgb`, as the config takes colors.
enum HexColor {
    static func parse(_ text: String) -> (red: Int, green: Int, blue: Int)? {
        var hex = text.trimmingCharacters(in: .whitespaces)
        if hex.hasPrefix("#") { hex.removeFirst() } else if hex.hasPrefix("0x") { hex.removeFirst(2) }
        if hex.count == 3 {
            hex = hex.map { "\($0)\($0)" }.joined()
        }
        guard hex.count == 6, let value = UInt32(hex, radix: 16) else { return nil }
        return (Int(value >> 16 & 0xFF), Int(value >> 8 & 0xFF), Int(value & 0xFF))
    }

    static func format(red: Int, green: Int, blue: Int) -> String {
        String(format: "#%02x%02x%02x", red, green, blue)
    }
}
