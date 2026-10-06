import AppKit

// Vertical tabs (config `window.tab_style = "sidebar"`), after tty7's sidebar: tabs grouped by
// git repository, each row showing the agent status, the title and the branch with its diff
// size. Tabs are still native window tabs underneath (one NSWindow per tab, with the native tab
// bar suppressed), so restore, Cmd+1…9 and "Move Tab to New Workspace" keep working;
// every window of a tab group shows the same list. Below the tabs, an agents panel lists every
// pane running a coding agent (in any window or workspace), so an agent in an unfocused split
// is one click away; it has its own selection: the focused agent pane.

/// One tab as the sidebar shows it.
struct SidebarTab {
    weak var controller: TerminalWindowController?
    var title: String
    var subtitle: String?
    var branch: String?
    var added: UInt32 = 0
    var removed: UInt32 = 0
    var status: AgentStatus?
    var shortcut: Int?
    var selected: Bool
}

extension SidebarTab: Equatable {
    static func == (a: SidebarTab, b: SidebarTab) -> Bool {
        a.controller === b.controller && a.title == b.title && a.subtitle == b.subtitle && a.branch == b.branch
            && a.added == b.added && a.removed == b.removed && a.status == b.status
            && a.shortcut == b.shortcut && a.selected == b.selected
    }
}

/// Tabs sharing a repository (or "Terminals" for the rest).
final class SidebarGroup {
    let name: String
    var tabs: [SidebarTab] = []
    /// Row objects, built once per reload (NSOutlineView tracks items by identity).
    var boxes: [TabBox] = []
    init(name: String) { self.name = name }
}

final class TabBox {
    let tab: SidebarTab
    init(_ tab: SidebarTab) { self.tab = tab }
}

/// A pane running an agent, as the agents panel shows it.
final class AgentBox {
    let pane: PaneKey
    /// The pane's workspace when no window shows it.
    let hiddenWorkspace: UInt64?
    let row: SidebarTab
    init(pane: PaneKey, hiddenWorkspace: UInt64?, row: SidebarTab) {
        self.pane = pane
        self.hiddenWorkspace = hiddenWorkspace
        self.row = row
    }
}

/// Split view hosting the sidebar next to the tab's split tree.
final class TabSplitViewController: NSSplitViewController {
    /// New windows open with the sidebar the way it was last toggled (Cmd+B).
    private static var collapsedByDefault = false

    let sidebar = TabSidebarViewController()
    private let host = NSViewController()

    init(content: TabContentView, width: CGFloat) {
        super.init(nibName: nil, bundle: nil)
        host.view = content
        let side = NSSplitViewItem(sidebarWithViewController: sidebar)
        side.minimumThickness = 180
        side.maximumThickness = 480
        side.canCollapse = true
        side.holdingPriority = .defaultLow + 10
        sidebar.preferredWidth = width
        side.isCollapsed = Self.collapsedByDefault
        let main = NSSplitViewItem(viewController: host)
        addSplitViewItem(side)
        addSplitViewItem(main)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        // NSSplitViewItem has no initial-thickness API; place the divider once.
        if !sidebarCollapsed { placeDivider() }
    }

    private func placeDivider() {
        if let w = sidebar.preferredWidth {
            splitView.setPosition(w, ofDividerAt: 0)
            sidebar.preferredWidth = nil
        }
    }

    var sidebarCollapsed: Bool {
        splitViewItems.first?.isCollapsed ?? false
    }

