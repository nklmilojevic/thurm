import AppKit
import CThurm

/// `config.window.option_as_alt`.
enum OptionAsAlt: String {
    case none, left, right, both
}

/// `config.quick_terminal.position`: the screen edge the quick terminal slides in from.
enum QuickTerminalPosition: String {
    case top, bottom, left, right, center
}

/// Resolved color theme (colors are 0xRRGGBB).
struct Theme {
    var name: String
    var foreground: UInt32
    var background: UInt32
    var cursor: UInt32
    var cursorText: UInt32
    var selectionForeground: UInt32
    var selectionBackground: UInt32
    var palette: [UInt32]

    static let fallback = Theme(name: "thurm",
                                foreground: 0xD6DDE6,
                                background: 0x0E1116,
                                cursor: 0x9FB4C8,
                                cursorText: 0x0E1116,
                                selectionForeground: 0x0E1116,
                                selectionBackground: 0x9FB4C8,
                                palette: [])

    /// Relative luminance of the background below 0.5 (same rule as `Theme::is_dark`).
    var isDark: Bool { luminance(background) < 0.5 }
}

/// A built-in theme as listed by `Config::ui_json`.
extension Theme {
    /// From the JSON of `thurm_proto`'s / `thurm_config::Theme` (missing fields: the fallback's).
    init(json t: [String: Any]) {
        var theme = Theme.fallback
        theme.name = jsonString(t["name"]) ?? theme.name
        theme.foreground = UInt32(truncatingIfNeeded: jsonUInt64(t["foreground"]) ?? UInt64(theme.foreground))
        theme.background = UInt32(truncatingIfNeeded: jsonUInt64(t["background"]) ?? UInt64(theme.background))
        theme.cursor = UInt32(truncatingIfNeeded: jsonUInt64(t["cursor"]) ?? UInt64(theme.cursor))
        theme.cursorText = UInt32(truncatingIfNeeded: jsonUInt64(t["cursor_text"]) ?? UInt64(theme.cursorText))
        theme.selectionForeground = UInt32(truncatingIfNeeded:
            jsonUInt64(t["selection_foreground"]) ?? UInt64(theme.selectionForeground))
        theme.selectionBackground = UInt32(truncatingIfNeeded:
            jsonUInt64(t["selection_background"]) ?? UInt64(theme.selectionBackground))
        if let palette = t["palette"] as? [Any] {
            theme.palette = palette.map { UInt32(truncatingIfNeeded: jsonUInt64($0) ?? 0) }
        }
        self = theme
    }
}

struct ThemeChoice {
    let name: String
    let dark: Bool
    /// One of Thurm's own themes (the rest come from the bundled collection).
    let own: Bool
}

func luminance(_ rgb: UInt32) -> Double {
    func lin(_ c: UInt32) -> Double {
        let v = Double(c & 0xFF) / 255
        return v <= 0.04045 ? v / 12.92 : pow((v + 0.055) / 1.055, 2.4)
    }
    return 0.2126 * lin(rgb >> 16) + 0.7152 * lin(rgb >> 8) + 0.0722 * lin(rgb)
}

/// Whether the app is currently drawn in a dark appearance.
var systemIsDark: Bool {
    NSApp.effectiveAppearance.bestMatch(from: [.darkAqua, .aqua]) == .darkAqua
}

struct AgentPreset {
    let name: String
    let command: [String]
}

/// Converts 0xRRGGBB to an NSColor.
func colorFromRGB(_ rgb: UInt32, alpha: CGFloat = 1) -> NSColor {
    let r = CGFloat((rgb >> 16) & 0xFF) / 255
    let g = CGFloat((rgb >> 8) & 0xFF) / 255
    let b = CGFloat(rgb & 0xFF) / 255
    return NSColor(srgbRed: r, green: g, blue: b, alpha: alpha)
}

/// The UI relevant part of the user's config, parsed from `thurm_config_json()`
/// (see `Config::ui_json` in crates/thurm-config).
final class AppConfig {
    // [font]
    var fontFamily = "JetBrains Mono"
    var fontSize: CGFloat = 13
    var lineHeight: CGFloat = 1
    var letterSpacing: CGFloat = 0
    var fontFallback: [String] = ["SF Mono", "Menlo"]
    var fontFamilyBold: String?
    var fontFamilyItalic: String?
    var fontFamilyBoldItalic: String?
    var fontThicken = true
    /// 0...1 (`thicken_strength / 255`).
    var thickenStrength: CGFloat = 1
    var nerdFontSymbols = true
    /// OpenType features for CoreText (ligature switches already folded in by the Rust side).
    var fontFeatures: [String] = []

