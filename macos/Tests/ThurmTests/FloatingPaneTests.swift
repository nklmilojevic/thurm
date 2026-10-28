import XCTest
@testable import Thurm

final class FloatingPaneTests: XCTestCase {
    private func key(_ id: UInt64) -> PaneKey { .local(id) }
    private let area = CGRect(x: 0, y: 40, width: 1000, height: 600)

    func testCardsStayInsideTheirAreaAndAboveTheMinimum() {
        let card = FloatingGeometry.rect(CGRect(x: 0.2, y: 0.1, width: 0.5, height: 0.5), in: area)
        XCTAssertEqual(card, CGRect(x: 200, y: 100, width: 500, height: 300))
        // Too small grows to the minimum; past the edge comes back inside.
        let tiny = FloatingGeometry.rect(CGRect(x: 0.9, y: 0.9, width: 0.01, height: 0.01), in: area)
        XCTAssertEqual(tiny.size, FloatingGeometry.minSize)
        XCTAssertEqual(tiny.maxX, area.maxX)
        XCTAssertEqual(tiny.maxY, area.maxY)
        // An area smaller than the minimum: the card fills it.
        let small = CGRect(x: 0, y: 0, width: 200, height: 100)
        XCTAssertEqual(FloatingGeometry.rect(CGRect(x: 0.5, y: 0.5, width: 0.5, height: 0.5), in: small), small)
        // Fractions round trip.
        let fraction = FloatingGeometry.fraction(of: card, in: area)
        XCTAssertEqual(fraction.minX, 0.2, accuracy: 1e-9)
        XCTAssertEqual(fraction.height, 0.5, accuracy: 1e-9)
    }

    func testResizingMovesOnlyTheDraggedEdges() {
        let card = CGRect(x: 200, y: 100, width: 500, height: 300)
        let wider = FloatingGeometry.resized(card, edges: .right, by: CGVector(dx: 50, dy: 80), in: area)
        XCTAssertEqual(wider, CGRect(x: 200, y: 100, width: 550, height: 300))
        let corner = FloatingGeometry.resized(card, edges: [.top, .left], by: CGVector(dx: -30, dy: -20), in: area)
        XCTAssertEqual(corner, CGRect(x: 170, y: 80, width: 530, height: 320))
        // Not past the minimum, nor out of the area; the other edges hold.
        let narrow = FloatingGeometry.resized(card, edges: .left, by: CGVector(dx: 400, dy: 0), in: area)
        XCTAssertEqual(narrow.width, FloatingGeometry.minSize.width)
        XCTAssertEqual(narrow.maxX, card.maxX)
        let tall = FloatingGeometry.resized(card, edges: .top, by: CGVector(dx: 0, dy: -500), in: area)
        XCTAssertEqual(tall.minY, area.minY)
        XCTAssertEqual(tall.maxY, card.maxY)
    }

    func testEdgesAreTheBandAroundACard() {
        let card = CGRect(x: 200, y: 100, width: 500, height: 300)
        XCTAssertEqual(FloatingGeometry.edges(at: CGPoint(x: 198, y: 250), of: card), .left)
        XCTAssertEqual(FloatingGeometry.edges(at: CGPoint(x: 703, y: 398), of: card), [.right, .bottom])
        XCTAssertEqual(FloatingGeometry.edges(at: CGPoint(x: 450, y: 101), of: card), .top)
        XCTAssertEqual(FloatingGeometry.edges(at: CGPoint(x: 450, y: 250), of: card), [])
        XCTAssertEqual(FloatingGeometry.edges(at: CGPoint(x: 100, y: 250), of: card), [])
    }

