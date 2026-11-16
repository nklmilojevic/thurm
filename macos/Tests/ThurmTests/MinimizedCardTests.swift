import Metal
import XCTest
@testable import Thurm

final class MinimizedCardTests: XCTestCase {
    private func key(_ id: UInt64) -> PaneKey { .local(id) }

    private func requireMetal() throws {
        try XCTSkipIf(MTLCreateSystemDefaultDevice() == nil, "no Metal device")
        _ = try XCTUnwrap(MetalContext.shared, "Metal pipeline initialization failed")
    }

    private func tab(minimized: Bool = false) -> TabContentView {
        let root = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(key(1)), second: .leaf(key(2)))
        let content = TabContentView(root: root, focused: key(3), zoomed: nil,
                                     floating: [FloatingPane(pane: key(3), frame: [0.2, 0.2, 0.5, 0.5],
                                                             minimized: minimized)])
        content.frame = CGRect(x: 0, y: 0, width: 1000, height: 600)
        return content
    }

    func testTabLayoutCodesMinimizedCards() throws {
        let json = #"{"id":2,"frame":[0.1,0.2,0.5,0.5],"minimized":true}"#
        let card = try JSONDecoder().decode(FloatingPane.self, from: Data(json.utf8))
        XCTAssertTrue(card.minimized)
        XCTAssertTrue(String(decoding: try JSONEncoder().encode(card), as: UTF8.self).contains("\"minimized\":true"))
        XCTAssertEqual(FloatingCard(card).saved, card)
        let shown = FloatingPane(pane: key(2), frame: [0.1, 0.2, 0.5, 0.5])
        XCTAssertFalse(String(decoding: try JSONEncoder().encode(shown), as: UTF8.self).contains("minimized"))
        let older = try JSONDecoder().decode(FloatingPane.self, from: Data(#"{"id":2,"frame":[0.1,0.2,0.5,0.5]}"#.utf8))
        XCTAssertFalse(older.minimized)
    }

    func testMinimizingHidesTheCardAndFocusesTheSplits() throws {
        try requireMetal()
        let content = tab()
        defer { content.detachAll() }
        XCTAssertEqual(content.focusedPane, key(3))
        content.setMinimized(key(3), true)
        XCTAssertTrue(content.isMinimized(key(3)))
        XCTAssertTrue(content.isCardHidden(key(3)))
        XCTAssertTrue(content.root.contains(content.focusedPane))
        XCTAssertEqual(content.visiblePaneIds, [key(1), key(2)])
        XCTAssertTrue(content.contains(key(3)), "its program keeps running in the tab")
        XCTAssertEqual(content.tabLayout(title: nil).floating?.first?.minimized, true)
    }

    func testFocusingAMinimizedCardRestoresIt() throws {
        try requireMetal()
        let content = tab(minimized: true)
        defer { content.detachAll() }
        XCTAssertTrue(content.root.contains(content.focusedPane), "a minimized card does not take the focus")
        content.focus(key(3))
        XCTAssertFalse(content.isMinimized(key(3)))
        XCTAssertEqual(content.focusedPane, key(3))
        XCTAssertEqual(content.visiblePaneIds, [key(1), key(2), key(3)])
        XCTAssertNil(content.tabLayout(title: nil).floating?.first.flatMap { $0.minimized ? $0 : nil })
    }

    func testClosingAMinimizedCardRemovesIt() throws {
        try requireMetal()
        let content = tab(minimized: true)
        defer { content.detachAll() }
        XCTAssertFalse(content.remove(pane: key(3)))
        XCTAssertFalse(content.contains(key(3)))
        XCTAssertTrue(content.floating.isEmpty)
    }

    func testHidingAndShowingCardsKeepsMinimizedOnesMinimized() throws {
        try requireMetal()
        let content = tab()
        defer { content.detachAll() }
        content.setMinimized(key(3), true)
        content.setFloatingHidden(true)
        content.setFloatingHidden(false)
        XCTAssertTrue(content.isMinimized(key(3)))
        XCTAssertTrue(content.root.contains(content.focusedPane))
    }

    private func minimizedTab(cards: Int, width: CGFloat) -> TabContentView {
        let floating = (0..<cards).map { i in
            FloatingPane(pane: key(UInt64(10 + i)), frame: [0.2, 0.2, 0.5, 0.5], minimized: true)
        }
        let content = TabContentView(root: .leaf(key(1)), focused: key(1), zoomed: nil, floating: floating)
        content.frame = CGRect(x: 0, y: 0, width: width, height: 600)
        content.layoutPanes()
        return content
    }

    private func setTitle(_ title: String, of pane: PaneKey) {
        SessionManager.shared.panes[pane] = PaneInfo(json: ["id": NSNumber(value: pane.id), "title": title, "alive": true])
    }

    func testChipsStayInOneRowAndReturnFromOverflowWhenWidened() throws {
        try requireMetal()
        for i in 0..<6 { setTitle("a long running build \(i)", of: key(UInt64(10 + i))) }
        defer { for i in 0..<6 { SessionManager.shared.panes[key(UInt64(10 + i))] = nil } }
        let content = minimizedTab(cards: 6, width: 360)
        defer { content.detachAll() }
        let bar = content.minimizedBar
        XCTAssertEqual(bar.chips.count, 6)
        XCTAssertGreaterThanOrEqual(bar.frame.minX, 0)
        XCTAssertLessThanOrEqual(bar.frame.maxX, 360)
        XCTAssertEqual(bar.frame.height, MinimizedCardChip.height)
        XCTAssertFalse(bar.overflow.isHidden)
        let shown = bar.chips.filter { !$0.isHidden }
        XCTAssertEqual(shown.count + bar.overflowPanes.count, 6)
        let visible = shown as [NSView] + [bar.overflow]
        for view in visible {
            XCTAssertTrue(bar.bounds.contains(view.frame), "every visible control stays on screen")
            XCTAssertEqual(view.frame.minY, 0)
            XCTAssertGreaterThan(view.frame.width, 0)
        }
        let frames = visible.map(\.frame)
        for (i, a) in frames.enumerated() {
            for b in frames[(i + 1)...] { XCTAssertFalse(a.intersects(b)) }
        }
        content.frame.size.width = 2000
        content.layoutPanes()
        XCTAssertEqual(bar.frame.height, MinimizedCardChip.height)
        XCTAssertTrue(bar.overflow.isHidden)
        XCTAssertTrue(bar.overflowPanes.isEmpty)
        XCTAssertTrue(bar.chips.allSatisfy { !$0.isHidden && bar.bounds.contains($0.frame) })
    }

    func testTheChipBarGrowsWithAMinimizedPanesTitle() throws {
        try requireMetal()
        setTitle("vim", of: key(10))
        defer { SessionManager.shared.panes[key(10)] = nil }
        let content = minimizedTab(cards: 1, width: 1000)
        defer { content.detachAll() }
        let before = content.minimizedBar.frame.width
        setTitle("cargo test --workspace", of: key(10))
        content.refreshHeaders()
        XCTAssertGreaterThan(content.minimizedBar.frame.width, before)
        XCTAssertEqual(content.minimizedBar.frame.maxX, content.paneArea.maxX - 12, accuracy: 0.5)
    }

    func testChipsAreButtonsThatRestoreTheirCard() throws {
        try requireMetal()
        setTitle("log tail", of: key(10))
        defer { SessionManager.shared.panes[key(10)] = nil }
        let content = minimizedTab(cards: 1, width: 1000)
        defer { content.detachAll() }
        let chip = try XCTUnwrap(content.minimizedBar.chips.first)
        XCTAssertTrue(chip.isAccessibilityElement())
        XCTAssertEqual(chip.accessibilityRole(), .button)
        XCTAssertEqual(chip.accessibilityLabel(), "Restore log tail")
        XCTAssertTrue(chip.canBecomeKeyView)
        XCTAssertTrue(chip.accessibilityPerformPress())
        XCTAssertFalse(content.isMinimized(key(10)))
        XCTAssertEqual(content.focusedPane, key(10))
    }

    func testANewMinimizedCardStartsAsAChipWithoutTheFocus() throws {
        try requireMetal()
        let content = TabContentView(root: .leaf(key(1)), focused: key(1), zoomed: nil, floating: [])
        content.frame = CGRect(x: 0, y: 0, width: 1000, height: 600)
        defer { content.detachAll() }
        content.addNewFloating(key(4), minimized: true)
        XCTAssertTrue(content.isMinimized(key(4)))
        XCTAssertEqual(content.focusedPane, key(1))
        XCTAssertEqual(content.visiblePaneIds, [key(1)])
        content.layoutPanes()
        XCTAssertEqual(content.minimizedBar.chips.map(\.pane), [key(4)])
    }

    func testMinimizingATiledPaneFloatsItAndLeavesTheFocus() throws {
        try requireMetal()
        let content = tab()
        defer { content.detachAll() }
        content.focus(key(1))
        let moves = content.focusMoves
        XCTAssertTrue(content.minimize(key(2), true))
        XCTAssertTrue(content.isFloating(key(2)))
        XCTAssertTrue(content.isMinimized(key(2)))
        XCTAssertEqual(content.root.panes, [key(1)])
        XCTAssertEqual(content.focusedPane, key(1))
        XCTAssertTrue(content.minimize(key(2), false))
        XCTAssertFalse(content.isMinimized(key(2)))
        XCTAssertEqual(content.focusedPane, key(1), "bringing it back does not focus it")
        XCTAssertEqual(content.focusMoves, moves, "the focus never went to it, not even for a moment")
    }

    func testMinimizingAtAPlaceMovesTheCardWithoutTheFocus() throws {
        try requireMetal()
        let content = tab()
        defer { content.detachAll() }
        content.focus(key(1))
        let moves = content.focusMoves
        XCTAssertTrue(content.minimize(key(2), true, at: .fraction(CGRect(x: 0.1, y: 0.1, width: 0.3, height: 0.3))))
        XCTAssertTrue(content.isMinimized(key(2)))
        let card = try XCTUnwrap(content.floating.first { $0.pane == key(2) })
        XCTAssertEqual(card.frame.minX, 0.1, accuracy: 0.01)
        XCTAssertEqual(card.frame.width, 0.3, accuracy: 0.01)
        XCTAssertTrue(content.minimize(key(2), false, at: .fraction(CGRect(x: 0.5, y: 0.5, width: 0.3, height: 0.3))))
        XCTAssertFalse(content.isMinimized(key(2)))
        XCTAssertEqual(try XCTUnwrap(content.floating.first { $0.pane == key(2) }).frame.minX, 0.5, accuracy: 0.01)
        XCTAssertEqual(content.focusMoves, moves)
    }

    func testAMinimizedCardLeavesHiddenCardsHidden() throws {
        try requireMetal()
        let content = tab()
        defer { content.detachAll() }
        content.setFloatingHidden(true)
        content.addNewFloating(key(4), minimized: true)
        content.layoutPanes()
        XCTAssertTrue(content.floatingHidden)
        XCTAssertTrue(content.isCardHidden(key(3)))
        XCTAssertEqual(content.minimizedBar.chips.map(\.pane), [key(4)], "its chip shows all the same")
        XCTAssertFalse(content.minimizedBar.isHidden)
        // Minimizing a pane of the splits leaves them hidden too.
        XCTAssertTrue(content.minimize(key(2), true))
        XCTAssertTrue(content.floatingHidden)
        XCTAssertTrue(content.isCardHidden(key(3)))
    }

    func testTheSplitsKeepTheirLastPane() throws {
        try requireMetal()
        let content = TabContentView(root: .leaf(key(1)), focused: key(1), zoomed: nil, floating: [])
        content.frame = CGRect(x: 0, y: 0, width: 1000, height: 600)
        defer { content.detachAll() }
        XCTAssertFalse(content.minimize(key(1), true))
        XCTAssertEqual(content.root.panes, [key(1)])
    }

    func testAChipDraggedOutOfTheRowStaysWhereItWasLeft() throws {
        try requireMetal()
        let floating = [
            FloatingPane(pane: key(10), frame: [0.2, 0.2, 0.5, 0.5], minimized: true, chip: [0.25, 0.5]),
            FloatingPane(pane: key(11), frame: [0.2, 0.2, 0.5, 0.5], minimized: true),
        ]
        let content = TabContentView(root: .leaf(key(1)), focused: key(1), zoomed: nil, floating: floating)
        content.frame = CGRect(x: 0, y: 0, width: 1000, height: 600)
        content.layoutPanes()
        defer { content.detachAll() }
        XCTAssertEqual(content.minimizedBar.chips.map(\.pane), [key(11)], "the row keeps the others")
        let chip = try XCTUnwrap(content.placedChips[key(10)])
        let area = content.paneArea
        XCTAssertEqual(chip.frame.minX, area.minX + 0.25 * area.width, accuracy: 1)
        XCTAssertEqual(chip.frame.minY, area.minY + 0.5 * area.height, accuracy: 1)
        XCTAssertEqual(content.tabLayout(title: nil).floating?.first?.chip, [0.25, 0.5])
        // Off the edge after a resize: kept inside the tab.
        content.frame.size.width = 200
        content.layoutPanes()
        XCTAssertLessThanOrEqual(chip.frame.maxX, content.paneArea.maxX)
        // Restored, it leaves the tab's chips.
        content.focus(key(10))
        content.layoutPanes()
        XCTAssertNil(content.placedChips[key(10)])
    }

    func testOnlyAPressReleasedOverTheChipRestoresIt() {
        let chip = CGRect(x: 10, y: 10, width: 100, height: 26)
        XCTAssertTrue(MinimizedCardChip.isClick(moved: false, cancelled: false, released: CGPoint(x: 50, y: 20), chip: chip))
        XCTAssertFalse(MinimizedCardChip.isClick(moved: false, cancelled: false, released: CGPoint(x: 111, y: 20), chip: chip),
                       "released just past its edge, without a drag")
        XCTAssertFalse(MinimizedCardChip.isClick(moved: true, cancelled: false, released: CGPoint(x: 50, y: 20), chip: chip))
        XCTAssertFalse(MinimizedCardChip.isClick(moved: false, cancelled: true, released: CGPoint(x: 50, y: 20), chip: chip))
    }

    func testTabLayoutCodesWhereAChipWasLeft() throws {
        let json = #"{"id":2,"frame":[0.1,0.2,0.5,0.5],"minimized":true,"chip":[0.25,0.75]}"#
        let card = try JSONDecoder().decode(FloatingPane.self, from: Data(json.utf8))
        XCTAssertEqual(card.chip, [0.25, 0.75])
        XCTAssertEqual(FloatingCard(card).chip, CGPoint(x: 0.25, y: 0.75))
        XCTAssertEqual(FloatingCard(card).saved, card)
        let row = FloatingPane(pane: key(2), frame: [0.1, 0.2, 0.5, 0.5], minimized: true)
        XCTAssertFalse(String(decoding: try JSONEncoder().encode(row), as: UTF8.self).contains("chip"))
    }

    func testChipsThatDoNotFitGoBehindAMenu() throws {
        try requireMetal()
        let count = 30
        for i in 0..<count { setTitle("build \(i)", of: key(UInt64(10 + i))) }
        defer { for i in 0..<count { SessionManager.shared.panes[key(UInt64(10 + i))] = nil } }
        let floating = (0..<count).map { i in
            FloatingPane(pane: key(UInt64(10 + i)), frame: [0.2, 0.2, 0.5, 0.5], minimized: true)
        }
        let content = TabContentView(root: .leaf(key(1)), focused: key(1), zoomed: nil, floating: floating)
        content.frame = CGRect(x: 0, y: 0, width: 300, height: 300)
        content.layoutPanes()
        defer { content.detachAll() }
        let bar = content.minimizedBar
        XCTAssertGreaterThanOrEqual(bar.frame.minY, content.paneArea.minY, "no row above the tab")
        XCTAssertEqual(bar.frame.height, MinimizedCardChip.height)
        XCTAssertFalse(bar.overflow.isHidden)
        let shown = bar.chips.filter { !$0.isHidden }
        XCTAssertEqual(shown.count + bar.overflowPanes.count, count, "every pane has a chip or a menu item")
        for view in shown as [NSView] + [bar.overflow] {
            XCTAssertTrue(bar.bounds.contains(view.frame))
        }
        let menu = bar.overflowMenu()
        XCTAssertEqual(menu.items.map(\.title), bar.overflowPanes.map { "build \($0.id - 10)" })
        let last = try XCTUnwrap(bar.overflowPanes.last)
        bar.restoreFromMenu(try XCTUnwrap(menu.items.last))
        XCTAssertFalse(content.isMinimized(last))
        XCTAssertEqual(content.focusedPane, last)
    }
}