    // [window]
    var paddingX: CGFloat = 8
    var paddingY: CGFloat = 6
    var opacity: CGFloat = 1
    var optionAsAlt: OptionAsAlt = .none
    var columns = 110
    var rows = 32
    var confirmClose = true
    var unfocusedSplitDim: CGFloat = 0.25
    /// `window.tab_style == "sidebar"`: vertical tabs instead of the native tab bar.
    var sidebarTabs = false
    var quitAfterLastWindow = false
    /// `window.blur` > 0: blur what is behind a translucent window.
    var blur = 0
    var sidebarWidth: CGFloat = 240

    // [cursor]
    var cursorBlink = false
    var cursorThickness: CGFloat = 2

    // [terminal]
    var copyOnSelect = false
    var scrollMultiplier: CGFloat = 3
    var smoothScroll = true
    var tabCompletion = true

    // [session]
    var quitTerminates = false

    // [security]
    var autoSecureInput = true
    var secureInputIndicator = true
    var confirmMultilinePaste = true

    // [notifications]
    var notificationsEnabled = true
    var programNotifications = true
    var agentNeedsInput = true
    var bellBadge = true

    // [ai]
    var aiExplain = false

    // [updates]
    var updateChannel = "auto"
    var checkForUpdates = true
    var downloadUpdates = false

    // [quick_terminal]
    var quickHotkey = ""
    var quickPosition: QuickTerminalPosition = .top
    var quickSize: CGFloat = 0.4
    var quickAutohide = true
    /// "main": the screen with the menu bar, else the one with the mouse pointer.
    var quickOnMainScreen = false
    var quickAnimationDuration: TimeInterval = 0.2
    /// nil: `opacity` (the window's).
    var quickOpacity: CGFloat?

    var theme = Theme.fallback
    /// `colors.theme`, split into its light and dark halves (equal for a single theme).
    var themeLight = Theme.fallback.name
    var themeDark = Theme.fallback.name
    var followsAppearance: Bool { themeLight != themeDark }
    var themes: [ThemeChoice] = []
    var agentPresets: [AgentPreset] = []
    var configPath = ""
    var loadError: String?

    /// Loads (and on first run creates) the config through the Rust core, resolving the
    /// theme for the current system appearance.
    static func load() -> AppConfig {
        let cfg = AppConfig()
        if let raw = thurm_config_json(systemIsDark) {
            let text = String(cString: raw)
            thurm_string_free(raw)
            if let obj = JSON.decode(text) as? [String: Any] {
                cfg.apply(obj)
            } else {
                cfg.loadError = "config JSON could not be parsed"
            }
        }
        if cfg.configPath.isEmpty, let raw = thurm_config_path() {
            cfg.configPath = String(cString: raw)
            thurm_string_free(raw)
        }
        return cfg
    }

