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
    func testPulseRunsOnlyWhileShownInAWindow() {
        let dots = WorkingDots()
        dots.reduced = { false }
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

    /// Reduce motion switched while a working row stays up stops or restarts its pulse; still,
    /// the dots are plainly visible.
    func testReduceMotionChangeWhileShown() {
        var reduced = false
        let dots = WorkingDots()
        dots.reduced = { reduced }
        dots.isShown = true
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 20),
                              styleMask: [.borderless], backing: .buffered, defer: true)
        window.contentView?.addSubview(dots)
        XCTAssertTrue(pulsing(dots))

        reduced = true
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil)
        XCTAssertFalse(dots.dots.contains { $0.animation(forKey: WorkingDots.key) != nil })
        XCTAssertTrue(dots.dots.allSatisfy { $0.opacity == 0.85 })

        reduced = false
        NSWorkspace.shared.notificationCenter.post(
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil)
        XCTAssertTrue(pulsing(dots))
    }

    /// Shown under reduce motion from the start, the dots stand still at full strength.
    func testReducedFromTheStartIsStillAndVisible() {
        let dots = WorkingDots()
        dots.reduced = { true }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 20),
                              styleMask: [.borderless], backing: .buffered, defer: true)
        window.contentView?.addSubview(dots)
        dots.isShown = true
        XCTAssertFalse(dots.dots.contains { $0.animation(forKey: WorkingDots.key) != nil })
        XCTAssertTrue(dots.dots.allSatisfy { $0.opacity == 0.85 })
    }
}