    func toggleSidebarAnimated() {
        guard !togglingSidebar, let side = splitViewItems.first else { return }
        let collapse = !side.isCollapsed
        Self.collapsedByDefault = collapse
        let panes = (host.view as? TabContentView)?.views.values.map { $0 } ?? []
        panes.forEach { $0.holdGridSize = true }
        sidebar.holdReloads = true
        togglingSidebar = true
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(saveWidth), object: nil)
        NSAnimationContext.runAnimationGroup({ ctx in
            ctx.duration = 0.2
            ctx.timingFunction = CAMediaTimingFunction(name: .easeInEaseOut)
            side.animator().isCollapsed = collapse
        }, completionHandler: { [weak self] in
            if !collapse { self?.placeDivider() }
            self?.view.layoutSubtreeIfNeeded()
            self?.togglingSidebar = false
            self?.sidebar.holdReloads = false
            panes.forEach { $0.holdGridSize = false }
        })
    }

    /// Do not save intermediate widths while the sidebar changes state.
    private var togglingSidebar = false

    /// Remember the width the user dragged the sidebar to (in config.toml, debounced).
    override func splitViewDidResizeSubviews(_ notification: Notification) {
        super.splitViewDidResizeSubviews(notification)
        guard sidebar.preferredWidth == nil, !togglingSidebar, !sidebarCollapsed, view.window?.inLiveResize == false,
              let w = splitViewItems.first?.viewController.view.frame.width, w >= 180 else { return }
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(saveWidth), object: nil)
        pendingWidth = w
        perform(#selector(saveWidth), with: nil, afterDelay: 1.0)
    }

    private var pendingWidth: CGFloat = 0

    @objc private func saveWidth() {
        let w = (pendingWidth).rounded()
        guard abs(w - SessionManager.shared.config.sidebarWidth) >= 1 else { return }
        _ = Core.shared.request(object: ["SetSetting": ["key": "window.sidebar_width", "value": String(format: "%.1f", w)]])
    }
}

final class TabSidebarViewController: NSViewController, NSOutlineViewDataSource, NSOutlineViewDelegate {
    weak var controller: TerminalWindowController?
    var preferredWidth: CGFloat?
    /// Set while the sidebar animates open or closed: a reload then would lay the rows out
    /// again on every frame of the animation, so it waits for the end.
    var holdReloads = false {
        didSet {
            guard oldValue, !holdReloads, reloadHeld else { return }
            reloadHeld = false
            reload()
        }
    }
    private var reloadHeld = false
    /// The rows as last shown: agents retitle their panes several times a second, which
    /// mostly changes nothing the sidebar shows.
    private var shown: (names: [String], tabs: [[SidebarTab]])?

    private let outline = NSOutlineView()
    private var groups: [SidebarGroup] = []
    private static let dragType = NSPasteboard.PasteboardType("com.thurm.sidebar-tab")
    /// Group names the user collapsed (kept across reloads and launches).
    private static var collapsed: Set<String> {
        get { Set(UserDefaults.standard.stringArray(forKey: "SidebarCollapsedGroups") ?? []) }
        set { UserDefaults.standard.set(Array(newValue), forKey: "SidebarCollapsedGroups") }
    }
    /// Headers only when there is more than one group.
    private var showHeaders = false
    /// The window's workspace; click for the workspace menu.
    private let workspaceButton = NSButton(title: "", target: nil, action: nil)
    private let workspaceMenu = NSMenu()
    private let agents = AgentsPanel()

    /// Solid sidebar color from the theme (set by the window controller). The stock sidebar
    /// material is translucent, so its shade depended on whatever was behind the window.
    var backgroundColor: NSColor? {
        didSet { view.layer?.backgroundColor = backgroundColor?.cgColor }
    }

    override func loadView() {
        let scroll = NSScrollView()
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true

        let column = NSTableColumn(identifier: .init("tab"))
        outline.addTableColumn(column)
        outline.outlineTableColumn = column
        outline.headerView = nil
        outline.style = .sourceList
        outline.rowSizeStyle = .custom
        outline.floatsGroupRows = false
        outline.indentationPerLevel = 0
        outline.backgroundColor = .clear
        outline.dataSource = self
        outline.delegate = self
        outline.target = self
        outline.action = #selector(rowClicked(_:))
        outline.menu = rowMenu()
        outline.registerForDraggedTypes([Self.dragType])
        outline.setDraggingSourceOperationMask(.move, forLocal: true)
        scroll.documentView = outline

        workspaceButton.target = self
        workspaceButton.action = #selector(showWorkspaceMenu(_:))
        workspaceButton.isBordered = false
        workspaceButton.image = NSImage(systemSymbolName: "chevron.up.chevron.down", accessibilityDescription: "Workspaces")
        workspaceButton.imagePosition = .imageTrailing
        workspaceButton.imageHugsTitle = true
        workspaceButton.symbolConfiguration = .init(pointSize: 9, weight: .semibold)
        workspaceButton.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .semibold)
        workspaceButton.contentTintColor = .secondaryLabelColor
        workspaceButton.alignment = .left
        workspaceButton.lineBreakMode = .byTruncatingTail
        workspaceButton.toolTip = "Workspaces (⌘⇧O)"
        workspaceButton.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        workspaceButton.translatesAutoresizingMaskIntoConstraints = false

