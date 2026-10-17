import XCTest
@testable import Thurm

final class PaneIconTests: XCTestCase {
    /// Each tile further back is smaller, still centered, and peeks out higher than the one in
    /// front of it.
    func testCardsBehindShrinkAndRise() {
        let size: CGFloat = 20
        let front = NSRect(x: 0, y: 0, width: size, height: size)
        var previous = front
        for depth in 1...PaneIcon.maxBehind {
            let card = PaneIcon.card(depth, size: size)
            XCTAssertLessThan(card.width, previous.width)
            XCTAssertEqual(card.width, card.height)
            XCTAssertEqual(card.midX, front.midX, accuracy: 0.001)
            XCTAssertLessThan(card.minY, previous.minY)
            previous = card
        }
    }

    /// The tiles behind only peek: each one stays mostly hidden behind the one in front.
    func testCardsBehindOnlyPeek() {
        let size: CGFloat = 20
        for depth in 1...PaneIcon.maxBehind {
            let card = PaneIcon.card(depth, size: size)
            let inFront = depth == 1 ? NSRect(x: 0, y: 0, width: size, height: size) : PaneIcon.card(depth - 1, size: size)
            XCTAssertGreaterThan(inFront.minY - card.minY, 1)
            XCTAssertLessThan(inFront.minY - card.minY, size * 0.2)
        }
        XCTAssertLessThanOrEqual(-PaneIcon.card(PaneIcon.maxBehind, size: size).minY, 6,
                                 "the deepest tile must fit above the tile in a 26pt tab pill")
    }
}
