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

    func testDimWeakerThanShadeFillsBlack() {
        XCTAssertEqual(ChromePalette.dimFill(dim: 0, isDark: true), 0)
        XCTAssertEqual(ChromePalette.dimFill(dim: 0.1, isDark: true), 0)
    }
}