        let root = NSView()
        root.wantsLayer = true
        root.layer?.backgroundColor = backgroundColor?.cgColor
        scroll.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(workspaceButton)
        root.addSubview(scroll)
        root.addSubview(agents)
        agents.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            workspaceButton.topAnchor.constraint(equalTo: root.safeAreaLayoutGuide.topAnchor, constant: 2),
            workspaceButton.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: 16),
            workspaceButton.trailingAnchor.constraint(lessThanOrEqualTo: root.trailingAnchor, constant: -12),
            scroll.topAnchor.constraint(equalTo: workspaceButton.bottomAnchor, constant: 4),
            scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: agents.topAnchor),
            agents.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            agents.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            agents.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -8),
        ])
        view = root
    }

    // MARK: Model

    /// Rebuilds the rows from the window's tab group (visual order) and the pane metadata.
    @objc private func showWorkspaceMenu(_ sender: NSButton) {
        SessionManager.shared.fillWorkspaceMenu(workspaceMenu)
        workspaceMenu.popUp(positioning: nil, at: NSPoint(x: 0, y: sender.bounds.maxY + 4), in: sender)
    }

    /// Tabs in the order the sidebar lists them: grouped by repository, the groups in the order
    /// of their first tab, then the rest ("Terminals"). Numbers follow it, so they can change
    /// when a tab closes or moves to another repository.
    static func displayOrder(_ windows: [NSWindow]) -> [NSWindow] {
        var roots: [String] = []
        var byRoot: [String: [NSWindow]] = [:]
        var other: [NSWindow] = []
        for w in windows {
            guard let c = w.windowController as? TerminalWindowController, !c.isClosed else { continue }
            if let root = group(for: c)?.key {
                if byRoot[root] == nil { roots.append(root) }
                byRoot[root, default: []].append(w)
            } else {
                other.append(w)
            }
        }
        return roots.flatMap { byRoot[$0] ?? [] } + other
    }

    /// The group a tab is listed under: its repository ("devbox · repo" on a remote host), or
    /// for a handoff tab the local repository it came from.
    static func group(for c: TerminalWindowController) -> (key: String, name: String)? {
        if let h = c.handoffID.flatMap({ Remotes.shared.handoffs[$0] }) {
            return (h.repo, (h.repo as NSString).lastPathComponent)
        }
        guard let root = SessionManager.shared.panes[c.focusedPane]?.git?.root else { return nil }
        let name = (root as NSString).lastPathComponent
        return c.host == localHost ? (root, name) : ("\(c.host):\(root)", "\(c.host) · \(name)")
    }

    func reload() {
        guard isViewLoaded, let window = controller?.window else { return }
        if holdReloads {
            reloadHeld = true
            return
        }
        let workspace = controller.flatMap { SessionManager.shared.workspace($0.workspaceID)?.name } ?? ""
        if workspaceButton.title != workspace { workspaceButton.title = workspace }
        agents.controller = controller
        agents.reload()
        let windows = controller?.orderedTabs ?? [window]
        let panes = SessionManager.shared.panes
        var byRoot: [String: SidebarGroup] = [:]
        var order: [SidebarGroup] = []
        let other = SidebarGroup(name: "Terminals")
        for (index, w) in windows.enumerated() {
            guard let c = w.windowController as? TerminalWindowController, !c.isClosed else { continue }
            let info = panes[c.focusedPane]
            var status: AgentStatus?
            for id in c.content.paneIds {
                if let a = panes[id]?.agent, status == nil || a.status.urgency > status!.urgency {
                    status = a.status
                }
            }
            let git = info?.git
            // Remote paths are the host's, not abbreviated against this Mac's home.
            let dir = info?.cwd.map { c.host == localHost ? abbreviatePath($0) : "\(c.host):\($0)" }
            var tab = SidebarTab(controller: c,
                                 title: c.titleOverride ?? info?.displayTitle ?? "Thurm",
                                 subtitle: dir,
                                 status: status,
                                 shortcut: tabShortcut(index: index, count: windows.count),
                                 selected: w === window)
            if let g = git {
                tab.branch = c.host == localHost ? g.branch : "\(c.host) · \(g.branch)"
                tab.added = g.added
                tab.removed = g.removed
            }
            if let h = c.handoffID.flatMap({ Remotes.shared.handoffs[$0] }) {
                // A handoff: where it runs, and whether its work came back.
                tab.branch = "\(h.host) · \(h.branch)" + (Remotes.shared.handoffErrors[h.id] != nil ? " · fetch failed" : "")
            }
            if let (key, name) = Self.group(for: c) {
                let group = byRoot[key] ?? {
                    let n = SidebarGroup(name: name)
                    byRoot[key] = n
                    order.append(n)
                    return n
                }()
                group.tabs.append(tab)
            } else {
                other.tabs.append(tab)
            }
        }
        if !other.tabs.isEmpty { order.append(other) }
        let names = order.map(\.name), tabs = order.map(\.tabs)
        if shown.map({ $0.names == names && $0.tabs == tabs }) != true {
            shown = (names, tabs)
            for g in order { g.boxes = g.tabs.map(TabBox.init) }
            groups = order
            showHeaders = groups.count > 1
            outline.reloadData()
            let collapsed = Self.collapsed
            for g in groups where !collapsed.contains(g.name) { outline.expandItem(g) }
        }
        // Select the row of this window's tab, also when the rows are unchanged: a click on
        // another tab's row selected that row here, and this window's tab is the one shown again.
        let current = (0..<outline.numberOfRows).first { (outline.item(atRow: $0) as? TabBox)?.tab.selected == true }
        if let row = current, outline.selectedRow != row {
            outline.selectRowIndexes([row], byExtendingSelection: false)
        }
    }

    // MARK: NSOutlineViewDataSource

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        if item == nil {
            return showHeaders ? groups.count : groups.first?.boxes.count ?? 0
        }
        if let g = item as? SidebarGroup { return g.boxes.count }
        return 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        if item == nil {
            if showHeaders { return groups[index] }
            return groups[0].boxes[index]
        }
        return (item as! SidebarGroup).boxes[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        item is SidebarGroup
    }

    // Collapsing a repository group sticks.
    func outlineViewItemDidCollapse(_ notification: Notification) {
        if let g = notification.userInfo?["NSObject"] as? SidebarGroup { Self.collapsed.insert(g.name) }
    }

    func outlineViewItemDidExpand(_ notification: Notification) {
        if let g = notification.userInfo?["NSObject"] as? SidebarGroup { Self.collapsed.remove(g.name) }
    }

    // MARK: Drag to reorder (moves the native tab)

    func outlineView(_ outlineView: NSOutlineView, pasteboardWriterForItem item: Any) -> NSPasteboardWriting? {
        guard let box = item as? TabBox, let w = box.tab.controller?.window else { return nil }
        let pb = NSPasteboardItem()
        pb.setString(String(w.windowNumber), forType: Self.dragType)
        return pb
    }

    func outlineView(_ outlineView: NSOutlineView, validateDrop info: NSDraggingInfo, proposedItem item: Any?,
                     proposedChildIndex index: Int) -> NSDragOperation {
        // Drop between rows (not onto one).
        index == NSOutlineViewDropOnItemIndex ? [] : .move
    }

    func outlineView(_ outlineView: NSOutlineView, acceptDrop info: NSDraggingInfo, item: Any?,
                     childIndex index: Int) -> Bool {
        guard let s = info.draggingPasteboard.string(forType: Self.dragType), let number = Int(s),
              let moving = NSApp.window(withWindowNumber: number),
              let group = controller?.window?.tabGroup else { return false }
        // Rows of the drop's group in order, and the tab that ends up right after the dropped one.
        let rows: [TabBox] = (item as? SidebarGroup)?.boxes ?? (showHeaders ? [] : groups.first?.boxes ?? [])
        let all = rows.compactMap { $0.tab.controller?.window }
        let windows = all.filter { $0 !== moving }
        // `index` counts the dragged row too when it comes from above the drop point.
        let from = all.firstIndex { $0 === moving }
        let adjusted = from.map { $0 < index ? index - 1 : index } ?? index
        let clamped = max(0, min(adjusted, windows.count))
        if clamped < windows.count {
            windows[clamped].addTabbedWindow(moving, ordered: .below)
        } else if let last = windows.last {
            last.addTabbedWindow(moving, ordered: .above)
        } else {
            return false
        }
        moving.makeKeyAndOrderFront(nil)
        _ = group
        SessionManager.shared.scheduleLayoutSave()
        SessionManager.shared.refreshSidebars()
        return true
    }

    // MARK: NSOutlineViewDelegate

    func outlineView(_ outlineView: NSOutlineView, isGroupItem item: Any) -> Bool {
        item is SidebarGroup
    }

    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        item is TabBox
    }

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat {
        if item is SidebarGroup { return 24 }
        return 40
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        if let g = item as? SidebarGroup {
            let label = NSTextField(labelWithString: g.name)
            label.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
            label.textColor = .secondaryLabelColor
            label.lineBreakMode = .byTruncatingTail
            return label
        }
        guard let box = item as? TabBox else { return nil }
        let cell = outlineView.makeView(withIdentifier: TabRowView.identifier, owner: nil) as? TabRowView
            ?? TabRowView()
        cell.configure(box.tab)
        return cell
    }

    // MARK: Actions

    @objc private func rowClicked(_ sender: Any?) {
        let row = outline.clickedRow >= 0 ? outline.clickedRow : outline.selectedRow
        guard row >= 0, let box = outline.item(atRow: row) as? TabBox,
              let w = box.tab.controller?.window else { return }
        w.makeKeyAndOrderFront(nil)
        if let view = box.tab.controller?.content.focusedView {
            w.makeFirstResponder(view)
        }
    }

    private func rowMenu() -> NSMenu {
        let menu = NSMenu()
        let close = NSMenuItem(title: "Close Tab", action: #selector(closeClicked(_:)), keyEquivalent: "")
        close.target = self
        menu.addItem(close)
        let move = NSMenuItem(title: "Move Tab to New Workspace", action: #selector(moveClicked(_:)), keyEquivalent: "")
        move.target = self
        menu.addItem(move)
        return menu
    }

    private func clickedTab() -> TerminalWindowController? {
        let row = outline.clickedRow
        guard row >= 0, let box = outline.item(atRow: row) as? TabBox else { return nil }
        return box.tab.controller
    }

    @objc private func closeClicked(_ sender: Any?) {
        clickedTab()?.window?.performClose(nil)
    }

    @objc private func moveClicked(_ sender: Any?) {
        guard let c = clickedTab() else { return }
        SessionManager.shared.moveTabToNewWorkspace(c)
    }
}

/// The agents panel at the bottom of the sidebar: every pane running an agent, wherever it is.
/// Its selection is the focused agent pane, independent of the tab list's selection.
final class AgentsPanel: NSView, NSTableViewDataSource, NSTableViewDelegate {
    weak var controller: TerminalWindowController?

    private let separator = NSBox()
    private let header = NSTextField(labelWithString: "Agents")
    private let summary = NSTextField(labelWithString: "")
    private let scroll = NSScrollView()
    private let table = NSTableView()
    private var rows: [AgentBox] = []
    private let handle = DragHandle()
    private var height: NSLayoutConstraint!
    private static let rowHeight: CGFloat = 40
    private static let headerHeight: CGFloat = 30
    /// Kept for the tab list when the panel is dragged taller.
    private static let minTabsHeight: CGFloat = 160
    /// Rows shown before the panel scrolls: `window.sidebar_agent_rows`, or where the header
    /// is being dragged to.
    private var visibleRows: Int { draggedRows ?? SessionManager.shared.config.sidebarAgentRows }
    private var draggedRows: Int?
    private var shownLimit = 0

    init() {
        super.init(frame: .zero)
        separator.boxType = .separator
        header.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
        header.textColor = .secondaryLabelColor
        summary.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        summary.textColor = .tertiaryLabelColor
        summary.alignment = .right

        let column = NSTableColumn(identifier: .init("agent"))
        table.addTableColumn(column)
        table.headerView = nil
        table.style = .sourceList
        table.rowSizeStyle = .custom
        table.rowHeight = Self.rowHeight
        table.intercellSpacing = NSSize(width: 0, height: 0)
        table.backgroundColor = .clear
        table.dataSource = self
        table.delegate = self
        table.target = self
        table.action = #selector(rowClicked(_:))
        scroll.documentView = table
        scroll.drawsBackground = false
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true

        handle.toolTip = "Drag to show more or fewer agents"
        var start = 0
        handle.began = { [weak self] in start = self?.visibleRows ?? 0 }
        handle.dragged = { [weak self] dy in
            guard let self else { return }
            let available = (self.superview?.bounds.height ?? 0) - Self.headerHeight - Self.minTabsHeight
            let fit = max(1, Int(available / Self.rowHeight), start)
            self.draggedRows = min(fit, max(1, start + Int((dy / Self.rowHeight).rounded())))
            self.updateHeight()
        }
        handle.ended = { [weak self] in
            guard let self, let rows = self.draggedRows else { return }
            if rows != SessionManager.shared.config.sidebarAgentRows {
                _ = Core.shared.request(object: ["SetSetting": ["key": "window.sidebar_agent_rows", "value": String(rows)]])
            }
            // The setting takes over when the config reloads.
            SessionManager.shared.config.sidebarAgentRows = rows
            self.draggedRows = nil
        }

        for v in [separator, header, summary, scroll, handle] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        height = heightAnchor.constraint(equalToConstant: 0)
        NSLayoutConstraint.activate([
            height,
            separator.topAnchor.constraint(equalTo: topAnchor),
            separator.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            separator.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            header.topAnchor.constraint(equalTo: separator.bottomAnchor, constant: 8),
            header.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 16),
            summary.firstBaselineAnchor.constraint(equalTo: header.firstBaselineAnchor),
            summary.leadingAnchor.constraint(greaterThanOrEqualTo: header.trailingAnchor, constant: 8),
            summary.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -14),
            scroll.topAnchor.constraint(equalTo: topAnchor, constant: Self.headerHeight),
            scroll.leadingAnchor.constraint(equalTo: leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: bottomAnchor),
            handle.topAnchor.constraint(equalTo: topAnchor),
            handle.leadingAnchor.constraint(equalTo: leadingAnchor),
            handle.trailingAnchor.constraint(equalTo: trailingAnchor),
            handle.heightAnchor.constraint(equalToConstant: Self.headerHeight),
        ])
        isHidden = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func reload() {
        let next = agentRows()
        if next.count == rows.count, shownLimit == visibleRows,
           zip(next, rows).allSatisfy({ $0.pane == $1.pane && $0.hiddenWorkspace == $1.hiddenWorkspace && $0.row == $1.row }) {
            // A click selects its row even when it changes nothing shown: put the selection back.
            let current = rows.firstIndex { $0.row.selected }
            if table.selectedRow != (current ?? -1) {
                if let i = current { table.selectRowIndexes([i], byExtendingSelection: false) } else { table.deselectAll(nil) }
            }
            return
        }
        rows = next
        isHidden = rows.isEmpty
        let waiting = rows.filter { $0.row.status == .needsInput }.count
        let working = rows.filter { $0.row.status == .working }.count
        summary.stringValue = [waiting > 0 ? "\(waiting) waiting" : nil, working > 0 ? "\(working) working" : nil]
            .compactMap { $0 }.joined(separator: " · ")
        table.reloadData()
        updateHeight()
        if let i = rows.firstIndex(where: { $0.row.selected }) {
            table.selectRowIndexes([i], byExtendingSelection: false)
            table.scrollRowToVisible(i)
        } else {
            table.deselectAll(nil)
        }
    }

    private func updateHeight() {
        shownLimit = visibleRows
        // The source list style pads below the last row: size to the table itself, or the
        // rows don't fit and scrolling to the selected one cuts off the first. Its intrinsic
        // height, not its frame: the frame stretches to fill the scroll view, so after the panel
        // had more agents it would count the empty space as padding and never shrink.
        let visible = min(rows.count, shownLimit)
        let padding = rows.isEmpty ? 0 : max(0, table.intrinsicContentSize.height - table.rect(ofRow: rows.count - 1).maxY)
        let content = visible == 0 ? 0 : table.rect(ofRow: visible - 1).maxY + padding
        height.constant = rows.isEmpty ? 0 : Self.headerHeight + content
    }

    /// Every agent pane: this window's tabs first, then other windows, then hidden workspaces.
    private func agentRows() -> [AgentBox] {
        let key = NSApp.keyWindow?.windowController as? TerminalWindowController
        return SessionManager.shared.agentPanes(first: controller).map { a in
            // Where it runs first (the repository or directory, and the workspace when hidden),
            // so a long status is what gets cut.
            let row = SidebarTab(controller: a.controller,
                                 title: a.title,
                                 subtitle: [a.hostLabel, a.place, a.hidden?.name, a.statusDetail].compactMap { $0 }
                                     .joined(separator: " · "),
                                 status: a.agent.status,
                                 shortcut: nil,
                                 selected: a.controller != nil && a.controller === key
                                     && a.controller?.focusedPane == a.pane)
            return AgentBox(pane: a.pane, hiddenWorkspace: a.hidden?.id, row: row)
        }
    }

    // MARK: NSTableViewDataSource / Delegate

    func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

    func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
        let cell = tableView.makeView(withIdentifier: TabRowView.identifier, owner: nil) as? TabRowView
            ?? TabRowView()
        cell.configure(rows[row].row, truncateTail: true)
        return cell
    }

    @objc private func rowClicked(_ sender: Any?) {
        let row = table.clickedRow
        guard row >= 0, row < rows.count else { return }
        focus(rows[row])
    }

    /// Brings the agent's pane forward: its window and tab, its split, its workspace.
    private func focus(_ agent: AgentBox) {
        let sm = SessionManager.shared
        guard let a = sm.agentPanes(first: controller).first(where: { $0.pane == agent.pane }) else { return }
        sm.focusAgent(a, from: controller)
    }

}

