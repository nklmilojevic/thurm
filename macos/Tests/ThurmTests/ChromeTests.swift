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
}
