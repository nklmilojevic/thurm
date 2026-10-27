import Foundation

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