    private func apply(_ root: [String: Any]) {
        loadError = jsonString(root["error"])
        configPath = jsonString(root["config_path"]) ?? ""
        if let features = root["font_features"] as? [String] {
            fontFeatures = features
        }
        if let presets = root["agent_presets"] as? [[String: Any]] {
            agentPresets = presets.compactMap { p in
                guard let name = jsonString(p["name"]) else { return nil }
                return AgentPreset(name: name, command: (p["command"] as? [String]) ?? [])
            }
        }
        if let t = root["theme"] as? [String: Any] {
            self.theme = Theme(json: t)
        }
        if let spec = root["theme_spec"] as? [String: Any] {
            themeLight = jsonString(spec["light"]) ?? themeLight
            themeDark = jsonString(spec["dark"]) ?? themeDark
        }
        if let list = root["themes"] as? [[String: Any]] {
            themes = list.compactMap { t in
                guard let name = jsonString(t["name"]) else { return nil }
                return ThemeChoice(name: name, dark: jsonBool(t["dark"]) ?? true, own: jsonBool(t["own"]) ?? true)
            }
        }
        guard let c = root["config"] as? [String: Any] else { return }

        if let font = c["font"] as? [String: Any] {
            fontFamily = jsonString(font["family"]) ?? fontFamily
            fontSize = CGFloat(jsonDouble(font["size"]) ?? Double(fontSize))
            lineHeight = CGFloat(jsonDouble(font["line_height"]) ?? Double(lineHeight))
            letterSpacing = CGFloat(jsonDouble(font["letter_spacing"]) ?? Double(letterSpacing))
            if let fb = font["fallback"] as? [String] { fontFallback = fb }
            fontFamilyBold = jsonString(font["family_bold"]).flatMap { $0.isEmpty ? nil : $0 }
            fontFamilyItalic = jsonString(font["family_italic"]).flatMap { $0.isEmpty ? nil : $0 }
            fontFamilyBoldItalic = jsonString(font["family_bold_italic"]).flatMap { $0.isEmpty ? nil : $0 }
            fontThicken = jsonBool(font["thicken"]) ?? fontThicken
            thickenStrength = CGFloat(min(255, max(0, jsonInt(font["thicken_strength"]) ?? 255))) / 255
            nerdFontSymbols = jsonBool(font["nerd_font_symbols"]) ?? nerdFontSymbols
        }
        if let w = c["window"] as? [String: Any] {
            paddingX = CGFloat(jsonDouble(w["padding_x"]) ?? Double(paddingX))
            paddingY = CGFloat(jsonDouble(w["padding_y"]) ?? Double(paddingY))
            opacity = CGFloat(min(1, max(0.05, jsonDouble(w["opacity"]) ?? Double(opacity))))
            if let o = jsonString(w["option_as_alt"]), let v = OptionAsAlt(rawValue: o) { optionAsAlt = v }
            columns = max(10, jsonInt(w["columns"]) ?? columns)
            rows = max(3, jsonInt(w["rows"]) ?? rows)
            confirmClose = jsonBool(w["confirm_close"]) ?? confirmClose
            unfocusedSplitDim = CGFloat(min(1, max(0, jsonDouble(w["unfocused_split_dim"])
                ?? Double(unfocusedSplitDim))))
            sidebarTabs = jsonString(w["tab_style"]) == "sidebar"
            blur = max(0, jsonInt(w["blur"]) ?? blur)
            quitAfterLastWindow = jsonBool(w["quit_after_last_window"]) ?? quitAfterLastWindow
            sidebarWidth = CGFloat(min(480, max(180, jsonDouble(w["sidebar_width"]) ?? Double(sidebarWidth))))
        }
        if let cur = c["cursor"] as? [String: Any] {
            cursorBlink = jsonBool(cur["blink"]) ?? cursorBlink
            cursorThickness = CGFloat(jsonDouble(cur["thickness"]) ?? Double(cursorThickness))
        }
        if let term = c["terminal"] as? [String: Any] {
            copyOnSelect = jsonBool(term["copy_on_select"]) ?? copyOnSelect
            smoothScroll = jsonBool(term["smooth_scroll"]) ?? smoothScroll
            tabCompletion = jsonBool(term["tab_completion"]) ?? tabCompletion
            scrollMultiplier = CGFloat(max(0.1, jsonDouble(term["scroll_multiplier"]) ?? Double(scrollMultiplier)))
        }
        if let session = c["session"] as? [String: Any] {
            quitTerminates = (jsonString(session["quit"]) ?? "detach") == "terminate"
        }
        if let sec = c["security"] as? [String: Any] {
            autoSecureInput = jsonBool(sec["auto_secure_input"]) ?? autoSecureInput
            secureInputIndicator = jsonBool(sec["secure_input_indicator"]) ?? secureInputIndicator
            confirmMultilinePaste = jsonBool(sec["confirm_multiline_paste"]) ?? confirmMultilinePaste
        }
        if let n = c["notifications"] as? [String: Any] {
            notificationsEnabled = jsonBool(n["enabled"]) ?? notificationsEnabled
            programNotifications = jsonBool(n["program_notifications"]) ?? programNotifications
            agentNeedsInput = jsonBool(n["agent_needs_input"]) ?? agentNeedsInput
            bellBadge = jsonBool(n["bell_badge"]) ?? bellBadge
        }
        if let ai = c["ai"] as? [String: Any] {
            aiExplain = (jsonBool(ai["enabled"]) ?? false) && (jsonBool(ai["explain"]) ?? true)
        }
        if let u = c["updates"] as? [String: Any] {
            updateChannel = jsonString(u["channel"]) ?? updateChannel
            checkForUpdates = jsonBool(u["check_automatically"]) ?? checkForUpdates
            downloadUpdates = jsonBool(u["download_automatically"]) ?? downloadUpdates
        }
        if let q = c["quick_terminal"] as? [String: Any] {
            quickHotkey = jsonString(q["hotkey"]) ?? quickHotkey
            if let p = jsonString(q["position"]).flatMap(QuickTerminalPosition.init(rawValue:)) { quickPosition = p }
            quickSize = CGFloat(min(1, max(0.1, jsonDouble(q["size"]) ?? Double(quickSize))))
            quickAutohide = jsonBool(q["autohide"]) ?? quickAutohide
            quickOnMainScreen = jsonString(q["screen"]) == "main"
            quickAnimationDuration = max(0, jsonDouble(q["animation_duration"]) ?? quickAnimationDuration)
            quickOpacity = jsonDouble(q["opacity"]).map { CGFloat(min(1, max(0.05, $0))) }
        }
    }

    /// Color used for the thin split dividers.
    var dividerColor: NSColor {
        let fg = colorFromRGB(theme.foreground)
        let bg = colorFromRGB(theme.background)
        return bg.blended(withFraction: 0.25, of: fg) ?? fg
    }
}
