import XCTest
@testable import Thurm

final class ChromeTests: XCTestCase {
    /// At the full dim, the dimmed pane's background is the sidebar and gutter color.
    func testFullDimLandsOnSurface() {
        for dark in [true, false] {
            let shade = Float(ChromePalette.surfaceShade(isDark: dark))
            for dim: Float in [0.25, 0.5, 1] {
                let fill = ChromePalette.dimFill(dim: CGFloat(dim), isDark: dark)
                let dimmed = (1 - dim) + fill * dim
                XCTAssertEqual(dimmed, 1 - shade, accuracy: 1e-5, "dark: \(dark), dim: \(dim)")
            }
        }
    }

    /// An unfocused header strip follows the body: undimmed at 0, the gutter color at full dim.
    func testDimmedHeaderFollowsTheBody() {
        for dark in [true, false] {
            let shade = ChromePalette.surfaceShade(isDark: dark)
            XCTAssertEqual(ChromePalette.dimmedBackgroundScale(drawn: 0, dim: 0.25, isDark: dark), 1)
            XCTAssertEqual(ChromePalette.dimmedBackgroundScale(drawn: 0.25, dim: 0.25, isDark: dark), 1 - shade,
                           accuracy: 1e-5)
        }
    }

    /// Thurm's default theme: the cursor is a grey, so the accent stays the theme's blue, and the
    /// surfaces mix the background toward the text as before.
    func testDefaultThemeKeepsBlueAccent() {
        var t = Theme.fallback
        t.palette = [0x1b2029, 0xe06c75, 0x98c379, 0xe5c07b, 0x61afef, 0xc678dd, 0x56b6c2, 0xc8d0da]
        let c = ChromeColors(background: t.background, foreground: t.foreground, cursor: t.cursor,
                             palette: t.palette, isDark: true)
        XCTAssertEqual(c.accent, ChromeRGBA(t.palette[4]))
        XCTAssertEqual(c.info, ChromeRGBA(t.palette[4]))
        XCTAssertEqual(c.canvas, ChromeRGBA(t.background))
        let raised = ChromeRGBA(t.background).mixed(0.05, with: ChromeRGBA(t.foreground))
        XCTAssertEqual(c.surfaceRaised, raised)
        XCTAssertEqual(c.surface, ChromeRGBA(t.background).mixed(0.18, with: ChromeRGBA(0)))
        XCTAssertEqual(c.contentSecondary, ChromeRGBA(t.foreground, alpha: 0.6))
    }

    /// A cursor in a color of its own (Thurm Light's steel blue) is the accent;
    /// agents at work keep the theme's blue.
    func testDistinctCursorIsTheAccent() {
        let palette: [UInt32] = [0x24292f, 0xcf222e, 0x116329, 0x7d4e00, 0x0969da, 0x8250df, 0x1b7c83, 0x6e7781]
        let c = ChromeColors(background: 0xfafbfc, foreground: 0x24292f, cursor: 0x3b5b7a,
                             palette: palette, isDark: false)
        XCTAssertEqual(c.accent, ChromeRGBA(0x3b5b7a))
        XCTAssertEqual(c.info, ChromeRGBA(0x0969da))
    }

    /// A cursor that is the text color, a grey, or too faint on the background isn't an accent.
    func testPlainCursorFallsBackToBlue() {
        let palette: [UInt32] = [0, 0xff5555, 0x50fa7b, 0xf1fa8c, 0xbd93f9]
        for (cursor, why) in [(0xf8f8f2 as UInt32, "the text color"), (0x9a9a9a, "grey"),
                              (0x3a2a40, "faint on the background")] {
            let accent = ChromeColors.accentRGB(background: 0x282a36, foreground: 0xf8f8f2, cursor: cursor,
                                                palette: palette, p3: false)
            XCTAssertEqual(accent, 0xbd93f9, why)
        }
        XCTAssertNil(ChromeColors.accentRGB(background: 0, foreground: 0xffffff, cursor: 0xffffff,
                                            palette: [], p3: false), "no palette: the system's blue")
    }

    func testDimWeakerThanShadeFillsBlack() {
        XCTAssertEqual(ChromePalette.dimFill(dim: 0, isDark: true), 0)
        XCTAssertEqual(ChromePalette.dimFill(dim: 0.1, isDark: true), 0)
    }

