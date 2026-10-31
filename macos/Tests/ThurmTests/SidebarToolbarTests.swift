import AppKit
import XCTest
@testable import Thurm

final class SidebarToolbarTests: XCTestCase {
    private let manager = SessionManager.shared
    private var savedConfig: AppConfig!
    private var controllers: [TerminalWindowController] = []

    override func setUp() {
        super.setUp()
        _ = NSApplication.shared
        savedConfig = manager.config
        manager.config = AppConfig()
    }

    override func tearDown() {
        for c in controllers {
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        controllers = []
        manager.config = savedConfig
        super.tearDown()
    }

    private func controller() throws -> (TerminalWindowController, TerminalWindow, TabSplitViewController) {
        let pane = PaneKey(localHost, UInt64(controllers.count + 1))
        let c = manager.makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil)
        controllers.append(c)
        return (c, try XCTUnwrap(c.window as? TerminalWindow), try XCTUnwrap(c.tabSplit))
    }

    private func filter(_ window: TerminalWindow) -> NSToolbarItem? {
        window.toolbar?.items.first { $0.action == #selector(AppDelegate.toggleSidebarActiveOnly(_:)) }
    }

    /// Collapsing hides the filter, keeps New Tab directly accessible, and remembers filtering.
    func testCollapsedToolbarKeepsNewTabAndRestoresFilter() throws {
        manager.config.sidebarActiveOnly = true
        let (_, window, split) = try controller()
        XCTAssertEqual(filter(window)?.toolTip, "Show All Tabs")
        let secondButton = try XCTUnwrap(filter(window))
        let separator = try XCTUnwrap(window.toolbar?.items.first { $0.itemIdentifier == .sidebarTrackingSeparator })

        split.splitViewItems[0].isCollapsed = true
        XCTAssertNil(filter(window))
        XCTAssertTrue(manager.config.sidebarActiveOnly)
        XCTAssertTrue(window.toolbar?.items[1] === secondButton)
        XCTAssertTrue(window.toolbar?.items.contains { $0 === separator } == true)
        XCTAssertEqual(secondButton.toolTip, "New Tab (⌘T)")
        let actions = try XCTUnwrap(window.toolbar).items.compactMap(\.action)
        XCTAssertEqual(actions, [#selector(AppDelegate.toggleTabSidebar(_:)), #selector(AppDelegate.newTab(_:))])

        split.splitViewItems[0].isCollapsed = false
        XCTAssertTrue(filter(window) === secondButton)
        XCTAssertTrue(window.toolbar?.items.contains { $0 === separator } == true)
        XCTAssertEqual(filter(window)?.toolTip, "Show All Tabs")
    }

    /// Animated transitions retain the same toolbar section, including its trailing space.
    func testAnimatedTogglesKeepToolbarSection() throws {
        let (_, window, split) = try controller()
        let toolbar = try XCTUnwrap(window.toolbar)
        let leading = Array(toolbar.items.prefix(2))
        let separator = try XCTUnwrap(toolbar.items.first { $0.itemIdentifier == .sidebarTrackingSeparator })
        let trailingSpace = try XCTUnwrap(toolbar.items.last)

        for _ in 0..<2 {
            for collapsed in [true, false] {
                split.toggleSidebarAnimated()
                let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in !split.sidebar.holdReloads },
                                                        object: nil)
                wait(for: [settled], timeout: 3)
                XCTAssertEqual(split.sidebarCollapsed, collapsed)
                XCTAssertEqual(toolbar.items.prefix(2).map(ObjectIdentifier.init), leading.map(ObjectIdentifier.init))
                XCTAssertTrue(toolbar.items.first { $0.itemIdentifier == .sidebarTrackingSeparator } === separator)
                XCTAssertTrue(toolbar.items.last === trailingSpace)
                XCTAssertEqual(toolbar.items.filter { $0.action == #selector(AppDelegate.newTab(_:)) }.count, 1)
                XCTAssertEqual(filter(window) == nil, collapsed)
            }
        }
    }

    /// Leaving sidebar mode removes its section; re-entering restores the last toggled state.
    func testNativeTabsRemoveSidebarSection() throws {
        manager.config.sidebarActiveOnly = true
        let (c, window, initialSplit) = try controller()
        let toolbar = try XCTUnwrap(window.toolbar)
        let trailingSpace = try XCTUnwrap(toolbar.items.last)
        let initiallyCollapsed = initialSplit.sidebarCollapsed

        func toggle(_ split: TabSplitViewController) {
            split.toggleSidebarAnimated()
            let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in !split.sidebar.holdReloads },
                                                    object: nil)
            wait(for: [settled], timeout: 3)
        }
        defer {
            if let split = c.tabSplit, split.sidebarCollapsed != initiallyCollapsed {
                toggle(split)
            }
        }

        for collapsed in [false, true] {
            let split = try XCTUnwrap(c.tabSplit)
            // Only the user toggle updates the default used when sidebar mode is recreated.
            if split.sidebarCollapsed != collapsed { toggle(split) }
            XCTAssertEqual(split.sidebarCollapsed, collapsed)
            manager.config.sidebarTabs = false
            c.applyTabStyle()
            XCTAssertEqual(toolbar.items.map(\.itemIdentifier), [.flexibleSpace])
            XCTAssertTrue(toolbar.items.last === trailingSpace)

            manager.config.sidebarTabs = true
            c.applyTabStyle()
            XCTAssertEqual(try XCTUnwrap(c.tabSplit).sidebarCollapsed, collapsed)
            XCTAssertEqual(window.sidebarCollapsed, collapsed)
            XCTAssertEqual(toolbar.items.filter { $0.itemIdentifier == .sidebarTrackingSeparator }.count, 1)
            XCTAssertEqual(toolbar.items.filter { $0.action == #selector(AppDelegate.newTab(_:)) }.count, 1)
            if collapsed {
                XCTAssertNil(filter(window))
                XCTAssertEqual(toolbar.items[1].action, #selector(AppDelegate.newTab(_:)))
                XCTAssertEqual(toolbar.items[1].toolTip, "New Tab (⌘T)")
            } else {
                XCTAssertEqual(filter(window)?.toolTip, "Show All Tabs")
            }
            XCTAssertTrue(manager.config.sidebarActiveOnly)
            XCTAssertTrue(toolbar.items.last === trailingSpace)
        }
    }

    /// Both controls fit directly in the toolbar even at the window's minimum width.
    func testCollapsedControlsStayOutOfOverflow() throws {
        let (_, window, split) = try controller()
        window.makeKeyAndOrderFront(nil)
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.2))
        split.toggleSidebarAnimated()
        RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.3))
        defer {
            split.toggleSidebarAnimated()
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.3))
        }
        for width in [window.minSize.width, 800] {
            window.setFrame(NSRect(x: 0, y: 0, width: width, height: 300), display: false)
            window.contentView?.superview?.layoutSubtreeIfNeeded()
            RunLoop.current.run(until: Date(timeIntervalSinceNow: 0.2))
            let visible = try XCTUnwrap(window.toolbar?.visibleItems).compactMap(\.action)
            XCTAssertEqual(visible, [#selector(AppDelegate.toggleTabSidebar(_:)), #selector(AppDelegate.newTab(_:))],
                           "width: \(width)")
        }
    }

    /// A background tab's toolbar must not change the front tab's controls.
    func testWindowsKeepIndependentToolbarStates() throws {
        let (_, expanded, _) = try controller()
        let (c, collapsed, split) = try controller()
        split.splitViewItems[0].isCollapsed = true
        XCTAssertNotNil(filter(expanded))
        XCTAssertNil(filter(collapsed))

        manager.config.sidebarTabs = false
        c.applyTabStyle()
        XCTAssertTrue(try XCTUnwrap(collapsed.toolbar).items.compactMap(\.action).isEmpty)
        XCTAssertNotNil(filter(expanded))
    }

    /// Tab and workspace switches copy the sidebar state before the destination is shown.
    func testMatchingSidebarUpdatesToolbarBeforeShowingWindow() throws {
        let (_, _, source) = try controller()
        let (_, window, destination) = try controller()
        source.splitViewItems[0].isCollapsed = true
        destination.match(source)
        XCTAssertNil(filter(window))

        source.splitViewItems[0].isCollapsed = false
        destination.match(source)
        XCTAssertNotNil(filter(window))
    }

    /// The button/shortcut path also supplies the initial state of newly created tabs.
    func testNewTabInheritsCollapsedToolbar() throws {
        let (_, window, split) = try controller()
        func toggle() {
            split.toggleSidebarAnimated()
            let settled = XCTNSPredicateExpectation(predicate: NSPredicate { _, _ in !split.sidebar.holdReloads },
                                                    object: nil)
            wait(for: [settled], timeout: 3)
        }
        toggle()
        defer { toggle() }
        XCTAssertTrue(split.sidebarCollapsed)
        XCTAssertNil(filter(window))
        let (_, newWindow, newSplit) = try controller()
        XCTAssertTrue(newSplit.sidebarCollapsed)
        XCTAssertNil(filter(newWindow))
    }
}
