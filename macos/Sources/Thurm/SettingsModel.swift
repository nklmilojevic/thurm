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
    /// What config.toml itself sets (`nil` when it doesn't), whatever settings.toml says.
    let fileValue: Any?
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
        fileValue = json["file_value"] is NSNull ? nil : json["file_value"]
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
        if let n = value as? NSNumber, kind == .float { return SettingNumber.format(n.doubleValue) }
        if let n = value as? NSNumber, kind != .bool { return n.stringValue }
        return ""
    }
}

struct SettingSection: Equatable, Identifiable {
    let id: String
    let label: String
}

/// A line above the form: what the config file's state means for the edits.
struct SettingsNotice: Equatable {
    let text: String
    let isError: Bool
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

    /// The file to edit a value the window can't: where it is set now. A list or table in
    /// settings.toml replaces config.toml's whole, so one config.toml sets is edited there.
    func editPath(for item: SettingItem) -> String {
        item.source == .file ? configPath : overlayPath
    }

    /// What the file state means for the edits, in one line; nil when there is nothing to say.
    var notice: SettingsNotice? {
        var parts: [String] = []
        if let error {
            parts.append("config.toml could not be loaded (\(error)). Thurm uses the defaults until it is fixed.")
        } else if configReadOnly {
            parts.append("config.toml is read-only (another tool manages it). Changes are saved to settings.toml.")
        }
        if let first = warnings.first {
            let more = warnings.count > 1 ? " (+\(warnings.count - 1) more)" : ""
            parts.append("Ignored in settings.toml: \(first)\(more).")
        }
        guard !parts.isEmpty else { return nil }
        return SettingsNotice(text: parts.joined(separator: " "), isError: error != nil)
    }
}

/// The text a setting shows in a form.
enum SettingText {
    static let summaryLimit = 90

    /// The first sentence of a setting's doc, cut at a word boundary past `limit` characters.
    static func summary(_ text: String, limit: Int = summaryLimit) -> String {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        var sentence = trimmed
        var index = trimmed.startIndex
        while index < trimmed.endIndex {
            let next = trimmed.index(after: index)
            if ".!?".contains(trimmed[index]), next == trimmed.endIndex || trimmed[next].isWhitespace {
                sentence = String(trimmed[...index])
                break
            }
            index = next
        }
        guard sentence.count > limit else { return sentence }
        let cut = sentence.prefix(limit)
        guard let space = cut.lastIndex(of: " ") else { return cut + "\u{2026}" }
        return String(cut[..<space]) + "\u{2026}"
    }

    /// An enum value as a label: `copy-paste` reads as "Copy paste".
    static func optionTitle(_ value: String) -> String {
        let spaced = value.replacingOccurrences(of: "-", with: " ").replacingOccurrences(of: "_", with: " ")
        return spaced.prefix(1).uppercased() + spaced.dropFirst()
    }
}

/// The subgroups of a section's rows: a heading for each table of keys that shares a section
/// with others (`colors.*`, `cursor.*` in Appearance), and the notification matrix.
struct SettingGroup {
    let heading: String?
    let items: [SettingItem]

    var isMatrix: Bool { items.contains { $0.row != nil } }
}

enum SettingGroups {
    private static let tables = ["colors": "Colors", "cursor": "Cursor", "tailscale": "Tailscale"]
    private static let headingOrder = ["Theme", "Colors", "Cursor"]

    static func heading(for item: SettingItem) -> String? {
        if item.key == "colors.theme" { return "Theme" }
        let table = item.key.split(separator: ".").first.map(String.init) ?? ""
        return tables[table]
    }

    /// The groups of `items` in the order they show: plain rows first, by heading, then the matrix.
    static func split(_ items: [SettingItem]) -> [SettingGroup] {
        var headings: [String?] = []
        var buckets: [String: [SettingItem]] = [:]
        for item in items where item.row == nil {
            let heading = heading(for: item)
            let key = heading ?? ""
            if buckets[key] == nil { headings.append(heading) }
            buckets[key, default: []].append(item)
        }
        func rank(_ heading: String?) -> Int {
            guard let heading else { return 0 }
            return headingOrder.firstIndex(of: heading).map { $0 + 1 } ?? headingOrder.count + 1
        }
        let ordered = headings.enumerated()
            .sorted { (rank($0.element), $0.offset) < (rank($1.element), $1.offset) }
            .map(\.element)
        var groups = ordered.map { SettingGroup(heading: $0, items: buckets[$0 ?? ""] ?? []) }
        let matrix = items.filter { $0.row != nil }
        if !matrix.isEmpty { groups.append(SettingGroup(heading: "States", items: matrix)) }
        return groups
    }
}

