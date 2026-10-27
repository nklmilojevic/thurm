import Foundation
import simd

/// A chrome color as components in the theme's color space (sRGB, or Display P3 when the theme's
/// colors are read as P3), each 0...1. The Mac makes `NSColor`s of them, the iPhone SwiftUI
/// colors, so both apps draw the same chrome for a theme.
struct ChromeRGBA: Equatable {
    var red: Double
    var green: Double
    var blue: Double
    var alpha: Double

    init(red: Double, green: Double, blue: Double, alpha: Double = 1) {
        (self.red, self.green, self.blue, self.alpha) = (red, green, blue, alpha)
    }

    /// 0xRRGGBB.
    init(_ rgb: UInt32, alpha: Double = 1) {
        self.init(red: Double((rgb >> 16) & 0xFF) / 255,
                  green: Double((rgb >> 8) & 0xFF) / 255,
                  blue: Double(rgb & 0xFF) / 255,
                  alpha: alpha)
    }

    /// `fraction` of the way to `other`.
    func mixed(_ fraction: Double, with other: ChromeRGBA) -> ChromeRGBA {
        func lerp(_ a: Double, _ b: Double) -> Double { a + (b - a) * fraction }
        return ChromeRGBA(red: lerp(red, other.red), green: lerp(green, other.green),
                          blue: lerp(blue, other.blue), alpha: lerp(alpha, other.alpha))
    }

    func withAlpha(_ alpha: Double) -> ChromeRGBA {
        ChromeRGBA(red: red, green: green, blue: blue, alpha: alpha)
    }
}

/// Semantic colors of the chrome, derived from the terminal theme so the chrome follows it in
/// light and dark alike. Colors the theme's 16 don't have (a short palette) are nil: the app
/// uses the system's.
struct ChromeColors: Equatable {
    let isDark: Bool
    /// The terminal background, which everything sits on.
    let canvas: ChromeRGBA
    /// A step off the canvas (darker in dark themes, like Catppuccin's mantle): the sidebar and
    /// the gutters between split panes.
    let surface: ChromeRGBA
    /// A step toward the content: popups, the find bar and toasts.
    let surfaceRaised: ChromeRGBA
    /// The thin lines between split panes.
    let divider: ChromeRGBA
    /// Hover, pressed and selected fills: content color at low alpha.
    let control: ChromeRGBA
    let controlHover: ChromeRGBA
    let controlPressed: ChromeRGBA
    let controlSelected: ChromeRGBA
    let contentPrimary: ChromeRGBA
    let contentSecondary: ChromeRGBA
    let contentTertiary: ChromeRGBA
    let contentDisabled: ChromeRGBA
    /// Focus rings and selected controls: the theme's cursor color when it is a color of its own
    /// (see `accentRGB`), else the theme's blue.
    let accent: ChromeRGBA?
    let danger: ChromeRGBA?
    let warning: ChromeRGBA?
    let success: ChromeRGBA?
    /// Agents at work and progress: the theme's blue, whatever the accent.
    let info: ChromeRGBA?
    /// Hairlines.
    let stroke: ChromeRGBA
    let strokeStrong: ChromeRGBA

    init(background: UInt32, foreground: UInt32, cursor: UInt32, palette: [UInt32], isDark: Bool,
         p3: Bool = false) {
        let bg = ChromeRGBA(background)
        let fg = ChromeRGBA(foreground)
        func mix(_ fraction: Double) -> ChromeRGBA { bg.mixed(fraction, with: fg) }
        func ink(_ alpha: Double) -> ChromeRGBA { fg.withAlpha(alpha) }
        func ansi(_ index: Int) -> ChromeRGBA? { palette.count > index ? ChromeRGBA(palette[index]) : nil }
        self.isDark = isDark
        canvas = bg
        surface = bg.mixed(Self.surfaceShade(isDark: isDark), with: ChromeRGBA(0))
        surfaceRaised = mix(0.05)
        divider = mix(0.25)
        control = ink(0.035)
        controlHover = ink(0.07)
        controlPressed = ink(0.11)
        controlSelected = ink(0.07)
        contentPrimary = fg
        contentSecondary = ink(0.6)
        contentTertiary = ink(0.42)
        contentDisabled = ink(0.3)
        accent = Self.accentRGB(background: background, foreground: foreground, cursor: cursor,
                                palette: palette, p3: p3).map { ChromeRGBA($0) }
        danger = ansi(1)
        warning = ansi(3)
        success = ansi(2)
        info = ansi(4)
        stroke = ink(0.09)
        strokeStrong = ink(0.14)
    }

    /// How far `surface` is shaded toward black from the canvas.
    static func surfaceShade(isDark: Bool) -> Double { isDark ? 0.18 : 0.05 }

    /// The accent: the cursor color when it reads as a color of its own (not grey, not the text
    /// color, and with 3:1 contrast on the background, as a focus ring needs), else palette blue.
    /// In most classic themes the cursor is the text color, so they keep blue.
    static func accentRGB(background: UInt32, foreground: UInt32, cursor: UInt32,
                          palette: [UInt32], p3: Bool) -> UInt32? {
        let lab = ColorAdjuster.oklab(cursor, p3: p3)
        let chroma = (lab.y * lab.y + lab.z * lab.z).squareRoot()
        let fromText = simd_distance(lab, ColorAdjuster.oklab(foreground, p3: p3))
        let lc = ColorAdjuster.luminance(cursor, p3: p3)
        let lb = ColorAdjuster.luminance(background, p3: p3)
        let contrast = (max(lc, lb) + 0.05) / (min(lc, lb) + 0.05)
        if chroma >= 0.05, fromText >= 0.08, contrast >= 3 { return cursor }
        return palette.count > 4 ? palette[4] : nil
    }
}
