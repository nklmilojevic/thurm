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
