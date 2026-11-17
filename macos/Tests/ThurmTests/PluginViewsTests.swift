import Metal
import XCTest
@testable import Thurm

final class PluginViewsTests: XCTestCase {
    private func key(_ id: UInt64) -> PaneKey { .local(id) }

    private func requireMetal() throws {
        try XCTSkipIf(MTLCreateSystemDefaultDevice() == nil, "no Metal device")
        _ = try XCTUnwrap(MetalContext.shared, "Metal pipeline initialization failed")
    }

    override func tearDown() {
        PluginViews.shared.apply([])
        super.tearDown()
    }

    private let json: Any = [[
        "client": 4, "id": "claude", "slot": "window.bottom_trailing",
        "view": [
            "type": "stack", "align": "top_trailing", "fill": "#ff9f0acc", "corner": 17, "later": true,
            "children": [["type": "icon", "symbol": "sparkle"]],
            "action": ["name": "usage.refresh", "args": ["tool": "claude"]],
            "popover": ["type": "column", "children": [["type": "text", "text": "5-hour limit"]]],
        ],
    ]]

    func testViewsDecodeAsTheDaemonSendsThem() throws {
        let views = PluginViews.decode(json)
        let view = try XCTUnwrap(views.first)
        XCTAssertEqual(view.key, "4/claude")
        XCTAssertEqual(view.view.type, "stack")
        XCTAssertEqual(view.view.align, "top_trailing")
        XCTAssertEqual(view.view.corner, 17)
        XCTAssertEqual(view.view.children.first?.symbol, "sparkle")
        XCTAssertEqual(view.view.action?.name, "usage.refresh")
        XCTAssertEqual(view.view.action?.args.flatMap { JSON.decode($0) as? [String: String] }, ["tool": "claude"])
        XCTAssertEqual(view.view.popover?.node.children.first?.text, "5-hour limit")
        XCTAssertEqual(PluginSlot.allCases.map(\.rawValue),
                       ["window.bottom_trailing", "window.bottom_leading", "window.top_trailing"])
        XCTAssertNotNil(PluginNodeView.color("#ff9f0a"))
        XCTAssertNotNil(PluginNodeView.color("#ff9f0acc"))
        XCTAssertNil(PluginNodeView.color("orange"))
        XCTAssertNil(PluginNodeView.color("#nope"))
        XCTAssertEqual(PluginViews.decode(["not": "a list"]), [])
        let png = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8DwHwAFBQIAX8jx0gAAAABJRU5ErkJggg=="
        XCTAssertEqual(PluginNodeView.image(png)?.size, NSSize(width: 1, height: 1))
        XCTAssertNil(PluginNodeView.image("not base64"))
    }

    func testMountedViewsShowInTheirSlotAboveTheMinimizedCards() throws {
        try requireMetal()
        let root = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(key(1)), second: .leaf(key(2)))
        let content = TabContentView(root: root, focused: key(1), zoomed: nil,
                                     floating: [FloatingPane(pane: key(3), frame: [0.2, 0.2, 0.5, 0.5],
                                                             minimized: true)])
        content.frame = CGRect(x: 0, y: 0, width: 1000, height: 600)
        defer { content.detachAll() }
        content.layoutSubtreeIfNeeded()
        let dock = try XCTUnwrap(content.pluginSlots.first { $0.slot == .bottomTrailing })
        XCTAssertTrue(dock.isHidden, "no views, no slot")
        let barAlone = content.minimizedBar.frame

        PluginViews.shared.apply(PluginViews.decode(json))
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        content.layoutPanes()
        XCTAssertFalse(dock.isHidden)
        XCTAssertGreaterThan(dock.frame.width, 0)
        XCTAssertEqual(dock.frame.maxX, content.paneArea.maxX - 12, accuracy: 0.5)
        XCTAssertEqual(dock.frame.maxY, content.paneArea.maxY - 10, accuracy: 0.5)
        XCTAssertLessThan(content.minimizedBar.frame.maxY, dock.frame.minY + 0.5, "the chips move up over it")
        XCTAssertLessThan(content.minimizedBar.frame.minY, barAlone.minY)
        XCTAssertTrue(content.pluginSlots.filter { $0.slot != .bottomTrailing }.allSatisfy(\.isHidden))

        PluginViews.shared.apply([])
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        content.layoutPanes()
        XCTAssertTrue(dock.isHidden)
    }

    func testARightClickOpensTheViewsMenuAndNeverTheTerminals() throws {
        let menuView: Any = [[
            "client": 4, "id": "usage", "slot": "window.bottom_trailing",
            "view": ["type": "icon", "symbol": "sparkle", "width": 40, "height": 40, "menu": [
                ["title": "Show Codex", "action": ["name": "usage.toggle"], "checked": true],
                [:],
                ["title": "Refresh", "action": ["name": "usage.refresh"]],
            ]],
        ]]
        let views = PluginViews.decode(menuView)
        XCTAssertEqual(views.first?.view.menu.map(\.title), ["Show Codex", "", "Refresh"])
        XCTAssertEqual(views.first?.view.menu.first?.checked, true)

        PluginViews.shared.apply(views)
        let window = NSWindow(contentRect: CGRect(x: 0, y: 0, width: 200, height: 200), styleMask: [.titled],
                              backing: .buffered, defer: false)
        let host = PluginSlotHost(slot: .bottomTrailing) { _, _ in }
        host.isHidden = false
        host.frame = CGRect(x: 0, y: 0, width: 40, height: 40)
        window.contentView?.addSubview(host)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        host.layoutSubtreeIfNeeded()
        let event = try XCTUnwrap(NSEvent.mouseEvent(with: .rightMouseDown, location: CGPoint(x: 20, y: 20),
                                                     modifierFlags: [], timestamp: 0,
                                                     windowNumber: window.windowNumber, context: nil,
                                                     eventNumber: 0, clickCount: 1, pressure: 1))
        XCTAssertTrue(window.contentView?.hitTest(CGPoint(x: 20, y: 20)) === host)
        XCTAssertEqual(host.menu(for: event)?.items.map(\.title), ["Show Codex", "", "Refresh"])
        XCTAssertEqual(host.menu(for: event)?.items.first?.state, .on)

        var plain = views
        plain[0].view.menu = []
        PluginViews.shared.apply(plain)
        RunLoop.main.run(until: Date().addingTimeInterval(0.2))
        XCTAssertNil(host.menu(for: event))
        let below = RightClickSpy()
        host.nextResponder = below
        host.rightMouseDown(with: event)
        XCTAssertFalse(below.rightClicked, "the terminal under the slot gets no right click")
    }
}

private final class RightClickSpy: NSResponder {
    var rightClicked = false
    override func rightMouseDown(with event: NSEvent) { rightClicked = true }
}