    func testDockSideIsTheTriangleUnderThePointer() {
        let pane = CGRect(x: 0, y: 0, width: 400, height: 200)
        XCTAssertEqual(FloatingGeometry.dockSide(CGPoint(x: 20, y: 100), in: pane), .left)
        XCTAssertEqual(FloatingGeometry.dockSide(CGPoint(x: 390, y: 120), in: pane), .right)
        XCTAssertEqual(FloatingGeometry.dockSide(CGPoint(x: 200, y: 10), in: pane), .up)
        XCTAssertEqual(FloatingGeometry.dockSide(CGPoint(x: 220, y: 190), in: pane), .down)
        XCTAssertEqual(FloatingGeometry.dockRect(pane, side: .left, gap: 0), CGRect(x: 0, y: 0, width: 200, height: 200))
        XCTAssertEqual(FloatingGeometry.dockRect(pane, side: .down, gap: 0), CGRect(x: 0, y: 100, width: 400, height: 100))
    }

    func testDividerCursorsStayOffCards() {
        // A vertical divider at x 500 from the top to the bottom, a card over its middle.
        let divider = CGRect(x: 497, y: 0, width: 6, height: 600)
        let parts = FloatingGeometry.uncovered(divider, axis: .horizontal, by: [CGRect(x: 300, y: 200, width: 400, height: 100)])
        XCTAssertEqual(parts, [CGRect(x: 497, y: 0, width: 6, height: 200), CGRect(x: 497, y: 300, width: 6, height: 300)])
        XCTAssertEqual(FloatingGeometry.uncovered(divider, axis: .horizontal, by: [CGRect(x: 0, y: 0, width: 100, height: 100)]),
                       [divider])
    }

    func testNewCardsCascade() {
        let first = FloatingGeometry.defaultFraction(index: 0)
        let second = FloatingGeometry.defaultFraction(index: 1)
        XCTAssertGreaterThan(second.minX, first.minX)
        XCTAssertEqual(second.size, first.size)
        XCTAssertLessThanOrEqual(FloatingGeometry.defaultFraction(index: 5).maxX, 1)
    }

    /// The layout as the daemon stores it (`thurm_proto::FloatingPane`).
    func testTabLayoutCodesFloatingPanes() throws {
        let json = """
        {"root":{"type":"pane","id":1},"focused":2,"zoomed":null,
         "floating":[{"id":2,"frame":[0.1,0.2,0.5,0.5]},{"id":7,"host":"devbox","frame":[0.3,0.3,0.4,0.4],"zoomed":true}]}
        """
        let tab = try JSONDecoder().decode(TabLayout.self, from: Data(json.utf8))
        XCTAssertEqual(tab.floating, [FloatingPane(pane: key(2), frame: [0.1, 0.2, 0.5, 0.5]),
                                      FloatingPane(pane: PaneKey("devbox", 7), frame: [0.3, 0.3, 0.4, 0.4], zoomed: true)])
        XCTAssertEqual(tab.panes, [key(1), key(2), PaneKey("devbox", 7)])
        XCTAssertEqual(tab.focusedKey, key(2))
        let back = try JSONDecoder().decode(TabLayout.self, from: JSONEncoder().encode(tab))
        XCTAssertEqual(back, tab)
        // A tab without floating panes is stored as before.
        let plain = TabLayout(title: nil, root: .pane(key(1)), focused: 1, zoomed: nil)
        let encoded = String(decoding: try JSONEncoder().encode(plain), as: UTF8.self)
        XCTAssertFalse(encoded.contains("floating"))
        XCTAssertFalse(encoded.contains("zoomed\":true"))
    }

