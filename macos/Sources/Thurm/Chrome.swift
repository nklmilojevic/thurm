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
        let c = ChromeColors(background: theme.background, foreground: theme.foreground,
                             cursor: theme.cursor, palette: theme.palette, isDark: theme.isDark,
                             p3: themeColorsAreDisplayP3)
        isDark = c.isDark
        canvas = NSColor(chrome: c.canvas)
        surface = NSColor(chrome: c.surface)
        surfaceRaised = NSColor(chrome: c.surfaceRaised)
        divider = NSColor(chrome: c.divider)
        control = NSColor(chrome: c.control)
        controlHover = NSColor(chrome: c.controlHover)
        controlPressed = NSColor(chrome: c.controlPressed)
        controlSelected = NSColor(chrome: c.controlSelected)
        contentPrimary = NSColor(chrome: c.contentPrimary)
        contentSecondary = NSColor(chrome: c.contentSecondary)
        contentTertiary = NSColor(chrome: c.contentTertiary)
        contentDisabled = NSColor(chrome: c.contentDisabled)
        accent = c.accent.map(NSColor.init(chrome:)) ?? .systemBlue
        accentContainer = accent.withAlphaComponent(0.18)
        accentRing = accent.withAlphaComponent(0.6)
        danger = c.danger.map(NSColor.init(chrome:)) ?? .systemRed
        warning = c.warning.map(NSColor.init(chrome:)) ?? .systemOrange
        success = c.success.map(NSColor.init(chrome:)) ?? .systemGreen
        info = c.info.map(NSColor.init(chrome:)) ?? .systemBlue
        stroke = NSColor(chrome: c.stroke)
        strokeStrong = NSColor(chrome: c.strokeStrong)
    }

    /// How far `surface` is shaded toward black from the canvas.
    static func surfaceShade(isDark: Bool) -> CGFloat { CGFloat(ChromeColors.surfaceShade(isDark: isDark)) }

    /// The unfocused-split dim lays the background scaled by this over the pane, so at the full
    /// `dim` the pane's background lands on `surface` and the pane sinks into the gutters, in
    /// any theme. A dim weaker than the shade gets a black fill and stops short of `surface`.
    static func dimFill(dim: CGFloat, isDark: Bool) -> Float {
        guard dim > 0 else { return 0 }
        return Float(max(0, 1 - surfaceShade(isDark: isDark) / dim))
    }

    /// The background of a pane drawn at `drawn` dim (on its way to `dim`), as a scale of the
    /// undimmed background: what the dim cover leaves of it.
    static func dimmedBackgroundScale(drawn: Float, dim: CGFloat, isDark: Bool) -> CGFloat {
        CGFloat(1 - drawn * (1 - dimFill(dim: dim, isDark: isDark)))
    }
}

extension AppConfig {
    var chrome: ChromePalette { ChromePalette(theme: theme) }
}

extension NSColor {
    /// A chrome color, in the space the theme's colors are read in (see `colorFromRGB`).
    convenience init(chrome c: ChromeRGBA) {
        if themeColorsAreDisplayP3 {
            self.init(displayP3Red: c.red, green: c.green, blue: c.blue, alpha: c.alpha)
        } else {
            self.init(srgbRed: c.red, green: c.green, blue: c.blue, alpha: c.alpha)
        }
    }
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
