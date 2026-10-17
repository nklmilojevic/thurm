import XCTest
@testable import Thurm

final class ColorAdjustmentTests: XCTestCase {
    private func ratio(_ a: UInt32, _ b: UInt32, p3: Bool = false) -> Double {
        let (la, lb) = (ColorAdjuster.luminance(a, p3: p3), ColorAdjuster.luminance(b, p3: p3))
        return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)
    }

    func testReadableTextIsLeftAlone() {
        XCTAssertEqual(ColorAdjuster.contrasted(0xCDD6F4, over: 0x1E1E2E, minimum: 1.5, p3: false), 0xCDD6F4)
        XCTAssertEqual(ColorAdjuster.contrasted(0x1E1E2E, over: 0x1E1E2E, minimum: 1, p3: false), 0x1E1E2E)
    }

    /// Unreadable text moves just far enough to reach the ratio, toward the nearer end.
    func testUnreadableTextReachesTheRatio() {
        for p3 in [false, true] {
            for (fg, bg) in [(0x202030, 0x1E1E2E), (0x45475A, 0x313244), (0xEEEEEE, 0xFFFFFF)] as [(UInt32, UInt32)] {
                let out = ColorAdjuster.contrasted(fg, over: bg, minimum: 3, p3: p3)
                XCTAssertNotEqual(out, fg)
                XCTAssertGreaterThanOrEqual(ratio(out, bg, p3: p3), 3, String(format: "%06x on %06x", fg, bg))
                XCTAssertNotEqual(out, 0xFFFFFF)
                XCTAssertNotEqual(out, 0)
            }
        }
        // Light text on white gets darker, dark text on near-black lighter.
        XCTAssertLessThan(ColorAdjuster.luminance(ColorAdjuster.contrasted(0xEEEEEE, over: 0xFFFFFF, minimum: 3, p3: false),
                                                  p3: false), ColorAdjuster.luminance(0xEEEEEE, p3: false))
        XCTAssertGreaterThan(ColorAdjuster.luminance(ColorAdjuster.contrasted(0x202030, over: 0x1E1E2E, minimum: 3,
                                                                             p3: false), p3: false),
                             ColorAdjuster.luminance(0x202030, p3: false))
    }

    /// A ratio neither black nor white reaches over a mid gray: the better of the two.
    func testUnreachableRatioFallsBackToBlackOrWhite() {
        XCTAssertEqual(ColorAdjuster.contrasted(0x777777, over: 0x777777, minimum: 21, p3: false), 0)
        XCTAssertEqual(ColorAdjuster.contrasted(0x333333, over: 0x333333, minimum: 21, p3: false), 0xFFFFFF)
    }

    func testOklabOfWhiteAndBlack() {
        let white = ColorAdjuster.oklab(0xFFFFFF, p3: false)
        XCTAssertEqual(white.x, 1, accuracy: 1e-4)
        XCTAssertEqual(white.y, 0, accuracy: 1e-4)
        XCTAssertEqual(white.z, 0, accuracy: 1e-4)
        XCTAssertEqual(ColorAdjuster.oklab(0, p3: true).x, 0, accuracy: 1e-9)
        // P3 red is outside sRGB: more chroma than sRGB red.
        let srgb = ColorAdjuster.oklab(0xFF0000, p3: false)
        let p3 = ColorAdjuster.oklab(0xFF0000, p3: true)
        XCTAssertGreaterThan((p3.y * p3.y + p3.z * p3.z), (srgb.y * srgb.y + srgb.z * srgb.z))
    }

    private let mocha: [UInt32] = [0x45475A, 0xF38BA8, 0xA6E3A1, 0xF9E2AF, 0x89B4FA, 0xF5C2E7, 0x94E2D5, 0xA6ADC8,
                                   0x585B70, 0xF37799, 0x89D88B, 0xEBD391, 0x74A8FC, 0xF2AEDE, 0x6BD7CA, 0xBAC2DE]

    func testProgramColorsMapToTheNearestThemeColor() {
        let adjuster = ColorAdjuster()
        adjuster.set(ColorAdjustments(palette: mocha, mapText: true))
        XCTAssertEqual(adjuster.text(0xFF0000, over: 0x1E1E2E, themed: false), 0xF37799)
        XCTAssertEqual(adjuster.text(0x00FF00, over: 0x1E1E2E, themed: false), 0x89D88B)
        XCTAssertEqual(adjuster.text(0x5F87FF, over: 0x1E1E2E, themed: false), 0x74A8FC)
        for c in mocha { XCTAssertEqual(adjuster.text(c, over: 0x1E1E2E, themed: false), c) }
        // The default foreground, faint text and the like keep their color.
        XCTAssertEqual(adjuster.text(0xFF0000, over: 0x1E1E2E, themed: true), 0xFF0000)
    }

    /// With the palette and contrast both on, the mapped color is then made readable.
    func testMappedColorsGetContrast() {
        let adjuster = ColorAdjuster()
        adjuster.set(ColorAdjustments(minimumContrast: 3, palette: mocha, mapText: true))
        let out = adjuster.text(0x404050, over: 0x45475A, themed: false)
        XCTAssertGreaterThanOrEqual(ratio(out, 0x45475A), 3)
    }

    func testOnlyTheMapOptionMapsText() {
        let adjuster = ColorAdjuster()
        adjuster.set(ColorAdjustments(palette: mocha, blendBackgrounds: true))
        XCTAssertEqual(adjuster.text(0xFF0000, over: 0x1E1E2E, themed: false), 0xFF0000)
    }

    /// A program's own backgrounds take on the theme's; the theme's colors stay as they are.
    func testProgramBackgroundsBlendWithTheTheme() {
        let adjuster = ColorAdjuster()
        adjuster.set(ColorAdjustments(palette: mocha, blendBackgrounds: true))
        XCTAssertEqual(adjuster.background(0x000000, themeBackground: 0x1E1E2E), 0x1E1E2E)
        XCTAssertEqual(adjuster.background(0x45475A, themeBackground: 0x1E1E2E), 0x45475A)
        XCTAssertEqual(adjuster.background(0x1E1E2E, themeBackground: 0x1E1E2E), 0x1E1E2E)
        let tinted = adjuster.background(0x282C34, themeBackground: 0x1E1E2E)
        XCTAssertGreaterThan(ColorAdjuster.luminance(tinted, p3: false), ColorAdjuster.luminance(0x282C34, p3: false))
        // A light theme multiplies: white becomes the theme background.
        XCTAssertEqual(adjuster.background(0xFFFFFF, themeBackground: 0xEFF1F5), 0xEFF1F5)

        adjuster.set(ColorAdjustments(palette: mocha))
        XCTAssertEqual(adjuster.background(0x000000, themeBackground: 0x1E1E2E), 0x000000)
    }

    func testIdentityByDefault() {
        XCTAssertTrue(ColorAdjustments().isIdentity)
        XCTAssertFalse(ColorAdjustments(blendBackgrounds: true).isIdentity)
        XCTAssertTrue(ColorAdjustments(palette: mocha, displayP3: true).isIdentity)
    }
}