    /// Same as `retain_tabs` on the Rust side: cards go with their panes, and the frontmost one
    /// takes over when the splits are gone.
    func testRetainingTabsKeepsAndPromotesCards() {
        var tab = TabLayout(title: nil, root: .split(dir: .right, ratio: 0.5, first: .pane(key(1)), second: .pane(key(2))),
                            focused: 3, zoomed: 1,
                            floating: [FloatingPane(pane: key(3), frame: [0.1, 0.1, 0.5, 0.5]),
                                       FloatingPane(pane: key(4), frame: [0.2, 0.2, 0.5, 0.5])])
        var kept = retainTabs([tab]) { $0 != key(2) }
        XCTAssertEqual(kept[0].focused, 3)
        XCTAssertEqual(kept[0].floating?.map(\.pane), [key(3), key(4)])
        kept = retainTabs([tab]) { $0 != key(3) }
        XCTAssertEqual(kept[0].focused, 1)
        XCTAssertEqual(kept[0].floating?.map(\.pane), [key(4)])
        tab.zoomed = nil
        kept = retainTabs([tab]) { $0 == key(3) || $0 == key(4) }
        XCTAssertEqual(kept[0].root, .pane(key(4)))
        XCTAssertEqual(kept[0].floating?.map(\.pane), [key(3)])
        XCTAssertEqual(kept[0].focused, 3)
        kept = retainTabs([tab]) { $0 == key(4) }
        XCTAssertEqual(kept[0].root, .pane(key(4)))
        XCTAssertNil(kept[0].floating)
        XCTAssertTrue(retainTabs([tab]) { _ in false }.isEmpty)
        // Layout.panes counts the cards.
        XCTAssertEqual(Layout(windows: [WindowLayout(frame: nil, tabs: [tab], selectedTab: 0, fullscreen: false)]).panes,
                       [key(1), key(2), key(3), key(4)])
    }

    func testFloatingCardsRoundTripTheirSavedFrame() {
        let card = FloatingCard(FloatingPane(pane: key(3), frame: [0.25, 0.5, 0.5, 0.25], zoomed: true))
        XCTAssertEqual(card.frame, CGRect(x: 0.25, y: 0.5, width: 0.5, height: 0.25))
        XCTAssertEqual(card.saved, FloatingPane(pane: key(3), frame: [0.25, 0.5, 0.5, 0.25], zoomed: true))
        // A malformed frame gets the default place.
        XCTAssertEqual(FloatingCard(FloatingPane(pane: key(3), frame: [1])).frame, FloatingGeometry.defaultFraction(index: 0))
    }

    /// Agents that floated in their tab float again when the wall puts them back.
    func testTheWallPutsFloatingAgentsBackAsCards() {
        let card = FloatingPane(pane: key(3), frame: [0.2, 0.2, 0.5, 0.5])
        let origin = WallOrigin(index: 0, tab: TabLayout(title: nil, root: .pane(key(2)), focused: 3, zoomed: nil,
                                                         floating: [card]))
        // The tab kept its shell (2), and got a card of its own (5) meanwhile.
        var plan = AgentWall.returns(origins: [origin], onWall: [key(3)], tabs: [[key(2), key(5)]], floating: [[key(5)]])
        XCTAssertEqual(plan.count, 1)
        XCTAssertEqual(plan[0].into, 0)
        XCTAssertEqual(plan[0].root.panes, [key(2)])
        XCTAssertEqual(plan[0].floating, [card])
        // The tab is gone: the agent comes back filling a new one.
        plan = AgentWall.returns(origins: [origin], onWall: [key(3)], tabs: [])
        XCTAssertNil(plan[0].into)
        XCTAssertEqual(plan[0].root.panes, [key(3)])
        XCTAssertEqual(plan[0].floating, [])
    }

    /// Two origins going back to one tab: the first one's card stays a card for the second.
    func testACardPutBackStaysACardForTheNextReturn() {
        let card = FloatingPane(pane: key(3), frame: [0.2, 0.2, 0.5, 0.5])
        let first = WallOrigin(index: 0, tab: TabLayout(title: nil, root: .pane(key(2)), focused: 2, zoomed: nil,
                                                        floating: [card]))
        let second = WallOrigin(index: 1, tab: TabLayout(title: nil, root: .split(dir: .right, ratio: 0.5,
                                                                                  first: .pane(key(2)),
                                                                                  second: .pane(key(4))),
                                                         focused: 4, zoomed: nil))
        let plan = AgentWall.returns(origins: [first, second], onWall: [key(3), key(4)], tabs: [[key(2)]],
                                     floating: [[]])
        XCTAssertEqual(plan.map(\.into), [0, 0])
        XCTAssertEqual(plan[0].floating, [card])
        XCTAssertEqual(Set(plan[1].root.panes), [key(2), key(4)])
    }
}