/// The SF Symbol of each section's tile in the sidebar.
enum SettingsSectionIcons {
    private static let symbols = [
        "appearance": "paintpalette",
        "font": "textformat",
        "window": "macwindow",
        "terminal": "terminal",
        "session": "clock.arrow.circlepath",
        "security": "lock.shield",
        "notifications": "bell.badge",
        "agents": "sparkles",
        "ai": "apple.intelligence",
        "programs": "app.badge",
        "updates": "arrow.down.circle",
        "quick_terminal": "rectangle.topthird.inset.filled",
        "keys": "keyboard",
        "remotes": "network",
    ]

    static func symbol(for section: String) -> String {
        symbols[section] ?? "gearshape"
    }
}

/// The sounds a notification can play: `default`, `none`, and the sound files of the system and
/// of the user (`~/Library/Sounds`), by name.
enum SystemSounds {
    /// The file types `NSSound(named:)` plays from the sound folders.
    static let extensions: Set<String> = ["aiff", "aif", "aifc", "caf", "wav", "mp3", "m4a", "snd"]

    static func names(in directories: [String] = ["/System/Library/Sounds",
                                                  FileManager.default.homeDirectoryForCurrentUser
                                                      .appendingPathComponent("Library/Sounds").path]) -> [String] {
        var names = Set<String>()
        for directory in directories {
            let urls = (try? FileManager.default.contentsOfDirectory(
                at: URL(fileURLWithPath: directory), includingPropertiesForKeys: [.isRegularFileKey],
                options: .skipsHiddenFiles)) ?? []
            for url in urls where extensions.contains(url.pathExtension.lowercased()) {
                guard (try? url.resourceValues(forKeys: [.isRegularFileKey]).isRegularFile) == true else { continue }
                names.insert(url.deletingPathExtension().lastPathComponent)
            }
        }
        return names.sorted()
    }
}

/// A list setting as one text field: its elements comma-separated. A list that wouldn't come back
/// unchanged through that (an element with a comma, surrounding spaces, or nothing in it) is edited
/// in the file instead.
enum SettingList {
    static func elements(_ value: Any?) -> [String] {
        (value as? [Any])?.compactMap { $0 as? String } ?? []
    }

    static func isEditableAsText(_ value: Any?) -> Bool {
        let list = elements(value)
        return parse(list.joined(separator: ", ")) == list
    }

    static func parse(_ text: String) -> [String] {
        text.split(separator: ",").map { $0.trimmingCharacters(in: .whitespaces) }.filter { !$0.isEmpty }
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

    /// The decimals a `float` setting keeps.
    static let decimals = 2

    /// The value saved for `number` in a control of `kind`: whole numbers for `int`, two
    /// decimals for `float` (rounded). Nil when the number cannot be saved for that kind.
    static func value(_ number: Double, kind: SettingKind) -> Any? {
        guard number.isFinite else { return nil }
        switch kind {
        case .int:
            let whole = number.rounded()
            // Stay well inside Int, so the conversion cannot trap.
            guard abs(whole) < 9.0e18 else { return nil }
            return Int(whole)
        case .float:
            return rounded(number)
        default:
            return number
        }
    }

    /// `number` to `decimals` places.
    static func rounded(_ number: Double) -> Double {
        let scale = pow(10, Double(decimals))
        return (number * scale).rounded() / scale
    }

    /// `number` as a control shows it: at most `decimals` places, no trailing zeros.
    static func format(_ number: Double) -> String {
        var text = String(format: "%.\(decimals)f", rounded(number))
        if text.contains(".") {
            while text.hasSuffix("0") { text.removeLast() }
            if text.hasSuffix(".") { text.removeLast() }
        }
        return text == "-0" ? "0" : text
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

/// A `[[remote]]` host, as the `remote` setting's value lists it.
struct RemoteHostEntry: Equatable, Identifiable {
    let name: String
    /// The ssh target.
    let target: String
    let enabled: Bool
    /// `ask`, `always` or `never`.
    let clipboardRead: String
    /// Its daemon's tailnet address, when it is reached over the tailnet instead of ssh.
    let tailnet: String?

    var id: String { name }

    static let clipboardChoices = ["ask", "always", "never"]

    init(name: String, target: String, enabled: Bool = true, clipboardRead: String = "ask", tailnet: String? = nil) {
        self.name = name
        self.target = target
        self.enabled = enabled
        self.clipboardRead = clipboardRead
        self.tailnet = tailnet
    }

    init?(json: [String: Any]) {
        guard let name = jsonString(json["name"]) else { return nil }
        self.init(name: name, target: jsonString(json["host"]) ?? "", enabled: jsonBool(json["enabled"]) ?? true,
                  clipboardRead: jsonString(json["clipboard_read"]) ?? "ask", tailnet: jsonString(json["tailnet"]))
    }

    /// The hosts of the `remote` setting's value (none when it is unset).
    static func entries(_ value: Any?) -> [RemoteHostEntry] {
        (value as? [[String: Any]] ?? []).compactMap(RemoteHostEntry.init(json:))
    }
}
