import XCTest
@testable import Thurm

/// The right-click menu offers a link only for a click on a cell, not in the padding or past the
/// last column or row, where the clicked position clamps to an edge cell.
final class GridHitTests: XCTestCase {
    func testCellsInsideTheGrid() {
        XCTAssertTrue(TerminalView.isInGrid(col: 0, row: 0, cols: 80, rows: 24))
        XCTAssertTrue(TerminalView.isInGrid(col: 79.9, row: 23.9, cols: 80, rows: 24))
    }

    func testPaddingIsOutside() {
        XCTAssertFalse(TerminalView.isInGrid(col: -0.1, row: 3, cols: 80, rows: 24))
        XCTAssertFalse(TerminalView.isInGrid(col: 3, row: -0.1, cols: 80, rows: 24))
    }

    func testPastTheLastColumnOrRowIsOutside() {
        XCTAssertFalse(TerminalView.isInGrid(col: 80, row: 3, cols: 80, rows: 24))
        XCTAssertFalse(TerminalView.isInGrid(col: 3, row: 24.5, cols: 80, rows: 24))
    }
}
