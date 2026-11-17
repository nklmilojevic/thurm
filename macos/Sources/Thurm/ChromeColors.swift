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

    /// Each channel through `f`, kept within 0...1.
    fileprivate func mapChannels(_ f: (Double) -> Double) -> ChromeRGBA {
        func clamp(_ v: Double) -> Double { min(1, max(0, v)) }
        return ChromeRGBA(red: clamp(f(red)), green: clamp(f(green)), blue: clamp(f(blue)), alpha: alpha)
    }

    /// The color of an Oklab value, in sRGB or Display P3 components; out-of-gamut channels are
    /// clamped.
    fileprivate init(oklab lab: SIMD3<Double>, p3: Bool) {
        let (L, a, b) = (lab.x, lab.y, lab.z)
        let l = pow(L + 0.3963377774 * a + 0.2158037573 * b, 3)
        let m = pow(L - 0.1055613458 * a - 0.0638541728 * b, 3)
        let s = pow(L - 0.0894841775 * a - 1.2914855480 * b, 3)
        var r = 4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s
        var g = -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s
        var bl = -0.0041960863 * l - 0.7034186147 * m + 1.7076147010 * s
        if p3 {
            (r, g, bl) = (0.8224621 * r + 0.177538 * g,
                          0.0331941 * r + 0.9668058 * g,
                          0.0170827 * r + 0.0723974 * g + 0.9105199 * bl)
        }
        func encode(_ v: Double) -> Double {
            let v = min(1, max(0, v))
            return v <= 0.0031308 ? 12.92 * v : 1.055 * pow(v, 1 / 2.4) - 0.055
        }
        self.init(red: encode(r), green: encode(g), blue: encode(bl))
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
    /// The selected tab's pill: the background raised a step in lightness, tinted like it. Dark
    /// themes lift it further while the window is key.
    let selected: ChromeRGBA
    /// The pill while the window is not key (the same as `selected` in light themes).
    let selectedInactive: ChromeRGBA
    /// The pill's inner outline: a lighter step of the fill in dark themes, the text color at
    /// low alpha in light ones. Dark themes draw it only while the window is key.
    let selectedStroke: ChromeRGBA
    /// The pill's soft drop shadow, drawn at `selectedShadowOpacity`; light themes only.
    let selectedShadow: ChromeRGBA?
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
        let raised = Self.raised(background, isDark: isDark, p3: p3)
        if isDark {
            // White at 50% in the overlay blend, then white at 10% added for the outline.
            let lifted = raised.mapChannels { $0 < 0.5 ? 1.5 * $0 : ($0 + 1) / 2 }
            selected = lifted
            selectedInactive = raised
            selectedStroke = lifted.mapChannels { $0 + 0.1 }
            selectedShadow = nil
        } else {
            selected = raised
            selectedInactive = raised
            selectedStroke = ink(0.1)
            selectedShadow = fg
        }
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

    /// The selected pill's light-theme shadow: opacity, blur radius and downward offset in points.
    static let selectedShadowOpacity = 0.15
    static let selectedShadowBlur = 3.5
    static let selectedShadowOffset = 1.5
    /// The width of the pill's inner outline, in points.
    static let selectedStrokeWidth = 0.8

    /// The background a step lighter in Oklab, hue kept and chroma eased off toward the ends of
    /// the lightness range.
    static func raised(_ background: UInt32, isDark: Bool, p3: Bool) -> ChromeRGBA {
        let lab = ColorAdjuster.oklab(background, p3: p3)
        let lightness = lab.x + (isDark ? 0.07 : 0.035)
        let chroma = max(1 - 2 * abs(lightness - 0.5) * 0.16, 0.72)
        return ChromeRGBA(oklab: SIMD3(lightness, lab.y * chroma, lab.z * chroma), p3: p3)
    }

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
