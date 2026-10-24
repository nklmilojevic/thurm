import XCTest
@testable import Thurm

final class AgentWallTests: XCTestCase {
    private let window = CGSize(width: 1600, height: 1000)

    func testRowsMakeCellsShapedLikeTerminals() {
        XCTAssertEqual(AgentWall.rows(count: 1, in: window), [1])
        XCTAssertEqual(AgentWall.rows(count: 2, in: window), [2])
        XCTAssertEqual(AgentWall.rows(count: 3, in: window), [2, 1])
        XCTAssertEqual(AgentWall.rows(count: 4, in: window), [2, 2])
        XCTAssertEqual(AgentWall.rows(count: 6, in: window), [3, 3])
        XCTAssertEqual(AgentWall.rows(count: 7, in: window), [3, 2, 2])
        XCTAssertEqual(AgentWall.rows(count: 9, in: window), [3, 3, 3])
        // A tall window stacks more.
        XCTAssertEqual(AgentWall.rows(count: 2, in: CGSize(width: 800, height: 1400)), [1, 1])
        XCTAssertEqual(AgentWall.rows(count: 0, in: window), [])
        XCTAssertEqual(AgentWall.rows(count: 4, in: .zero), [2, 2])
    }

    func testGridGivesEveryPaneOfARowTheSameSpace() throws {
        let panes = (1...5).map { PaneKey.local(UInt64($0)) }
        let grid = try XCTUnwrap(AgentWall.grid(panes, rows: [3, 2]))
        XCTAssertEqual(grid.panes, panes)
        let frames = Dictionary(uniqueKeysWithValues: grid.frames(in: CGRect(x: 0, y: 0, width: 1200, height: 600),
                                                                   gap: 0))
        for pane in panes[0..<3] {
            XCTAssertEqual(frames[pane]!.width, 400, accuracy: 1)
            XCTAssertEqual(frames[pane]!.height, 300, accuracy: 1)
            XCTAssertEqual(frames[pane]!.minY, 0)
        }
        for pane in panes[3...] {
            XCTAssertEqual(frames[pane]!.width, 600, accuracy: 1)
            XCTAssertEqual(frames[pane]!.minY, 300, accuracy: 1)
        }
        XCTAssertNil(AgentWall.grid([], rows: []))
    }

    private func tab(_ root: LayoutNode, focused: UInt64, title: String? = nil) -> TabLayout {
        TabLayout(title: title, root: root, focused: focused, zoomed: nil)
    }

    private func key(_ id: UInt64) -> PaneKey { .local(id) }

    /// Agent 1 alone in tab 0; tab 1 a shell (2) under agent 3; agent 4 alone in tab 2.
    private var origins: [WallOrigin] {
        [
            WallOrigin(index: 0, tab: tab(.pane(key(1)), focused: 1, title: "api")),
            WallOrigin(index: 1, tab: tab(.split(dir: .down, ratio: 0.3, first: .pane(key(2)), second: .pane(key(3))),
                                          focused: 3)),
            WallOrigin(index: 2, tab: tab(.pane(key(4)), focused: 4)),
        ]
    }

    func testAgentsGoBackWhereTheyWere() {
        // Agent 9 started on the wall: it has no tab to go to.
        let plan = AgentWall.returns(origins: origins, onWall: [key(1), key(3), key(4), key(9)], tabs: [[key(2)]])
        XCTAssertEqual(plan.map(\.panes), [[key(1)], [key(3)], [key(4)]])
        XCTAssertEqual(plan.map(\.into), [nil, 0, nil])
        XCTAssertEqual(plan[0].root.layoutNode, .pane(key(1)))
        // The split comes back as it was, the shell on top with its share.
        XCTAssertEqual(plan[1].root.layoutNode, origins[1].tab.root)
    }

    func testPanesOpenedMeanwhileStayInTheirTab() {
        let plan = AgentWall.returns(origins: origins, onWall: [key(1), key(3)], tabs: [[key(2), key(5)]])
        // Agent 4 ended on the wall: nothing goes back to its tab.
        XCTAssertEqual(plan.map(\.panes), [[key(1)], [key(3)]])
        XCTAssertEqual(plan[1].root.panes, [key(2), key(3), key(5)])
    }

    func testAGoneTabComesBackWithOnlyItsAgents() {
        // The shell of tab 1 was closed while its agent was on the wall.
        let plan = AgentWall.returns(origins: origins, onWall: [key(3)], tabs: [])
        XCTAssertEqual(plan.count, 1)
        XCTAssertNil(plan[0].into)
        XCTAssertEqual(plan[0].root.layoutNode, .pane(key(3)))
    }

    func testOriginsGoBackInTabOrder() {
        let plan = AgentWall.returns(origins: origins.reversed(), onWall: [key(1), key(3), key(4)], tabs: [[key(2)]])
        XCTAssertEqual(plan.map(\.origin.index), [0, 1, 2])
    }

    func testTabsComeBackAfterTheTabTheyFollowed() {
        // Tabs were ~ (1), mixed (2), shell (6), alpha (7), gamma (9); alpha and gamma went to the
        // wall, then delta (10) opened next to it and joined, from after mixed.
        var tabs = [[key(1)], [key(2)], [key(6)]]
        let back: [(after: UInt64?, index: Int, pane: UInt64)] = [(2, 2, 10), (6, 3, 7), (7, 4, 9)]
        for b in back {
            tabs.insert([key(b.pane)], at: AgentWall.position(after: b.after, index: b.index, in: tabs))
        }
        XCTAssertEqual(tabs.map { $0[0].id }, [1, 2, 10, 6, 7, 9])
        XCTAssertEqual(AgentWall.position(after: nil, index: 3, in: tabs), 0)
        // The tab it followed is gone: its old place.
        XCTAssertEqual(AgentWall.position(after: 42, index: 3, in: tabs), 3)
        XCTAssertEqual(AgentWall.position(after: 42, index: 30, in: tabs), tabs.count)
    }

    /// The layout as the daemon stores it (`thurm_proto::TabLayout`).
    func testDecodesTheDaemonsLayout() throws {
        let json = """
        {"title":"Agents","root":{"type":"split","dir":"right","ratio":0.5,
          "first":{"type":"pane","id":1},"second":{"type":"pane","id":3}},"focused":1,"zoomed":null,
         "wall":[{"index":1,"after":3,"tab":{"title":"api","root":{"type":"pane","id":1},"focused":1,"zoomed":null}}]}
        """
        let decoded = try JSONDecoder().decode(TabLayout.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.wall, [WallOrigin(index: 1, after: 3, tab: tab(.pane(key(1)), focused: 1, title: "api"))])
        let plain = try JSONDecoder().decode(TabLayout.self, from: Data(#"{"root":{"type":"pane","id":1},"focused":1}"#.utf8))
        XCTAssertNil(plain.wall)
        let back = try JSONDecoder().decode(TabLayout.self, from: JSONEncoder().encode(decoded))
        XCTAssertEqual(back, decoded)
    }
}