/// The Cmd+N of the tab at `index` (in display order): 1…8, and 9 for the last tab.
func tabShortcut(index: Int, count: Int) -> Int? {
    if index < 8 { return index + 1 }
    return index == count - 1 ? 9 : nil
}

/// "~/dev/personal/thurm" → "~/d/p/thurm".
func abbreviatePath(_ path: String) -> String {
    var p = path
    let home = NSHomeDirectory()
    if p == home { return "~" }
    if p.hasPrefix(home + "/") { p = "~" + p.dropFirst(home.count) }
    var parts = p.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
    guard parts.count > 2 else { return p }
    for i in 0..<(parts.count - 1) where parts[i].count > 1 && parts[i] != "~" {
        parts[i] = String(parts[i].prefix(parts[i].hasPrefix(".") ? 2 : 1))
    }
    return parts.joined(separator: "/")
}

/// One sidebar row: status dot, title over branch/diff (or directory), Cmd+N hint.
final class TabRowView: NSTableCellView {
    static let identifier = NSUserInterfaceItemIdentifier("TabRow")

    private let dot = NSView()
    private let title = NSTextField(labelWithString: "")
    private let subtitle = NSTextField(labelWithString: "")
    private let shortcut = NSTextField(labelWithString: "")

    init() {
        super.init(frame: .zero)
        identifier = Self.identifier
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 4
        title.font = .systemFont(ofSize: 13)
        title.lineBreakMode = .byTruncatingTail
        subtitle.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        subtitle.textColor = .secondaryLabelColor
        subtitle.lineBreakMode = .byTruncatingMiddle
        subtitle.maximumNumberOfLines = 1
        title.maximumNumberOfLines = 1
        shortcut.font = .systemFont(ofSize: 11)
        shortcut.textColor = .tertiaryLabelColor
        for v in [dot, title, subtitle, shortcut] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        subtitle.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        shortcut.setContentHuggingPriority(.required, for: .horizontal)
        NSLayoutConstraint.activate([
            dot.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
            dot.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),
            title.leadingAnchor.constraint(equalTo: dot.trailingAnchor, constant: 8),
            title.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            title.trailingAnchor.constraint(lessThanOrEqualTo: shortcut.leadingAnchor, constant: -6),
            subtitle.leadingAnchor.constraint(equalTo: title.leadingAnchor),
            subtitle.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 1),
            subtitle.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -8),
            shortcut.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            shortcut.centerYAnchor.constraint(equalTo: title.centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func configure(_ tab: SidebarTab, truncateTail: Bool = false) {
        subtitle.lineBreakMode = truncateTail ? .byTruncatingTail : .byTruncatingMiddle
        title.stringValue = tab.title
        title.font = .systemFont(ofSize: 13, weight: tab.selected ? .medium : .regular)
        shortcut.stringValue = tab.shortcut.map { "⌘\($0)" } ?? ""
        if let branch = tab.branch {
            // Attributed strings don't inherit the field's line break mode: keep one line, and
            // shorten the branch rather than the diff counts.
            let para = NSMutableParagraphStyle()
            para.lineBreakMode = .byTruncatingMiddle
            let base: [NSAttributedString.Key: Any] = [.foregroundColor: NSColor.secondaryLabelColor,
                                                      .font: subtitle.font!, .paragraphStyle: para]
            let s = NSMutableAttributedString(string: branch, attributes: base)
            if tab.added > 0 {
                s.append(NSAttributedString(string: "  +\(tab.added)",
                                            attributes: base.merging([.foregroundColor: NSColor.systemGreen]) { $1 }))
            }
            if tab.removed > 0 {
                s.append(NSAttributedString(string: " −\(tab.removed)",
                                            attributes: base.merging([.foregroundColor: NSColor.systemRed]) { $1 }))
            }
            subtitle.attributedStringValue = s
        } else {
            // The directory, unless the title already says the same.
            let dir = tab.subtitle ?? ""
            subtitle.stringValue = dir == tab.title || tab.title.hasSuffix(dir) ? "" : dir
        }
        let color: NSColor
        switch tab.status {
        case .some(.working): color = .systemBlue
        case .some(.needsInput): color = .systemOrange
        case .some(.done): color = .systemGreen
        case .some(.error): color = .systemRed
        case .some(.idle): color = .systemGray
        case .none: color = .clear
        }
        dot.layer?.backgroundColor = color.cgColor
        toolTip = [tab.title, tab.subtitle].compactMap { $0 }.joined(separator: "\n")
    }
}

/// The agents panel's header: dragging it up shows more agents, down fewer.
private final class DragHandle: NSView {
    var began: () -> Void = {}
    /// Points moved up since the drag began.
    var dragged: (CGFloat) -> Void = { _ in }
    var ended: () -> Void = {}
    private var startY: CGFloat = 0

    override func resetCursorRects() {
        addCursorRect(bounds, cursor: .resizeUpDown)
    }

    override func mouseDown(with event: NSEvent) {
        startY = event.locationInWindow.y
        began()
    }

    override func mouseDragged(with event: NSEvent) {
        dragged(event.locationInWindow.y - startY)
    }

    override func mouseUp(with event: NSEvent) {
        ended()
    }
}
