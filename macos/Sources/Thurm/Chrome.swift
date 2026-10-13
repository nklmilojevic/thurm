import AppKit

/// Semantic colors of the window chrome (tab strip, sidebar, pane headers, find bar, popups,
/// toasts, split dividers). All of them derive from the terminal theme here, so the chrome
/// follows the theme in light and dark alike.
struct ChromePalette {
    let isDark: Bool
    /// The terminal background, which everything sits on.
    let canvas: NSColor
    /// A step off the canvas (darker in dark themes, like Catppuccin's mantle): the sidebar and
    /// the gutters between split panes.
    let surface: NSColor
    /// A step toward the content: popups, the find bar and toasts.
    let surfaceRaised: NSColor
    /// The thin lines between split panes.
    let divider: NSColor
    /// Hover, pressed and selected fills: content color at low alpha.
    let control: NSColor
    let controlHover: NSColor
    let controlPressed: NSColor
    let controlSelected: NSColor
    let contentPrimary: NSColor
    let contentSecondary: NSColor
    let contentTertiary: NSColor
    let contentDisabled: NSColor
    let accent: NSColor
    let accentContainer: NSColor
    let accentRing: NSColor
    let danger: NSColor
    let warning: NSColor
    let success: NSColor
    let info: NSColor
    /// Hairlines (see `ChromeStroke`).
    let stroke: NSColor
    let strokeStrong: NSColor

    init(theme: Theme) {
        let bg = colorFromRGB(theme.background)
        let fg = colorFromRGB(theme.foreground)
        func mix(_ fraction: CGFloat) -> NSColor { bg.blended(withFraction: fraction, of: fg) ?? fg }
        func ink(_ alpha: CGFloat) -> NSColor { fg.withAlphaComponent(alpha) }
        func ansi(_ index: Int, _ fallback: NSColor) -> NSColor {
            theme.palette.count > index ? colorFromRGB(theme.palette[index]) : fallback
        }
        isDark = theme.isDark
        canvas = bg
        surface = bg.shadow(withLevel: isDark ? 0.18 : 0.05) ?? bg
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
        accent = ansi(4, .systemBlue)
        accentContainer = accent.withAlphaComponent(0.18)
        accentRing = accent.withAlphaComponent(0.6)
        danger = ansi(1, .systemRed)
        warning = ansi(3, .systemOrange)
        success = ansi(2, .systemGreen)
        info = accent
        stroke = ink(0.09)
        strokeStrong = ink(0.14)
    }
}

extension AppConfig {
    var chrome: ChromePalette { ChromePalette(theme: theme) }
}

/// Corner radii of the chrome, on one scale.
enum ChromeRadius {
    /// A split pane (card) and its header.
    static let pane: CGFloat = 10
    /// Floating windows: the quick terminal and the command palette.
    static let window: CGFloat = 12
    /// Small floating surfaces: the find bar, the completion list and toasts.
    static let control: CGFloat = 8
}

/// Hairline width for panes, popups and pills. Drawn at 7-10% alpha (see `ChromePalette.stroke`),
/// brighter than the surface in dark themes, so edges read in both modes.
enum ChromeStroke {
    static let width: CGFloat = 0.75
}

/// `window.material`: what the chrome sits on when the window is translucent (`window.opacity`
/// below 1). The blur itself comes from the terminal's own `window.blur` setting or any
/// material but `theme`.
enum WindowMaterial: String {
    case theme, system, blend, modern

    /// Share of the theme canvas laid over the blur, so the chrome reads as tinted glass.
    var tint: CGFloat {
        switch self {
        case .theme, .system: return 0
        case .blend: return 0.48
        case .modern: return 0.3
        }
    }
}