    /// Cards overlapping during a zoom stay cut out where they overlap, and the rest of the pane
    /// area stays gutter.
    func testOverlappingCardsStayCutOutOfTheGutter() {
        let area = CGRect(x: 0, y: 0, width: 400, height: 300)
        let radii: [CGFloat] = [8, 8, 8, 8]
        let gutter = CardPath.gutter(area, cards: [(CGRect(x: 10, y: 10, width: 200, height: 200), radii),
                                                   (CGRect(x: 110, y: 60, width: 200, height: 200), radii)])
        XCTAssertFalse(gutter.contains(CGPoint(x: 150, y: 100)), "overlap")
        XCTAssertFalse(gutter.contains(CGPoint(x: 40, y: 40)), "first card only")
        XCTAssertFalse(gutter.contains(CGPoint(x: 280, y: 240)), "second card only")
        XCTAssertTrue(gutter.contains(CGPoint(x: 360, y: 20)), "outside both cards")
        XCTAssertTrue(gutter.contains(CGPoint(x: 11, y: 11)), "a card's rounded corner")
    }

    private func assertRGB(_ c: ChromeRGBA, _ rgb: UInt32, _ what: String, file: StaticString = #filePath,
                           line: UInt = #line) {
        let want = ChromeRGBA(rgb)
        for (got, expected, channel) in [(c.red, want.red, "red"), (c.green, want.green, "green"),
                                         (c.blue, want.blue, "blue")] {
            XCTAssertEqual(got * 255, expected * 255, accuracy: 1, "\(what) \(channel)", file: file, line: line)
        }
    }

    /// Catppuccin Mocha: the background raised a step and kept in its hue, then lifted while the
    /// window is key, with a lighter outline and no shadow.
    func testSelectedPillOnDarkTheme() {
        let c = ChromeColors(background: 0x1e1e2e, foreground: 0xcdd6f4, cursor: 0xf5e0dc, palette: [],
                             isDark: true)
        assertRGB(c.selectedInactive, 0x2f2f3f, "inactive")
        assertRGB(c.selected, 0x47475f, "selected")
        for color in [c.selectedInactive, c.selected, c.selectedStroke] {
            XCTAssertEqual(color.red, color.green, accuracy: 0.5 / 255, "hue kept")
            XCTAssertEqual(color.alpha, 1)
        }
        XCTAssertEqual(c.selectedStroke.red, c.selected.red + 0.1, accuracy: 1e-9)
        XCTAssertEqual(c.selectedStroke.blue, c.selected.blue + 0.1, accuracy: 1e-9)
        XCTAssertNil(c.selectedShadow)
    }

    /// A light theme: the raised background without the dark themes' lift, the same while the
    /// window is not key, outlined and shadowed in the text color.
    func testSelectedPillOnLightTheme() {
        let c = ChromeColors(background: 0xeff1f5, foreground: 0x4c4f69, cursor: 0xdc8a78, palette: [],
                             isDark: false)
        let raised = ChromeColors.raised(0xeff1f5, isDark: false, p3: false)
        XCTAssertEqual(c.selected, raised)
        XCTAssertEqual(c.selectedInactive, raised)
        XCTAssertGreaterThan(c.selected.red + c.selected.green + c.selected.blue,
                             c.surface.red + c.surface.green + c.surface.blue)
        let dark = ChromeColors(background: 0xeff1f5, foreground: 0x4c4f69, cursor: 0xdc8a78, palette: [],
                                isDark: true)
        XCTAssertNotEqual(c.selected, dark.selected)
        XCTAssertEqual(c.selectedStroke, ChromeRGBA(0x4c4f69, alpha: 0.1))
        XCTAssertEqual(c.selectedShadow, ChromeRGBA(0x4c4f69))
    }

    /// Components read as Display P3 come back as P3: a near-grey background lands where the
    /// sRGB reading does.
    func testSelectedPillInDisplayP3() {
        for bg: UInt32 in [0x1e1e22, 0x2a2a2a, 0xf0f0ee] {
            for dark in [true, false] {
                let p3 = ChromeColors(background: bg, foreground: 0xcccccc, cursor: 0xcccccc, palette: [],
                                      isDark: dark, p3: true)
                let srgb = ChromeColors(background: bg, foreground: 0xcccccc, cursor: 0xcccccc, palette: [],
                                        isDark: dark)
                let want = UInt32((srgb.selected.red * 255).rounded()) << 16
                    | UInt32((srgb.selected.green * 255).rounded()) << 8 | UInt32((srgb.selected.blue * 255).rounded())
                assertRGB(p3.selected, want, String(format: "%06x dark: %d", bg, dark ? 1 : 0))
            }
        }
    }
}
