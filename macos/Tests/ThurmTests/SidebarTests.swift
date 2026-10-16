import XCTest
@testable import Thurm

final class SidebarTests: XCTestCase {
    /// Distinct live objects standing in for the tabs' window controllers.
    private let a = NSObject(), b = NSObject(), c = NSObject(), d = NSObject()

    private func id(_ o: NSObject) -> ObjectIdentifier { ObjectIdentifier(o) }

    /// A retitled tab keeps its row: the same windows in the same groups and order.
    func testSameRowsKeepsRowsWhenOnlyContentChanged() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertTrue(TabSidebarViewController.sameRows(shown, [[id(a), id(b)], [id(c)]]))
    }

    func testSameRowsRejectsSameLengthReorder() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(b), id(a)], [id(c)]]))
    }

    func testSameRowsRejectsSameLengthReplacement() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(d)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b)], [id(d)]]))
    }

    func testSameRowsRejectsAddedOrRemovedTabs() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b), id(d)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a)], [id(c)]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a), id(b)]]))
    }

    func testSameRowsRejectsRegrouping() {
        let shown = [[id(a), id(b)], [id(c)]]
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[id(a)], [id(b), id(c)]]))
    }
}
