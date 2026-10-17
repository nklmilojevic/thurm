import AppKit
import XCTest
@testable import Thurm

final class WorkingDotsTests: XCTestCase {
    private func pulsing(_ dots: WorkingDots) -> Bool {
        dots.dots.allSatisfy { $0.animation(forKey: WorkingDots.key) != nil }
    }

    /// Hidden, the dots leave the title the whole row.
    func testHiddenTakesNoWidth() {
        let dots = WorkingDots()
        XCTAssertEqual(dots.intrinsicContentSize.width, 0)
        dots.isShown = true
        XCTAssertGreaterThan(dots.intrinsicContentSize.width, 0)
        dots.isShown = false
        XCTAssertEqual(dots.intrinsicContentSize.width, 0)
    }

    /// The pulse runs only while shown in a window, and a reconfigure (every retitle of a
    /// working tab) leaves the running pulse in place rather than restarting it.
    func testPulseRunsOnlyWhileShownInAWindow() throws {
        try XCTSkipIf(Motion.reduced, "reduce motion keeps the dots still")
        let dots = WorkingDots()
        dots.isShown = true
        XCTAssertFalse(pulsing(dots))

        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 20),
                              styleMask: [.borderless], backing: .buffered, defer: true)
        window.contentView?.addSubview(dots)
        XCTAssertTrue(pulsing(dots))
        let first = dots.dots[0].animation(forKey: WorkingDots.key)
        dots.isShown = true
        XCTAssertTrue(dots.dots[0].animation(forKey: WorkingDots.key) === first)

        dots.isShown = false
        XCTAssertFalse(dots.dots.contains { $0.animation(forKey: WorkingDots.key) != nil })
        dots.isShown = true
        XCTAssertTrue(pulsing(dots))
        dots.removeFromSuperview()
        XCTAssertFalse(dots.dots.contains { $0.animation(forKey: WorkingDots.key) != nil })
    }
}
