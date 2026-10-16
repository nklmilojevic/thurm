import XCTest
@testable import Thurm

final class SidebarTests: XCTestCase {
    private func tab(_ title: String) -> SidebarTab {
        SidebarTab(controller: nil, title: title, selected: false)
    }

    /// A retitled tab keeps its row; an added, removed or regrouped one rebuilds the list.
    func testSameRowsIgnoresWhatTheRowsShow() {
        let shown = [[tab("a"), tab("b")], [tab("c")]]
        XCTAssertTrue(TabSidebarViewController.sameRows(shown, [[tab("a ⠋"), tab("b")], [tab("c")]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[tab("a"), tab("b"), tab("d")], [tab("c")]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[tab("a")], [tab("b"), tab("c")]]))
        XCTAssertFalse(TabSidebarViewController.sameRows(shown, [[tab("a"), tab("b")]]))
    }
}
