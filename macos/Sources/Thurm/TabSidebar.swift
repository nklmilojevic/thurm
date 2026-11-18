import AppKit

// Vertical tabs (config `window.tab_style = "sidebar"`), after tty7's sidebar: tabs grouped by
// git repository (or by workspace, listing every workspace's tabs), each row showing the agent
// status, the title and the branch with its diff size. Tabs are still native window tabs
// underneath (one NSWindow per tab, with the native tab bar suppressed), so restore, Cmd+1…9
// and "Move Tab to New Workspace" keep working; every window of a tab group shows the same list.
// With only active tabs listed (`window.sidebar_active_only`), the list shows every agent that
// is not idle instead, in any window or workspace, one row per pane under its status, so an
// agent in an unfocused split is one click away.

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
    var trailingDetail: String? = nil
    var selected: Bool
    var icon: PaneIcon = .shell
    /// The tab's other panes, stacked behind its icon.
    var behind: [PaneIcon] = []
    /// The bar under the title (OSC 9;4).
    var progress: ProgressReport?
    /// A tab of a workspace no window shows, which a click switches to.
    var hidden: HiddenTab?
    /// Something is going on in it (see `TabSidebarViewController.isActive`).
    var active = false
    /// The agent pane the row stands for, in the activity list; nil for a tab's row.
    var pane: PaneKey?

    /// Whether the two differ in their progress only: the row then moves its bar and nothing else.
    func differsInProgressOnly(from other: SidebarTab) -> Bool {
        var same = other
        same.progress = progress
        return same == self && other.progress != progress
    }
}

extension SidebarTab: Equatable {
    static func == (a: SidebarTab, b: SidebarTab) -> Bool {
        a.controller === b.controller && a.title == b.title && a.subtitle == b.subtitle && a.branch == b.branch
            && a.added == b.added && a.removed == b.removed && a.status == b.status
            && a.shortcut == b.shortcut && a.trailingDetail == b.trailingDetail && a.selected == b.selected && a.icon == b.icon
            && a.behind == b.behind && a.progress == b.progress && a.hidden == b.hidden && a.active == b.active
            && a.pane == b.pane
    }
}

/// A tab of a workspace no window shows: its `hiddenTabs[index]`.
struct HiddenTab: Equatable {
    let workspace: UInt64
    let index: Int
}

/// Tabs sharing a repository (or "Terminals" for the rest), or a workspace's tabs.
final class SidebarGroup {
    let name: String
    /// What its collapsed state is kept under: the name, or for a workspace its id.
    let key: String
    let elsewhere: Bool
    let workspace: UInt64?
    var tabs: [SidebarTab] = []
    /// Row objects, built once per reload (NSOutlineView tracks items by identity).
    var boxes: [TabBox] = []
    /// A host's header, in the list by host (`window.sidebar_by_host`): its workspaces' groups
    /// are under it, not tabs.
    let host: HostId?
    /// The host's connection state while it is not connected, after its name.
    var state: String?
    var subgroups: [SidebarGroup] = []
    /// A connected host without a workspace: the row that opens one.
    var newWorkspace: NewWorkspaceBox?
    init(name: String, key: String? = nil, elsewhere: Bool = false, workspace: UInt64? = nil, host: HostId? = nil) {
        self.name = name
        self.key = key ?? name
        self.elsewhere = elsewhere
        self.workspace = workspace
        self.host = host
    }

    /// Listed without its header: its rows stand in its parent's place. A host's only
    /// workspace, or the one host the list is narrowed to (the top of the sidebar names it).
    var headless = false

    /// What the outline lists under it.
    var children: [AnyObject] {
        host == nil ? boxes : subgroups.flatMap(\.shown) + (newWorkspace.map { [$0] } ?? [])
    }

    /// What stands for it in its parent: itself, or without a header what it lists.
    var shown: [AnyObject] { headless ? children : [self] }

    /// Itself and the groups under it, headers first.
    var flattened: [SidebarGroup] { [self] + subgroups.flatMap(\.flattened) }
}

/// "New Workspace on <host>", under a connected host that has none.
final class NewWorkspaceBox {
    let host: HostId
    init(host: HostId) { self.host = host }

    var title: String { host == localHost ? "New Workspace" : "New Workspace on \(host)" }
}

/// The list by host (`window.sidebar_by_host`): which hosts it shows, in what order, and what
/// each lists, before any row is made. Nothing in it depends on which workspace a window shows
/// (but for a host that is not connected), so switching tabs or workspaces moves only the
/// selection.
enum SidebarHosts {
    struct Section: Equatable {
        let host: HostId
        /// Its connection state while it is not connected (it then lists nothing).
        var state: String?
        /// Its workspaces, in the order they were made.
        var workspaces: [UInt64]
        /// Connected without a workspace: a row opens one.
        var offersNewWorkspace: Bool
    }

    static func title(_ host: HostId) -> String { host == localHost ? "This Mac" : host }

    /// This Mac, then the `[[remote]]` hosts in config order, then the hosts of workspaces
    /// whose remote is gone from the config.
    static func hosts(remotes: [HostId], workspaceHosts: [HostId]) -> [HostId] {
        var hosts = [localHost]
        for h in remotes + workspaceHosts where !hosts.contains(h) { hosts.append(h) }
        return hosts
    }

    /// The one host listed: `filter` when it is one of `hosts`, else every host (nil).
    static func effectiveFilter(_ filter: HostId?, hosts: [HostId]) -> HostId? {
        filter.flatMap { hosts.contains($0) ? $0 : nil }
    }

    /// `state` is nil for a connected host. A host that is not connected lists none of its
    /// workspaces but the one `shown`, so the selected tab stays in sight.
    static func sections(remotes: [HostId], workspaces: [(id: UInt64, host: HostId)], shown: UInt64?,
                         filter: HostId?, state: (HostId) -> String?) -> [Section] {
        var hosts = Self.hosts(remotes: remotes, workspaceHosts: workspaces.map(\.host))
        if let only = effectiveFilter(filter, hosts: hosts) { hosts = [only] }
        return hosts.map { h in
            let state = h == localHost ? nil : state(h)
            var ids = workspaces.filter { $0.host == h }.map(\.id)
            if state != nil { ids = ids.filter { $0 == shown } }
            return Section(host: h, state: state, workspaces: ids, offersNewWorkspace: state == nil && ids.isEmpty)
        }
    }
}

final class TabBox {
    var tab: SidebarTab
    /// The group it is listed under, also when that group shows no header.
    weak var group: SidebarGroup?
    init(_ tab: SidebarTab, group: SidebarGroup? = nil) {
        self.tab = tab
        self.group = group
    }
}

/// Split view hosting the sidebar next to the tab's split tree.
final class TabSplitViewController: NSSplitViewController {
    /// New windows open with the sidebar the way it was last toggled (Cmd+B).
    private static var collapsedByDefault = false

    let sidebar = TabSidebarViewController()
    private let host = NSViewController()
    /// `window.sidebar_width` as last applied, so a reload moves the divider only when it changed.
    private var configuredWidth: CGFloat
    private var collapseObservation: NSKeyValueObservation?

    init(content: TabContentView, width: CGFloat) {
        configuredWidth = width
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
        // Also follows divider gestures and the state copied when switching tabs/workspaces.
        collapseObservation = side.observe(\.isCollapsed) { [weak self] _, _ in
            self?.syncSidebarToolbar()
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func viewDidAppear() {
        super.viewDidAppear()
        syncSidebarToolbar()
        // NSSplitViewItem has no initial-thickness API; place the divider once.
        if !sidebarCollapsed { placeDivider() }
    }

    private func placeDivider() {
        if let w = sidebar.preferredWidth {
            splitView.setPosition(w, ofDividerAt: 0)
            sidebar.preferredWidth = nil
        }
    }

    /// After a config reload: a new `window.sidebar_width` moves the divider (or, while the
    /// sidebar is collapsed, is where it opens).
    func applyConfiguredWidth(_ width: CGFloat) {
        guard abs(width - configuredWidth) >= 1 else { return }
        configuredWidth = width
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(saveWidth), object: nil)
        sidebar.preferredWidth = width
        if !sidebarCollapsed && !togglingSidebar { placeDivider() }
    }

    var sidebarCollapsed: Bool {
        splitViewItems.first?.isCollapsed ?? false
    }

    private func syncSidebarToolbar() {
        guard !togglingSidebar else { return }
        (view.window as? TerminalWindow)?.sidebarCollapsed = sidebarCollapsed
    }

    /// Before this window is shown in place of `other`'s (a new tab, a workspace switch): the
    /// sidebar as open, as wide and as far scrolled as there, its rows built, so the swap changes
    /// nothing in it but the selection.
    func match(_ other: TabSplitViewController?) {
        if let other {
            splitViewItems.first?.isCollapsed = other.sidebarCollapsed
            view.layoutSubtreeIfNeeded()
            // A sidebar not laid out yet (a window restored a moment ago) has no width to copy.
            let width = other.sidebar.view.frame.width
            if !other.sidebarCollapsed, width >= splitViewItems[0].minimumThickness {
                splitView.setPosition(width, ofDividerAt: 0)
                sidebar.preferredWidth = nil
            }
        }
        sidebar.reload()
        view.layoutSubtreeIfNeeded()
        if let other { sidebar.scrollOffset = other.sidebar.scrollOffset }
    }

    func toggleSidebarAnimated() {
        guard !togglingSidebar, let side = splitViewItems.first else { return }
        let collapse = !side.isCollapsed
        Self.collapsedByDefault = collapse
        let panes = (host.view as? TabContentView)?.holdGridsForSidebar() ?? []
        sidebar.holdReloads = true
        togglingSidebar = true
        NSObject.cancelPreviousPerformRequests(withTarget: self, selector: #selector(saveWidth), object: nil)
        // Remove the far-edge button before closing. When opening, restore it only once the
        // sidebar has room for it, so it never flashes into the overflow menu mid-animation.
        if collapse { (view.window as? TerminalWindow)?.sidebarCollapsed = true }
        NSAnimationContext.runAnimationGroup({ ctx in
            ctx.duration = Motion.duration(Motion.standard)
            ctx.timingFunction = Motion.timingFunction
            side.animator().isCollapsed = collapse
        }, completionHandler: { [weak self] in
            if !collapse { self?.placeDivider() }
            self?.togglingSidebar = false
            self?.syncSidebarToolbar()
            self?.view.layoutSubtreeIfNeeded()
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
        configuredWidth = w
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
    private var shown: (names: [[String]], tabs: [[SidebarTab]], theme: Theme)?

    private let outline = SidebarOutlineView()
    private let dropLine: NSView = {
        let line = NSView()
        line.wantsLayer = true
        line.isHidden = true
        return line
    }()
    private var groups: [SidebarGroup] = []
    static let dragType = NSPasteboard.PasteboardType("com.thurm.sidebar-tab")
    /// Keys of the groups the user collapsed (kept across reloads and launches).
    private static var collapsed: Set<String> {
        get { Set(UserDefaults.standard.stringArray(forKey: "SidebarCollapsedGroups") ?? []) }
        set { UserDefaults.standard.set(Array(newValue), forKey: "SidebarCollapsedGroups") }
    }
    /// Headers only when there is more than one group.
    private var showHeaders = false
    /// What the outline lists at its top level.
    private var rootItems: [AnyObject] = []
    /// Where the window's workspace runs ("This Mac" or the host); click for the workspace picker.
    private let workspaceButton = NSButton(title: "", target: nil, action: nil)
    private let hostIcon = NSImageView()
    /// Lists every host's tabs, or one host's (`window.sidebar_by_host`).
    private let hostsButton = NSButton(title: "", target: nil, action: nil)
    /// Set while expanding and collapsing the groups after a reload, which must not change
    /// what the user collapsed.
    private var restoringExpansion = false
    private let usage = UsagePanel()
    /// Moves the agents' "updated 20s ago" on while the activity list shows.
    private var ageTimer: Timer?

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
        // The row view draws the selection (see `SidebarRowView`): the source list draws its own
        // over any other.
        outline.selectionHighlightStyle = .none
        outline.rowSizeStyle = .custom
        outline.floatsGroupRows = false
        outline.indentationPerLevel = 0
        outline.backgroundColor = .clear
        outline.dataSource = self
        outline.delegate = self
        outline.target = self
        outline.action = #selector(rowClicked(_:))
        outline.activate = { [weak self] in
            guard let self, self.outline.selectedRow >= 0 else { return }
            self.activate(row: self.outline.selectedRow)
        }
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
        hostIcon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        hostIcon.contentTintColor = .secondaryLabelColor
        hostIcon.translatesAutoresizingMaskIntoConstraints = false
        hostsButton.target = self
        hostsButton.action = #selector(showHostsPopover(_:))
        hostsButton.isBordered = false
        hostsButton.symbolConfiguration = .init(pointSize: 12, weight: .regular)
        hostsButton.contentTintColor = .secondaryLabelColor
        hostsButton.toolTip = "Hosts"
        hostsButton.setAccessibilityLabel("Hosts")
        hostsButton.translatesAutoresizingMaskIntoConstraints = false

        let root = NSView()
        root.wantsLayer = true
        root.layer?.backgroundColor = backgroundColor?.cgColor
        scroll.translatesAutoresizingMaskIntoConstraints = false
        root.addSubview(hostIcon)
        root.addSubview(workspaceButton)
        root.addSubview(hostsButton)
        root.addSubview(scroll)
        root.addSubview(usage)
        usage.translatesAutoresizingMaskIntoConstraints = false
        NSLayoutConstraint.activate([
            workspaceButton.topAnchor.constraint(equalTo: root.safeAreaLayoutGuide.topAnchor, constant: 2),
            // The icon starts where the headings do, the name where a host's header names it.
            hostIcon.leadingAnchor.constraint(equalTo: root.leadingAnchor, constant: SidebarHeadingView.inset),
            hostIcon.widthAnchor.constraint(equalToConstant: SidebarHeadingView.hostLabelOffset - 5),
            hostIcon.centerYAnchor.constraint(equalTo: workspaceButton.centerYAnchor),
            workspaceButton.leadingAnchor.constraint(equalTo: hostIcon.trailingAnchor, constant: 5),
            workspaceButton.trailingAnchor.constraint(lessThanOrEqualTo: hostsButton.leadingAnchor, constant: -6),
            hostsButton.trailingAnchor.constraint(equalTo: root.trailingAnchor, constant: -12),
            hostsButton.centerYAnchor.constraint(equalTo: workspaceButton.centerYAnchor),
            scroll.topAnchor.constraint(equalTo: workspaceButton.bottomAnchor, constant: 4),
            scroll.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            scroll.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            scroll.bottomAnchor.constraint(equalTo: usage.topAnchor),
            usage.leadingAnchor.constraint(equalTo: root.leadingAnchor),
            usage.trailingAnchor.constraint(equalTo: root.trailingAnchor),
            usage.bottomAnchor.constraint(equalTo: root.bottomAnchor, constant: -8),
        ])
        view = root
        let timer = Timer(timeInterval: 1, repeats: true) { [weak self] _ in
            guard let self, SessionManager.shared.config.sidebarActiveOnly,
                  self.view.window?.isVisible == true, !self.view.isHiddenOrHasHiddenAncestor else { return }
            self.refreshAges()
        }
        RunLoop.main.add(timer, forMode: .common)
        ageTimer = timer
    }

    deinit { ageTimer?.invalidate() }

    /// Moves the agent rows' "updated 20s ago" on. Only the shown rows, and only their age:
    /// a status or membership change comes with a full reload.
    func refreshAges(now: Date = Date()) {
        let panes = SessionManager.shared.panes
        let visible = outline.rows(in: outline.visibleRect)
        for row in visible.location..<NSMaxRange(visible) {
            guard let box = outline.item(atRow: row) as? TabBox, let pane = box.tab.pane,
                  let info = panes[pane], let agent = info.agent else { continue }
            let age = AgentPane(pane: pane, info: info, agent: agent, controller: nil, hidden: nil).activityAge(now: now)
            guard age != box.tab.trailingDetail else { continue }
            box.tab.trailingDetail = age
            (outline.view(atColumn: 0, row: row, makeIfNecessary: false) as? TabRowView)?
                .configure(box.tab, truncateTail: true)
        }
    }

    // MARK: Model

    /// Opens the same picker as the native tab strip's workspace button.
    @objc private func showWorkspaceMenu(_ sender: NSButton) {
        WorkspacePicker.shared.toggle(relativeTo: sender)
    }

    @objc private func showHostsPopover(_ sender: NSButton) {
        SidebarHostsPopover.shared.toggle(relativeTo: sender)
    }

    /// The top row: where the window's workspace runs, and the hosts button (hidden while there
    /// is no other host to list and the list by host is off).
    /// Listed by host, it names what is listed (every host, or the one host and its state) and
    /// opens the hosts choice in place of the workspace picker, as every workspace is listed.
    private func syncTopRow(_ shown: Workspace?, config: AppConfig) {
        var title: String, symbol: String, tip: String, action: Selector
        if config.sidebarByHost {
            let sm = SessionManager.shared
            let hosts = SidebarHosts.hosts(remotes: config.remoteNames, workspaceHosts: sm.workspaces.map(\.host))
            let only = SidebarHosts.effectiveFilter(config.sidebarHost, hosts: hosts)
            title = only.map(SidebarHosts.title) ?? "All Hosts"
            if let state = only.flatMap(Self.hostState) { title += " · \(state)" }
            symbol = only.map { $0 == localHost ? "laptopcomputer" : "globe" } ?? "square.stack.3d.up"
            tip = "Hosts"
            action = #selector(showHostsPopover(_:))
        } else {
            let host = shown?.host ?? controller?.host ?? localHost
            title = SidebarHosts.title(host)
            symbol = host == localHost ? "laptopcomputer" : "globe"
            tip = shown.map { "Workspace \($0.name) · Workspaces (⌘⇧O)" } ?? "Workspaces (⌘⇧O)"
            action = #selector(showWorkspaceMenu(_:))
        }
        if workspaceButton.title != title { workspaceButton.title = title }
        if workspaceButton.toolTip != tip { workspaceButton.toolTip = tip }
        workspaceButton.action = action
        if hostIcon.image?.accessibilityDescription != symbol {
            hostIcon.image = NSImage(systemSymbolName: symbol, accessibilityDescription: symbol)
        }
        hostsButton.isHidden = config.remoteNames.isEmpty || config.sidebarByHost
        if hostsButton.image == nil {
            hostsButton.image = NSImage(systemSymbolName: "square.stack.3d.up", accessibilityDescription: "Hosts")
        }
    }

    /// How far the tab list is scrolled.
    var scrollOffset: NSPoint {
        get { outline.enclosingScrollView?.contentView.bounds.origin ?? .zero }
        set {
            guard let scroll = outline.enclosingScrollView else { return }
            scroll.contentView.scroll(to: newValue)
            scroll.reflectScrolledClipView(scroll.contentView)
        }
    }

    /// Tabs in the order the sidebar lists them: grouped by repository, the groups in the order
    /// of their first tab, then the rest ("Terminals"); ungrouped, in tab order. Numbers follow
    /// it, so they can change when a tab closes or moves to another repository.
    static func displayOrder(_ windows: [NSWindow]) -> [NSWindow] {
        let live = windows.filter { ($0.windowController as? TerminalWindowController)?.isClosed == false }
        let (pinned, rest) = pinnedFirst(live) { ($0.windowController as! TerminalWindowController).pinned }
        guard groupsByRepo(SessionManager.shared.config) else { return pinned + rest }
        return pinned + listingOrder(rest) { group(for: $0.windowController as! TerminalWindowController)?.key }
    }

    static func pinnedFirst<T>(_ items: [T], isPinned: (T) -> Bool) -> (pinned: [T], rest: [T]) {
        (items.filter(isPinned), items.filter { !isPinned($0) })
    }

    static let pinnedKey = "pinned"

    /// The activity list's statuses, most urgent first, with their headings.
    static let activityStatuses: [(AgentStatus, String)] = [(.needsInput, "Needs input"), (.error, "Failed"),
                                                            (.done, "Done"), (.working, "Working")]

    /// The activity list's groups after the pinned tabs: the agents' rows under their status, in
    /// the order given (so a status change moves only that row), then the tabs' rows. Empty
    /// groups are left for the caller to drop.
    static func activityGroups(agents: [SidebarTab], tabs: [SidebarTab]) -> [SidebarGroup] {
        var groups = activityStatuses.map { status, name -> SidebarGroup in
            let g = SidebarGroup(name: name, key: "activity:\(status.rawValue)")
            g.tabs = agents.filter { $0.status == status }
            return g
        }
        let rest = SidebarGroup(name: "Tabs", key: "activity:tabs")
        rest.tabs = tabs
        groups.append(rest)
        return groups
    }

    /// An agent's row in the activity list: what it is doing before where it runs, so that stays
    /// visible in a narrow sidebar. `workspace` names a workspace other than this window's.
    static func agentRow(_ a: AgentPane, selected: Bool, workspace: String?) -> SidebarTab {
        var row = SidebarTab(controller: a.controller,
                             title: a.title,
                             subtitle: [a.statusDetail, a.hostLabel, a.place, workspace].compactMap { $0 }
                                 .joined(separator: " · "),
                             status: a.agent.status,
                             shortcut: nil,
                             trailingDetail: a.activityAge(),
                             selected: selected,
                             icon: PaneIcon(a.info))
        row.progress = TerminalView.shownProgress(a.info)
        row.active = true
        row.pane = a.pane
        return row
    }

    /// A tab's own row in the activity list, besides its agents' rows: when a pane without an
    /// agent row reports progress (the bar then is that pane's), or when it is the selected tab
    /// and its focused pane has no agent row. Nil when it has no row.
    static func activityTabRow(_ tab: SidebarTab, panes: [PaneKey], focused: PaneKey, listed: Set<PaneKey>,
                               info: [PaneKey: PaneInfo]) -> SidebarTab? {
        let rest = panes.filter { !listed.contains($0) }
        var row = tab
        row.progress = (rest.contains(focused) ? TerminalView.shownProgress(info[focused]) : nil)
            ?? rest.lazy.compactMap { TerminalView.shownProgress(info[$0]) }.first
        guard row.progress != nil || (tab.selected && !listed.contains(focused)) else { return nil }
        return row
    }

    /// The panes of the tab a row shows, and its focused one.
    private static func panes(of tab: SidebarTab) -> (ids: [PaneKey], focused: PaneKey)? {
        if let c = tab.controller { return (c.content.paneIds, c.focusedPane) }
        guard let h = tab.hidden, let ws = SessionManager.shared.workspace(h.workspace),
              ws.hiddenTabs.indices.contains(h.index) else { return nil }
        let t = ws.hiddenTabs[h.index]
        return (t.panes, t.focusedKey)
    }

    /// `window.sidebar_active_only`: what needs a look. Every agent that isn't idle, in any
    /// window or workspace, one row per pane under its status; then the tabs of `listed` that
    /// `activityTabRow` keeps.
    private func activityGroups(listed: [SidebarGroup], shown: Workspace?, host only: HostId?,
                                panes info: [PaneKey: PaneInfo]) -> [SidebarGroup] {
        let sm = SessionManager.shared
        var agents = sm.agentPanes(first: controller).filter { $0.agent.status != .idle }
        if let only { agents = agents.filter { $0.pane.host == only } }
        if sm.config.sidebarByHost {
            // As in the list by host: a host that is not connected lists nothing but the
            // workspace this window shows, as what it last reported is stale.
            agents = agents.filter { a in
                Self.hostState(a.pane.host) == nil || (a.hidden?.id ?? a.controller?.workspaceID) == shown?.id
            }
        }
        if sm.config.sidebarAllWorkspaces || sm.config.sidebarByHost {
            // By host when listed by host, then by workspace in the tab list's order, so a
            // switch moves none.
            let hosts = sm.config.sidebarByHost
                ? SidebarHosts.hosts(remotes: sm.config.remoteNames, workspaceHosts: sm.workspaces.map(\.host)) : []
            let rank = { (a: AgentPane) -> (Int, Int) in
                let ws = a.hidden?.id ?? a.controller?.workspaceID
                return (hosts.firstIndex(of: a.pane.host) ?? 0,
                        sm.workspaces.firstIndex { $0.id == ws } ?? sm.workspaces.count)
            }
            agents = agents.enumerated()
                .sorted {
                    let x = rank($0.element), y = rank($1.element)
                    return (x.0, x.1, $0.offset) < (y.0, y.1, $1.offset)
                }
                .map(\.element)
        }
        let rows = agents.map { a -> SidebarTab in
            let ws = a.hidden ?? a.controller.flatMap { sm.workspace($0.workspaceID) }
            return Self.agentRow(a, selected: a.controller != nil && a.controller === controller
                                     && controller?.focusedPane == a.pane,
                                 workspace: ws.flatMap { $0.id == shown?.id ? nil : $0.name })
        }
        let listedPanes = Set(rows.compactMap(\.pane))
        var tabs: [SidebarTab] = []
        for g in listed {
            for tab in g.tabs {
                guard let (ids, focused) = Self.panes(of: tab),
                      var row = Self.activityTabRow(tab, panes: ids, focused: focused, listed: listedPanes, info: info)
                else { continue }
                if g.elsewhere {
                    let name = g.workspace.flatMap { sm.workspace($0) }.map(Self.heading) ?? g.name
                    row.subtitle = [row.subtitle, name].compactMap { $0 }.joined(separator: " · ")
                }
                tabs.append(row)
            }
        }
        return Self.activityGroups(agents: rows, tabs: tabs)
    }

    private static func pinnedElsewhere(_ tab: SidebarTab, of ws: Workspace) -> SidebarTab {
        var tab = tab
        tab.subtitle = [tab.subtitle, heading(ws)].compactMap { $0 }.joined(separator: " · ")
        return tab
    }

    /// Whether the window's tabs are listed under their repository: every workspace's tabs are
    /// listed under the workspace instead.
    static func groupsByRepo(_ config: AppConfig) -> Bool {
        config.sidebarGroupByRepo && !config.sidebarAllWorkspaces && !config.sidebarByHost
    }

    /// `items` grouped by `key`: the groups in the order of their first item, then the items
    /// without one.
    static func listingOrder<T>(_ items: [T], key: (T) -> String?) -> [T] {
        var keys: [String] = []
        var byKey: [String: [T]] = [:]
        var other: [T] = []
        for item in items {
            if let k = key(item) {
                if byKey[k] == nil { keys.append(k) }
                byKey[k, default: []].append(item)
            } else {
                other.append(item)
            }
        }
        return keys.flatMap { byKey[$0] ?? [] } + other
    }

    /// Whether a tab with these panes has something going on: an agent that isn't idle, or a
    /// program reporting progress. `window.sidebar_active_only` lists only these (and the
    /// selected tab).
    static func isActive(_ panes: [PaneKey], info: [PaneKey: PaneInfo], progress: ProgressReport?) -> Bool {
        progress != nil || panes.contains { info[$0]?.agent.map { $0.status != .idle } ?? false }
    }

    /// The status a tab shows: the most urgent of its panes' agents, among the states that
    /// mark tabs.
    private static func status(_ panes: [PaneKey], info: [PaneKey: PaneInfo], config: AppConfig) -> AgentStatus? {
        var status: AgentStatus?
        for id in panes {
            if let a = info[id]?.agent, config.notice(for: a).tabIndicator,
               status == nil || a.status.urgency > status!.urgency {
                status = a.status
            }
        }
        return status
    }

    /// The row of a window's tab.
    private static func row(_ c: TerminalWindowController, shortcut: Int?, selected: Bool,
                            info panes: [PaneKey: PaneInfo], config: AppConfig) -> SidebarTab {
        let info = panes[c.focusedPane]
        let git = info?.git
        // Remote paths are the host's, not abbreviated against this Mac's home.
        let dir = info?.cwd.map { c.host == localHost ? abbreviatePath($0) : "\(c.host):\($0)" }
        var tab = SidebarTab(controller: c,
                             title: c.titleOverride ?? info?.displayTitle ?? "Thurm",
                             subtitle: dir,
                             status: status(c.content.paneIds, info: panes, config: config),
                             shortcut: shortcut,
                             selected: selected,
                             icon: PaneIcon(info),
                             behind: PaneIcon.behind(c))
        tab.progress = progress(focused: c.focusedPane, panes: c.content.paneIds, info: panes)
        tab.active = isActive(c.content.paneIds, info: panes, progress: tab.progress)
        if let g = git {
            tab.branch = c.host == localHost ? g.branch : "\(c.host) · \(g.branch)"
            tab.added = g.added
            tab.removed = g.removed
        }
        if let branch = handoffBranch(c.handoffID) { tab.branch = branch }
        return tab
    }

    /// A handoff tab's branch line: where it runs, and whether its work came back.
    private static func handoffBranch(_ id: String?) -> String? {
        guard let h = id.flatMap({ Remotes.shared.handoffs[$0] }) else { return nil }
        return "\(h.host) · \(h.branch)" + (Remotes.shared.handoffErrors[h.id] != nil ? " · fetch failed" : "")
    }

    /// The row of `ws`'s `index`th tab, which no window shows: as its window would show it, so
    /// switching to the workspace changes nothing in it.
    private static func hiddenRow(_ t: TabLayout, index: Int, of ws: Workspace,
                                  info panes: [PaneKey: PaneInfo], config: AppConfig) -> SidebarTab {
        let focused = t.focusedKey
        let info = panes[focused]
        let dir = info?.cwd.map { ws.host == localHost ? abbreviatePath($0) : "\(ws.host):\($0)" }
        var tab = SidebarTab(controller: nil,
                             title: t.title ?? info?.displayTitle ?? "Thurm",
                             subtitle: dir,
                             status: status(t.panes, info: panes, config: config),
                             shortcut: nil,
                             selected: false,
                             icon: PaneIcon(info),
                             behind: t.panes.lazy.filter { $0 != focused }.prefix(PaneIcon.maxBehind)
                                 .map { PaneIcon(panes[$0]) })
        tab.progress = progress(focused: focused, panes: t.panes, info: panes)
        tab.active = isActive(t.panes, info: panes, progress: tab.progress)
        if let g = info?.git {
            tab.branch = ws.host == localHost ? g.branch : "\(ws.host) · \(g.branch)"
            tab.added = g.added
            tab.removed = g.removed
        }
        if let branch = handoffBranch(t.handoff) { tab.branch = branch }
        tab.hidden = HiddenTab(workspace: ws.id, index: index)
        return tab
    }

    /// A workspace's header, the same whether this window shows it or not: its name, and a
    /// remote workspace's host.
    static func heading(_ ws: Workspace) -> String {
        ws.isRemote ? "\(ws.name) (\(ws.host))" : ws.name
    }

    /// What a workspace's group keeps its collapsed state under.
    private static func collapseKey(_ ws: Workspace) -> String { "workspace:\(ws.id)" }

    /// The group of a workspace this window does not show, under `name`: its tabs as the window
    /// showing it lists them, or its hidden tabs; pinned tabs are under Pinned.
    private static func workspaceGroup(_ ws: Workspace, name: String, info panes: [PaneKey: PaneInfo],
                                       config: AppConfig) -> SidebarGroup {
        let sm = SessionManager.shared
        let g = SidebarGroup(name: name, key: collapseKey(ws), elsewhere: true, workspace: ws.id)
        if let shownIn = sm.controllerShowing(ws) {
            g.tabs = sm.group(of: shownIn).filter { !$0.pinned }.map {
                row($0, shortcut: nil, selected: false, info: panes, config: config)
            }
        } else {
            g.tabs = ws.hiddenTabs.enumerated().filter { $0.element.pinned != true }.map {
                hiddenRow($0.element, index: $0.offset, of: ws, info: panes, config: config)
            }
        }
        return g
    }

    /// A workspace's name under its host's header: without the host's name in front, which
    /// names the workspaces made on a remote host after its first ("devbox-quiet-otter").
    static func nameUnderHost(_ ws: Workspace) -> String {
        guard ws.isRemote, ws.name.hasPrefix(ws.host + "-"), ws.name.count > ws.host.count + 1 else { return ws.name }
        return String(ws.name.dropFirst(ws.host.count + 1))
    }

    /// Whether the daemon is connected to a remote host (replaced in tests).
    static var isConnected: (HostId) -> Bool = { Core.shared.isConnected($0) }

    /// A host's state on its header in the list by host: nil while it is connected.
    static func hostState(_ host: HostId) -> String? {
        guard host != localHost, !isConnected(host) else { return nil }
        guard SessionManager.shared.config.remoteNames.contains(host) else { return "not configured" }
        return Remotes.shared.phaseLabel(host)
    }

    /// The group a tab is listed under: its repository ("devbox · repo" on a remote host), or
    /// for a handoff tab the local repository it came from.
    static func group(for c: TerminalWindowController) -> (key: String, name: String)? {
        if let h = c.handoffID.flatMap({ Remotes.shared.handoffs[$0] }) {
            return (h.repo, (h.repo as NSString).lastPathComponent)
        }
        return group(for: c.focusedPane)
    }

    static func group(for pane: PaneKey) -> (key: String, name: String)? {
        guard let root = SessionManager.shared.panes[pane]?.git?.root else { return nil }
        let name = (root as NSString).lastPathComponent
        return pane.host == localHost ? (root, name) : ("\(pane.host):\(root)", "\(pane.host) · \(name)")
    }

    func reload() {
        guard isViewLoaded, let window = controller?.window else { return }
        if holdReloads {
            reloadHeld = true
            return
        }
        let sm = SessionManager.shared
        let shownWorkspace = controller.flatMap { sm.workspace($0.workspaceID) }
        let config = sm.config
        syncTopRow(shownWorkspace, config: config)
        usage.reload()
        let windows = controller?.orderedTabs ?? [window]
        let panes = sm.panes
        let byRepo = Self.groupsByRepo(config)
        var byRoot: [String: SidebarGroup] = [:]
        var order: [SidebarGroup] = []
        let pinned = SidebarGroup(name: "Pinned", key: Self.pinnedKey)
        // Every workspace listed: this window's tabs under its workspace (under its host, the
        // workspace's name is enough).
        let other = config.sidebarByHost
            ? SidebarGroup(name: shownWorkspace.map(Self.nameUnderHost) ?? "Tabs", key: shownWorkspace.map(Self.collapseKey))
            : config.sidebarAllWorkspaces
            ? SidebarGroup(name: shownWorkspace.map(Self.heading) ?? "Tabs", key: shownWorkspace.map(Self.collapseKey))
            : SidebarGroup(name: "Terminals")
        for (index, w) in windows.enumerated() {
            guard let c = w.windowController as? TerminalWindowController, !c.isClosed else { continue }
            let tab = Self.row(c, shortcut: tabShortcut(index: index, count: windows.count), selected: w === window,
                               info: panes, config: config)
            if c.pinned {
                pinned.tabs.append(tab)
            } else if byRepo, let (key, name) = Self.group(for: c) {
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
        for ws in sm.workspaces where ws.id != shownWorkspace?.id {
            if let shownIn = sm.controllerShowing(ws) {
                pinned.tabs += sm.group(of: shownIn).filter(\.pinned).map {
                    Self.pinnedElsewhere(Self.row($0, shortcut: nil, selected: false, info: panes, config: config), of: ws)
                }
            } else {
                pinned.tabs += ws.hiddenTabs.enumerated().filter { $0.element.pinned == true }.map {
                    Self.pinnedElsewhere(Self.hiddenRow($0.element, index: $0.offset, of: ws, info: panes, config: config),
                                         of: ws)
                }
            }
        }
        var hostFilter: HostId?
        if config.sidebarByHost {
            let sections = SidebarHosts.sections(remotes: config.remoteNames,
                                                 workspaces: sm.workspaces.map { ($0.id, $0.host) },
                                                 shown: shownWorkspace?.id, filter: config.sidebarHost,
                                                 state: Self.hostState)
            // The one host listed, as the top of the sidebar names it: none for a host that is gone,
            // also when This Mac is the only host left.
            hostFilter = SidebarHosts.effectiveFilter(config.sidebarHost,
                                                      hosts: SidebarHosts.hosts(remotes: config.remoteNames,
                                                                                workspaceHosts: sm.workspaces.map(\.host)))
            if shownWorkspace == nil { order.append(other) }
            for section in sections {
                let g = SidebarGroup(name: SidebarHosts.title(section.host), key: "host:\(section.host)",
                                     host: section.host)
                g.state = section.state
                g.subgroups = section.workspaces.compactMap { id -> SidebarGroup? in
                    if id == shownWorkspace?.id { return other }
                    return sm.workspace(id).map {
                        Self.workspaceGroup($0, name: Self.nameUnderHost($0), info: panes, config: config)
                    }
                }.filter { !$0.tabs.isEmpty }
                if section.offersNewWorkspace { g.newWorkspace = NewWorkspaceBox(host: section.host) }
                // A host's only workspace needs no name of its own, nor its first one named after
                // it (a remote host's first workspace is); nor the one host listed, unless nothing
                // is under it: its header then is its only row, with its state.
                if let first = g.subgroups.first, g.subgroups.count == 1 || first.name == section.host {
                    first.headless = true
                }
                g.headless = hostFilter != nil && (!g.subgroups.isEmpty || g.newWorkspace != nil)
                order.append(g)
            }
        } else if config.sidebarAllWorkspaces {
            // The workspaces in the order they were made, so switching to one moves nothing.
            if !sm.workspaces.contains(where: { $0.id == shownWorkspace?.id }) { order.append(other) }
            for ws in sm.workspaces {
                guard ws.id != shownWorkspace?.id else {
                    order.append(other)
                    continue
                }
                order.append(Self.workspaceGroup(ws, name: Self.heading(ws), info: panes, config: config))
            }
        } else if !other.tabs.isEmpty {
            order.append(other)
        }
        order.insert(pinned, at: 0)
        if config.sidebarActiveOnly {
            let listed = order.flatMap(\.flattened).filter { $0 !== pinned && $0.host == nil }
            order = [pinned] + activityGroups(listed: listed, shown: shownWorkspace, host: hostFilter, panes: panes)
        }
        // A host's header stays without rows: it shows the host's state, or offers a workspace.
        order.removeAll { $0.host == nil && $0.tabs.isEmpty }
        let all = order.flatMap(\.flattened), leaves = all.filter { $0.host == nil }
        let names = all.map { [$0.key, $0.name, $0.state ?? "", $0.newWorkspace?.host ?? "", $0.headless ? "-" : ""] }
        let tabs = leaves.map(\.tabs)
        let theme = SessionManager.shared.config.theme
        if let s = shown, s.names == names, s.theme == theme,
           Self.sameRows(s.tabs.map(Self.identities), tabs.map(Self.identities)) {
            // The same tabs in the same places, so only a title, status or branch changed:
            // reconfigure those rows. reloadData would rebuild every row view, and a working
            // agent retitles its tab several times a second.
            if s.tabs != tabs {
                shown = (names, tabs, theme)
                for (g, new) in zip(groups.flatMap(\.flattened).filter { $0.host == nil }, tabs) {
                    g.tabs = new
                    for (box, tab) in zip(g.boxes, new) where box.tab != tab {
                        // A program reports progress many times a second: move only the bar.
                        let progressOnly = tab.differsInProgressOnly(from: box.tab)
                        box.tab = tab
                        let row = outline.row(forItem: box)
                        guard row >= 0,
                              let view = outline.view(atColumn: 0, row: row, makeIfNecessary: false) as? TabRowView
                        else { continue }
                        if progressOnly {
                            view.setProgress(tab.progress)
                        } else {
                            view.configure(tab, truncateTail: tab.pane != nil)
                        }
                    }
                }
            }
        } else {
            shown = (names, tabs, theme)
            for g in leaves { g.boxes = g.tabs.map { TabBox($0, group: g) } }
            groups = order
            // Every workspace listed: each under its name, also when the others have no tabs to
            // show. The activity list: each status under its name, also when it is the only one.
            showHeaders = config.sidebarActiveOnly || config.sidebarAllWorkspaces || config.sidebarByHost
                || groups.count > 1 || groups.first?.key == Self.pinnedKey
            rootItems = showHeaders ? groups.flatMap(\.shown) : groups.first?.boxes ?? []
            outline.reloadData()
            // Every group open first, so a workspace under a collapsed host keeps its own state.
            let collapsed = Self.collapsed, headed = all.filter { !$0.headless }
            restoringExpansion = true
            for g in headed { outline.expandItem(g) }
            for g in headed.reversed() where collapsed.contains(g.key) { outline.collapseItem(g) }
            restoringExpansion = false
        }
        // Select the row of this window's tab, also when the rows are unchanged: a click on
        // another tab's row selected that row here, and this window's tab is the one shown again.
        // An agent row goes before its tab's row (a pinned tab's, say): it is the pane in focus.
        let selected = (0..<outline.numberOfRows).filter { (outline.item(atRow: $0) as? TabBox)?.tab.selected == true }
        let current = selected.first { (outline.item(atRow: $0) as? TabBox)?.tab.pane != nil } ?? selected.first
        // Not while the keyboard is on the new-workspace row, which Return then opens.
        let onNewWorkspace = outline.window?.firstResponder === outline
            && outline.selectedRow >= 0 && outline.item(atRow: outline.selectedRow) is NewWorkspaceBox
        if let row = current, outline.selectedRow != row, !onNewWorkspace {
            outline.selectRowIndexes([row], byExtendingSelection: false)
        }
    }

    /// The progress a tab's row shows. One bar for the whole tab: the focused pane's, as that is
    /// the pane the tab is named after; else the first other pane (in split order) that reports
    /// one, so a build left running in a split still shows.
    static func progress(focused: PaneKey, panes: [PaneKey], info: [PaneKey: PaneInfo]) -> ProgressReport? {
        TerminalView.shownProgress(info[focused])
            ?? panes.lazy.compactMap { TerminalView.shownProgress(info[$0]) }.first
    }

    /// What a row stands for: the window it shows, the agent pane, or the hidden tab.
    private struct RowID: Equatable {
        let window: ObjectIdentifier?
        let pane: PaneKey?
        let hidden: HiddenTab?
    }

    /// The identity of each row in a group.
    private static func identities(_ group: [SidebarTab]) -> [RowID] {
        group.map { RowID(window: $0.controller.map(ObjectIdentifier.init), pane: $0.pane, hidden: $0.hidden) }
    }

    /// Whether two listings hold the same tabs in the same groups and order, whatever they show.
    static func sameRows<ID: Equatable>(_ a: [[ID]], _ b: [[ID]]) -> Bool {
        a.count == b.count && zip(a, b).allSatisfy { x, y in
            x.count == y.count && zip(x, y).allSatisfy { $0 == $1 }
        }
    }

    // MARK: NSOutlineViewDataSource

    func outlineView(_ outlineView: NSOutlineView, numberOfChildrenOfItem item: Any?) -> Int {
        if item == nil { return rootItems.count }
        if let g = item as? SidebarGroup { return g.children.count }
        return 0
    }

    func outlineView(_ outlineView: NSOutlineView, child index: Int, ofItem item: Any?) -> Any {
        if item == nil { return rootItems[index] }
        return (item as! SidebarGroup).children[index]
    }

    func outlineView(_ outlineView: NSOutlineView, isItemExpandable item: Any) -> Bool {
        item is SidebarGroup
    }

    // Collapsing a repository group sticks.
    func outlineViewItemDidCollapse(_ notification: Notification) {
        guard !restoringExpansion else { return }
        if let g = notification.userInfo?["NSObject"] as? SidebarGroup { Self.collapsed.insert(g.key) }
    }

    func outlineViewItemDidExpand(_ notification: Notification) {
        guard !restoringExpansion else { return }
        if let g = notification.userInfo?["NSObject"] as? SidebarGroup { Self.collapsed.remove(g.key) }
    }

    // MARK: Drag to reorder (moves the native tab)

    /// Whether `w` is one of this window's tabs: the only ones a drag reorders. Another
    /// workspace's (listed with every workspace) would join this window's tab group.
    private func ownsTab(_ w: NSWindow) -> Bool {
        guard let own = controller?.window else { return false }
        return (own.tabGroup?.windows ?? [own]).contains { $0 === w }
    }

    /// The rows follow the tab order, so a drag can reorder tabs: not in the activity list,
    /// whose rows go by status.
    private var reorders: Bool { !SessionManager.shared.config.sidebarActiveOnly }

    /// The tab a drag carries, when it is one of this window's.
    private func draggedTab(_ info: NSDraggingInfo) -> NSWindow? {
        guard reorders, let w = Self.draggedWindow(info), ownsTab(w) else { return nil }
        return w
    }

    static func draggedWindow(_ info: NSDraggingInfo) -> NSWindow? {
        guard let s = info.draggingPasteboard.string(forType: dragType), let number = Int(s) else { return nil }
        return NSApp.window(withWindowNumber: number)
    }

    func outlineView(_ outlineView: NSOutlineView, pasteboardWriterForItem item: Any) -> NSPasteboardWriting? {
        guard reorders, let box = item as? TabBox, box.tab.pane == nil, let w = box.tab.controller?.window, ownsTab(w)
        else { return nil }
        let pb = NSPasteboardItem()
        pb.setString(String(w.windowNumber), forType: Self.dragType)
        return pb
    }

    func outlineView(_ outlineView: NSOutlineView, validateDrop info: NSDraggingInfo, proposedItem item: Any?,
                     proposedChildIndex index: Int) -> NSDragOperation {
        guard let moving = draggedTab(info) else { return [] }
        if let target = workspaceGroup(at: item) {
            guard moveTarget(moving, into: target) != nil else { return [] }
            outlineView.setDropItem(target, dropChildIndex: NSOutlineViewDropOnItemIndex)
            return .move
        }
        // One of this window's tabs, dropped between rows (not onto one) among them.
        guard allowsDrop(moving, into: item, at: index) else { return [] }
        return .move
    }

    static func allowsDrop(into group: SidebarGroup?, at index: Int, pinned: Bool, ownPinnedRows: Int) -> Bool {
        guard index != NSOutlineViewDropOnItemIndex else { return false }
        if group?.key == pinnedKey { return pinned && index <= ownPinnedRows }
        return !pinned && group?.elsewhere != true
    }

    private func allowsDrop(_ moving: NSWindow, into item: Any?, at index: Int) -> Bool {
        guard let (group, index) = dropTarget(item, at: index) else { return false }
        let own = group?.boxes.prefix { $0.tab.controller?.window.map(ownsTab) == true }.count ?? 0
        return Self.allowsDrop(into: group, at: index,
                               pinned: (moving.windowController as? TerminalWindowController)?.pinned == true,
                               ownPinnedRows: own)
    }

    /// The group a drop between rows under `item` goes into, and where among its rows: the
    /// item itself, or the group whose rows are listed there without a header (nil: the one
    /// list without headers). A host's header lists workspaces, not tabs: no drop there.
    private func dropTarget(_ item: Any?, at index: Int) -> (group: SidebarGroup?, index: Int)? {
        if let g = item as? SidebarGroup, g.host == nil { return (g, index) }
        if item == nil, !showHeaders { return (nil, index) }
        guard index != NSOutlineViewDropOnItemIndex else { return nil }
        let items = (item as? SidebarGroup)?.children ?? rootItems
        let near = [index - 1, index].lazy.compactMap { items.indices.contains($0) ? items[$0] as? TabBox : nil }.first
        guard let g = near?.group, let start = items.firstIndex(where: { ($0 as? TabBox)?.group === g })
        else { return nil }
        return (g, index - start)
    }

    private func workspaceGroup(at item: Any?) -> SidebarGroup? {
        let group = item as? SidebarGroup ?? outline.parent(forItem: item) as? SidebarGroup
        return group?.elsewhere == true ? group : nil
    }

    private func moveTarget(_ moving: NSWindow, into group: SidebarGroup) -> Workspace? {
        guard let c = moving.windowController as? TerminalWindowController else { return nil }
        return SessionManager.shared.moveTargets(for: c).first { $0.id == group.workspace }
    }

    func outlineView(_ outlineView: NSOutlineView, acceptDrop info: NSDraggingInfo, item: Any?,
                     childIndex index: Int) -> Bool {
        guard let moving = draggedTab(info) else { return false }
        if let target = workspaceGroup(at: item) {
            guard let ws = moveTarget(moving, into: target),
                  let c = moving.windowController as? TerminalWindowController else { return false }
            DispatchQueue.main.async { SessionManager.shared.moveTab(c, to: ws) }
            return true
        }
        guard allowsDrop(moving, into: item, at: index), let group = controller?.window?.tabGroup,
              let (target, index) = dropTarget(item, at: index) else { return false }
        // Rows of the drop's group in order, and the tab that ends up right after the dropped one.
        let rows: [TabBox] = target?.boxes ?? (showHeaders ? [] : groups.first?.boxes ?? [])
        let all = rows.compactMap { $0.tab.controller?.window }.filter(ownsTab)
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

    /// A tab, or the new-workspace row, so the keyboard reaches it (Return opens it).
    func outlineView(_ outlineView: NSOutlineView, shouldSelectItem item: Any) -> Bool {
        item is TabBox || item is NewWorkspaceBox
    }

    func outlineView(_ outlineView: NSOutlineView, rowViewForItem item: Any) -> NSTableRowView? {
        item is TabBox || item is NewWorkspaceBox ? SidebarRowView() : nil
    }

    func outlineView(_ outlineView: NSOutlineView, heightOfRowByItem item: Any) -> CGFloat {
        if let g = item as? SidebarGroup { return g.host != nil ? SidebarHeadingView.hostHeight : SidebarHeadingView.height }
        if item is NewWorkspaceBox { return NewWorkspaceRowView.height }
        return 40
    }

    func outlineView(_ outlineView: NSOutlineView, viewFor tableColumn: NSTableColumn?, item: Any) -> NSView? {
        if let g = item as? SidebarGroup {
            guard let host = g.host else { return SidebarHeadingView(g.name) }
            return SidebarHeadingView(g.name, symbol: host == localHost ? "laptopcomputer" : "globe", detail: g.state)
        }
        if let box = item as? NewWorkspaceBox {
            let view = NewWorkspaceRowView(box.title)
            view.press = { SessionManager.shared.newWorkspace(host: box.host) }
            return view
        }
        guard let box = item as? TabBox else { return nil }
        let cell = outlineView.makeView(withIdentifier: TabRowView.identifier, owner: nil) as? TabRowView
            ?? TabRowView()
        // A tab's card previews the tab, not an agent's pane.
        cell.showsHoverCard = box.tab.pane == nil && box.tab.controller != nil
        cell.configure(box.tab, truncateTail: box.tab.pane != nil)
        return cell
    }

    // MARK: Actions

    @objc private func rowClicked(_ sender: Any?) {
        activate(row: outline.clickedRow >= 0 ? outline.clickedRow : outline.selectedRow)
    }

    /// A row clicked, or chosen with the keyboard: its tab, agent or new workspace comes forward.
    private func activate(row: Int) {
        TabHoverCard.shared.dismiss()
        if row >= 0, let box = outline.item(atRow: row) as? NewWorkspaceBox {
            SessionManager.shared.newWorkspace(host: box.host)
            return
        }
        guard row >= 0, let box = outline.item(atRow: row) as? TabBox else { return }
        if let pane = box.tab.pane {
            focusAgent(pane)
            return
        }
        if let hidden = box.tab.hidden {
            SessionManager.shared.showHiddenTab(hidden, from: controller)
            return
        }
        guard let w = box.tab.controller?.window else { return }
        w.makeKeyAndOrderFront(nil)
        if let view = box.tab.controller?.content.focusedView {
            w.makeFirstResponder(view)
        }
    }

    /// Brings an agent's pane forward: its window and tab, its split, its workspace.
    private func focusAgent(_ pane: PaneKey) {
        let sm = SessionManager.shared
        guard let a = sm.agentPanes(first: controller).first(where: { $0.pane == pane }) else { return }
        sm.focusAgent(a, from: controller)
        // Its tab came forward with another split focused: the list there selects this agent
        // before it is drawn, not a refresh later.
        sm.controller(for: pane)?.sidebar?.reload()
    }

    private func rowMenu() -> NSMenu {
        let menu = NSMenu()
        let close = NSMenuItem(title: "Close Tab", action: #selector(closeClicked(_:)), keyEquivalent: "")
        close.target = self
        menu.addItem(close)
        let move = NSMenuItem(title: "Move Tab to New Workspace", action: #selector(moveClicked(_:)), keyEquivalent: "")
        move.target = self
        menu.addItem(move)
        let moveTo = NSMenuItem(title: "Move Tab to Workspace", action: nil, keyEquivalent: "")
        moveTo.submenu = NSMenu(title: "Move Tab to Workspace")
        moveTo.submenu?.delegate = self
        menu.addItem(moveTo)
        let pin = NSMenuItem(title: "Pin Tab", action: #selector(pinClicked(_:)), keyEquivalent: "")
        pin.target = self
        menu.addItem(pin)
        let float = NSMenuItem(title: "Float Tab Here", action: #selector(floatClicked(_:)), keyEquivalent: "")
        float.target = self
        menu.addItem(float)
        menu.delegate = self
        return menu
    }

    private func clickedRow() -> SidebarTab? {
        let row = outline.clickedRow
        guard row >= 0, let box = outline.item(atRow: row) as? TabBox else { return nil }
        return box.tab
    }

    private func clickedTab() -> TerminalWindowController? {
        clickedRow()?.controller
    }

    private func clickedRowPinned() -> Bool? {
        guard let tab = clickedRow() else { return nil }
        if let c = tab.controller { return c.pinned }
        return tab.hidden.map(SessionManager.shared.isPinned)
    }

    @objc private func pinClicked(_ sender: Any?) {
        guard let tab = clickedRow() else { return }
        if let c = tab.controller {
            SessionManager.shared.togglePin(c)
        } else if let hidden = tab.hidden {
            SessionManager.shared.togglePin(hidden)
        }
    }

    @objc private func closeClicked(_ sender: Any?) {
        clickedTab()?.window?.performClose(nil)
    }

    @objc private func moveClicked(_ sender: Any?) {
        guard let c = clickedTab() else { return }
        SessionManager.shared.moveTabToNewWorkspace(c)
    }

    @objc private func floatClicked(_ sender: Any?) {
        guard let src = clickedTab(), let controller else { return }
        SessionManager.shared.floatTab(src, into: controller)
    }

    func cardDrop(at windowPoint: NSPoint, for pane: PaneKey) -> (gap: TabGap, line: CGRect)? {
        guard reorders, !view.isHiddenOrHasHiddenAncestor, view.window != nil else { return nil }
        let p = outline.convert(windowPoint, from: nil)
        guard outline.visibleRect.contains(p), !overOtherTabs(outline.row(at: p)) else { return nil }
        let rows = cardDropRows()
        let grouped = Self.groupsByRepo(SessionManager.shared.config)
        let keys = rows.map { grouped ? Self.group(for: $0.tab)?.key : "" }
        let key = grouped ? Self.group(for: pane)?.key : ""
        if let gap = SidebarDrop.gap(rows: rows.map(\.rect), keys: keys, key: key, at: p.y) {
            return ((rows[gap.index].tab, gap.after), Self.dropLine(rows[gap.index].rect, after: gap.after))
        }
        guard let edge = SidebarDrop.newGroupEdge(keys: keys, key: key),
              let last = controller?.window?.tabGroup?.windows.last?.windowController as? TerminalWindowController
        else { return nil }
        return ((last, true), Self.dropLine(rows[edge.index].rect, after: edge.after))
    }

    private static func dropLine(_ row: CGRect, after: Bool) -> CGRect {
        CGRect(x: row.minX + 8, y: (after ? row.maxY : row.minY) - 1, width: max(0, row.width - 16), height: 2)
    }

    func showCardDrop(_ line: CGRect?) {
        guard let line else {
            dropLine.isHidden = true
            return
        }
        if dropLine.superview !== outline { outline.addSubview(dropLine) }
        dropLine.frame = line
        dropLine.layer?.backgroundColor = SessionManager.shared.config.chrome.accent.cgColor
        dropLine.isHidden = false
    }

    private func overOtherTabs(_ row: Int) -> Bool {
        guard row >= 0, let item = outline.item(atRow: row) else { return false }
        if let group = item as? SidebarGroup { return group.elsewhere }
        return ownedTab(row: row) == nil
    }

    private func ownedTab(row: Int) -> TerminalWindowController? {
        guard let box = outline.item(atRow: row) as? TabBox, let c = box.tab.controller, let w = c.window,
              ownsTab(w), (outline.parent(forItem: box) as? SidebarGroup)?.elsewhere != true else { return nil }
        return c
    }

    private func cardDropRows() -> [(tab: TerminalWindowController, rect: CGRect)] {
        (0..<outline.numberOfRows).compactMap { row in ownedTab(row: row).map { ($0, outline.rect(ofRow: row)) } }
    }
}

enum SidebarDrop {
    static func gap(rows: [CGRect], keys: [String?], key: String?, at y: CGFloat) -> (index: Int, after: Bool)? {
        let nearest = rows.indices.filter { keys[$0] == key }.min { distance(y, rows[$0]) < distance(y, rows[$1]) }
        return nearest.map { ($0, y >= rows[$0].midY) }
    }

    static func newGroupEdge(keys: [String?], key: String?) -> (index: Int, after: Bool)? {
        guard !keys.isEmpty else { return nil }
        guard key != nil else { return (keys.count - 1, true) }
        return keys.lastIndex { $0 != nil }.map { ($0, true) } ?? (0, false)
    }

    private static func distance(_ y: CGFloat, _ rect: CGRect) -> CGFloat {
        y < rect.minY ? rect.minY - y : y >= rect.maxY ? y - rect.maxY : 0
    }
}

extension TabSidebarViewController: NSMenuDelegate {
    func menuNeedsUpdate(_ menu: NSMenu) {
        if menu === outline.menu {
            menu.items.first { $0.submenu != nil }?.isHidden = clickedTab() == nil
        } else {
            SessionManager.shared.fillMoveTabMenu(menu, for: clickedTab())
        }
    }
}

extension TabSidebarViewController: NSMenuItemValidation {
    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(floatClicked(_:)) { return validateFloatItem(menuItem, for: clickedTab()) }
        guard menuItem.action == #selector(pinClicked(_:)) else { return clickedTab() != nil }
        guard let pinned = clickedRowPinned() else { return false }
        menuItem.title = pinned ? "Unpin Tab" : "Pin Tab"
        return clickedTab()?.isQuick != true
    }

    func validateFloatItem(_ item: NSMenuItem, for src: TerminalWindowController?) -> Bool {
        let can = src.flatMap { src in controller.map { SessionManager.shared.canFloat(src, into: $0) } } ?? false
        item.isHidden = !can
        return can
    }
}

/// A group's heading in the tab list. Its label starts where the workspace title above it does,
/// wherever the outline places the row.
final class SidebarHeadingView: NSView {
    static let height: CGFloat = 22
    /// A host's header, in the list by host.
    static let hostHeight: CGFloat = 28
    static let inset: CGFloat = 16
    /// Where a host's name starts after its icon.
    static let hostLabelOffset: CGFloat = 22
    let label: NSTextField
    private var icon: NSImageView?
    /// A host's state while it is not connected.
    private var detail: NSTextField?

    init(_ title: String, symbol: String? = nil, detail state: String? = nil) {
        label = NSTextField(labelWithString: title)
        super.init(frame: .zero)
        let chrome = SessionManager.shared.config.chrome
        label.lineBreakMode = .byTruncatingTail
        if let symbol {
            label.font = .systemFont(ofSize: NSFont.systemFontSize, weight: .semibold)
            label.textColor = chrome.contentPrimary
            let icon = NSImageView(image: NSImage(systemSymbolName: symbol, accessibilityDescription: nil) ?? NSImage())
            icon.symbolConfiguration = .init(pointSize: 12, weight: .regular)
            icon.contentTintColor = chrome.contentSecondary
            addSubview(icon)
            self.icon = icon
        } else {
            label.font = .systemFont(ofSize: NSFont.smallSystemFontSize, weight: .semibold)
            label.textColor = chrome.contentSecondary
        }
        addSubview(label)
        if let state {
            let d = NSTextField(labelWithString: state)
            d.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
            d.textColor = chrome.contentSecondary
            d.lineBreakMode = .byTruncatingTail
            addSubview(d)
            detail = d
        }
        setAccessibilityElement(true)
        setAccessibilityRole(.staticText)
        setAccessibilityLabel([title, state].compactMap { $0 }.joined(separator: ", "))
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func layout() {
        super.layout()
        var table = superview
        while let v = table, !(v is NSTableView) { table = v.superview }
        let x = table.map { convert(NSPoint.zero, to: $0).x } ?? 0
        var left = max(0, Self.inset - x)
        if let icon {
            let s = icon.intrinsicContentSize
            icon.frame = NSRect(x: left, y: ((bounds.height - s.height) / 2).rounded(), width: s.width, height: s.height)
            left += Self.hostLabelOffset
        }
        let size = label.intrinsicContentSize
        var right = bounds.width - 8
        if let detail {
            // The host's state whole when it fits beside the name; else it keeps up to half the
            // row and the name is cut short, so a long name never hides the state.
            let d = detail.cell?.cellSize ?? detail.intrinsicContentSize
            let room = max(0, right - left - 8)
            let w = min(ceil(d.width) + 1, max(room - ceil(size.width), (room / 2).rounded(.down)))
            detail.frame = NSRect(x: right - w, y: ((bounds.height - d.height) / 2).rounded(), width: w, height: d.height)
            right -= w + 8
        }
        label.frame = NSRect(x: left, y: ((bounds.height - size.height) / 2).rounded(),
                             width: max(0, right - left), height: size.height)
    }
}

/// "New Workspace on <host>" under a connected host without one: a plus and the title, quieter
/// than a tab.
final class NewWorkspaceRowView: NSView {
    static let height: CGFloat = 28
    private let icon = NSImageView()
    private let label: NSTextField
    /// Opens the workspace: VoiceOver's press, as the row is a button to it.
    var press: (() -> Void)?

    init(_ title: String) {
        label = NSTextField(labelWithString: title)
        super.init(frame: .zero)
        let chrome = SessionManager.shared.config.chrome
        icon.image = NSImage(systemSymbolName: "plus", accessibilityDescription: nil)
        icon.symbolConfiguration = .init(pointSize: 11, weight: .medium)
        icon.contentTintColor = chrome.contentSecondary
        label.font = .systemFont(ofSize: NSFont.systemFontSize)
        label.textColor = chrome.contentSecondary
        label.lineBreakMode = .byTruncatingTail
        addSubview(icon)
        addSubview(label)
        setAccessibilityElement(true)
        setAccessibilityRole(.button)
        setAccessibilityLabel(title)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func accessibilityPerformPress() -> Bool {
        guard let press else { return false }
        press()
        return true
    }

    override func layout() {
        super.layout()
        var table = superview
        while let v = table, !(v is NSTableView) { table = v.superview }
        let x = table.map { convert(NSPoint.zero, to: $0).x } ?? 0
        let left = max(0, SidebarHeadingView.inset - x)
        let s = icon.intrinsicContentSize
        icon.frame = NSRect(x: left + 2, y: ((bounds.height - s.height) / 2).rounded(), width: s.width, height: s.height)
        let size = label.intrinsicContentSize
        label.frame = NSRect(x: left + 22, y: ((bounds.height - size.height) / 2).rounded(),
                             width: max(0, bounds.width - left - 34), height: size.height)
    }
}

/// The sidebar's outline: Return, Enter or Space opens the selected row, as a click does.
final class SidebarOutlineView: NSOutlineView {
    var activate: (() -> Void)?

    override func keyDown(with event: NSEvent) {
        let keys: Set<UInt16> = [36, 76, 49] // Return, keypad Enter, Space
        if keys.contains(event.keyCode), event.modifierFlags.intersection(.deviceIndependentFlagsMask)
            .subtracting([.numericPad, .function]).isEmpty, selectedRow >= 0, let activate {
            activate()
            return
        }
        super.keyDown(with: event)
    }
}

/// A sidebar row whose selection is a capsule in the chrome's colors, like the tab strip's
/// selected tab. Its table highlights nothing itself (`selectionHighlightStyle = .none`).
final class SidebarRowView: NSTableRowView {
    override var isSelected: Bool {
        didSet { if isSelected != oldValue { needsDisplay = true } }
    }

    private let keyRedraw = KeyWindowRedraw()

    override init(frame frameRect: NSRect) {
        super.init(frame: frameRect)
        // The light themes' shadow falls below the row.
        clipsToBounds = false
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        keyRedraw.track(self) { [weak self] in self?.isSelected == true }
    }

    override func drawBackground(in dirtyRect: NSRect) {
        super.drawBackground(in: dirtyRect)
        guard isSelected else { return }
        SessionManager.shared.config.chrome.drawSelectionPill(in: bounds.insetBy(dx: 10, dy: 1),
                                                              key: window?.isKeyWindow == true)
    }

    override var interiorBackgroundStyle: NSView.BackgroundStyle { .normal }
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

/// One sidebar row: icon tile with a status dot, title over branch/diff (or directory) and the
/// progress bar, working dots and the Cmd+N hint (shown while Command is held).
final class TabRowView: NSTableCellView {
    static let identifier = NSUserInterfaceItemIdentifier("TabRow")

    private let icon = NSImageView()
    private let dot = NSView()
    private let working = WorkingDots()
    private let title = NSTextField(labelWithString: "")
    private let subtitle = NSTextField(labelWithString: "")
    private let shortcut = NSTextField(labelWithString: "")
    private var trailingDetail: String?
    private var shortcutHint = ""
    private let hold: CommandHold
    private var subtitleEnd: NSLayoutConstraint!
    private let progress = ProgressBarView()
    /// Tab rows show the tab's hover card; agent rows (one pane each) don't.
    var showsHoverCard = false
    private weak var controller: TerminalWindowController?

    init(hold: CommandHold = .shared) {
        self.hold = hold
        super.init(frame: .zero)
        identifier = Self.identifier
        addTrackingArea(NSTrackingArea(rect: .zero, options: [.mouseEnteredAndExited, .activeInActiveApp, .inVisibleRect],
                                       owner: self))
        icon.imageScaling = .scaleNone
        icon.imageAlignment = .alignBottom
        dot.wantsLayer = true
        dot.layer?.cornerRadius = 4
        dot.layer?.borderWidth = 1.5
        title.font = .systemFont(ofSize: 13)
        title.lineBreakMode = .byTruncatingTail
        subtitle.font = .monospacedDigitSystemFont(ofSize: 11, weight: .regular)
        subtitle.textColor = SessionManager.shared.config.chrome.contentSecondary
        subtitle.lineBreakMode = .byTruncatingMiddle
        subtitle.maximumNumberOfLines = 1
        title.maximumNumberOfLines = 1
        shortcut.font = .systemFont(ofSize: 11)
        shortcut.textColor = SessionManager.shared.config.chrome.contentTertiary
        progress.isHidden = true
        for v in [icon, dot, title, subtitle, working, shortcut, progress] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        title.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        subtitle.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        shortcut.setContentHuggingPriority(.required, for: .horizontal)
        // An empty hint has no width, and the gap goes with it: the branch gets the row back.
        subtitleEnd = subtitle.trailingAnchor.constraint(lessThanOrEqualTo: shortcut.leadingAnchor)
        NSLayoutConstraint.activate([
            subtitleEnd,
            icon.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 6),
            // Centered on the row, not the title: the status dot on its corner stays inside
            // the selection.
            // The tile's bottom stays put; the panes stacked behind it rise above.
            icon.bottomAnchor.constraint(equalTo: centerYAnchor, constant: 10),
            icon.widthAnchor.constraint(equalToConstant: 20),
            icon.heightAnchor.constraint(equalToConstant: 26),
            dot.centerXAnchor.constraint(equalTo: icon.trailingAnchor, constant: -1),
            dot.centerYAnchor.constraint(equalTo: icon.bottomAnchor, constant: -19),
            dot.widthAnchor.constraint(equalToConstant: 8),
            dot.heightAnchor.constraint(equalToConstant: 8),
            title.leadingAnchor.constraint(equalTo: icon.trailingAnchor, constant: 8),
            title.topAnchor.constraint(equalTo: topAnchor, constant: 4),
            title.trailingAnchor.constraint(lessThanOrEqualTo: working.leadingAnchor, constant: -6),
            working.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            working.centerYAnchor.constraint(equalTo: title.centerYAnchor),
            subtitle.leadingAnchor.constraint(equalTo: title.leadingAnchor),
            subtitle.topAnchor.constraint(equalTo: title.bottomAnchor, constant: 1),
            shortcut.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            shortcut.firstBaselineAnchor.constraint(equalTo: subtitle.firstBaselineAnchor),
            // Under the title and branch, in the row's bottom padding: the row keeps its height.
            progress.leadingAnchor.constraint(equalTo: title.leadingAnchor),
            progress.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            progress.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -2),
            progress.heightAnchor.constraint(equalToConstant: ProgressBarView.height),
        ])
        NotificationCenter.default.addObserver(self, selector: #selector(commandHeldChanged),
                                               name: CommandHold.changed, object: hold)
    }

    @objc private func commandHeldChanged() {
        shortcut.stringValue = trailingDetail ?? (hold.held ? shortcutHint : "")
        subtitleEnd.constant = shortcut.stringValue.isEmpty ? 0 : -4
    }

    /// The Cmd+N hint as shown, empty while hidden.
    var shownShortcut: String { shortcut.stringValue }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    func configure(_ tab: SidebarTab, truncateTail: Bool = false) {
        controller = tab.controller
        subtitle.lineBreakMode = truncateTail ? .byTruncatingTail : .byTruncatingMiddle
        title.stringValue = tab.title
        title.font = .systemFont(ofSize: 13, weight: tab.selected ? .medium : .regular)
        trailingDetail = tab.trailingDetail
        shortcutHint = tab.shortcut.map { "⌘\($0)" } ?? ""
        commandHeldChanged()
        if let branch = tab.branch {
            // Attributed strings don't inherit the field's line break mode: keep one line, and
            // shorten the branch rather than the diff counts.
            let para = NSMutableParagraphStyle()
            para.lineBreakMode = .byTruncatingMiddle
            let chrome = SessionManager.shared.config.chrome
            let base: [NSAttributedString.Key: Any] = [.foregroundColor: chrome.contentSecondary,
                                                      .font: subtitle.font!, .paragraphStyle: para]
            let s = NSMutableAttributedString(string: branch, attributes: base)
            if tab.added > 0 {
                s.append(NSAttributedString(string: "  +\(tab.added)",
                                            attributes: base.merging([.foregroundColor: chrome.success]) { $1 }))
            }
            if tab.removed > 0 {
                s.append(NSAttributedString(string: " −\(tab.removed)",
                                            attributes: base.merging([.foregroundColor: chrome.danger]) { $1 }))
            }
            subtitle.attributedStringValue = s
        } else {
            // The directory, unless the title already says the same.
            let dir = tab.subtitle ?? ""
            subtitle.stringValue = dir == tab.title || tab.title.hasSuffix(dir) ? "" : dir
        }
        icon.image = tab.icon.tile(size: 20, behind: tab.behind, badge: nil)
        // A working agent shows the moving dots; the badge on the tile is for the states that
        // want attention. Idle agents show neither.
        working.isShown = tab.status == .working
        working.color = SessionManager.shared.config.chrome.contentSecondary
        dot.isHidden = tab.status == nil || tab.status == .idle || tab.status == .working
        dot.layer?.backgroundColor = tab.status?.color.cgColor
        dot.layer?.borderColor = SessionManager.shared.config.sidebarColor.cgColor
        progress.track = SessionManager.shared.config.chrome.control
        setProgress(tab.progress)
        // The other tabs have their hover card instead.
        toolTip = showsHoverCard && !tab.selected ? nil : [tab.title, tab.subtitle].compactMap { $0 }.joined(separator: "\n")
    }

    /// Sets the bar alone: its fill layer moves, the rest of the row stays as it is. Like the
    /// pane's bar, indeterminate is a still, dimmed full bar.
    func setProgress(_ report: ProgressReport?) {
        progress.report = report
        if progress.isHidden != (report == nil) { progress.isHidden = report == nil }
    }

    override func mouseEntered(with event: NSEvent) {
        if showsHoverCard { TabHoverCard.shared.enter(self, controller: controller, edge: .right) }
    }

    override func mouseExited(with event: NSEvent) {
        if showsHoverCard { TabHoverCard.shared.exit(self) }
    }

    override func scrollWheel(with event: NSEvent) {
        TabHoverCard.shared.dismiss()
        super.scrollWheel(with: event)
    }
}

/// Whether Command alone has been held a moment, for the sidebar's Cmd+N hints. The delay keeps
/// them from flashing on every Command shortcut.
final class CommandHold {
    static let shared = CommandHold()
    static let changed = Notification.Name("ThurmCommandHoldChanged")
    private(set) var held = false {
        didSet { if held != oldValue { NotificationCenter.default.post(name: Self.changed, object: self) } }
    }
    private var pending: DispatchWorkItem?
    /// Runs the reveal after the delay; tests run it themselves.
    private let schedule: (DispatchWorkItem) -> Void

    private convenience init() {
        self.init { DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: $0) }
        NSEvent.addLocalMonitorForEvents(matching: [.flagsChanged, .keyDown]) { [weak self] event in
            self?.update(event.type, event.modifierFlags)
            return event
        }
    }

    init(schedule: @escaping (DispatchWorkItem) -> Void) {
        self.schedule = schedule
        // Posted on the main thread, so the reset runs there without a hop.
        NotificationCenter.default.addObserver(forName: NSApplication.didResignActiveNotification,
                                               object: nil, queue: nil) { [weak self] _ in self?.reset() }
    }

    func update(_ type: NSEvent.EventType, _ flags: NSEvent.ModifierFlags) {
        let mods = flags.intersection([.command, .option, .control, .shift, .function])
        guard type == .flagsChanged, mods == .command else { return reset() }
        guard pending == nil, !held else { return }
        let work = DispatchWorkItem { [weak self] in
            self?.pending = nil
            self?.held = true
        }
        pending = work
        schedule(work)
    }

    private func reset() {
        pending?.cancel()
        pending = nil
        held = false
    }
}

/// Three dots that pulse in turn while an agent works. The pulse is a Core Animation keyframe on
/// each dot's opacity, so the render server runs it: no timer, no redraw, nothing per frame on
/// the main thread. It runs only while shown in a window, at a capped frame rate, and stands
/// still under reduce motion. Hidden, it takes no width.
final class WorkingDots: NSView {
    static let size: CGFloat = 3.5
    static let gap: CGFloat = 2.5
    /// Space after the last dot, before whatever follows the view.
    static let trailing: CGFloat = 6
    static let period: CFTimeInterval = 1.2
    static let key = "working"

    let dots = (0..<3).map { _ in CALayer() }
    /// Whether to stand still; tests swap in their own answer.
    var reduced: () -> Bool = { Motion.reduced }

    var isShown = false {
        didSet {
            guard isShown != oldValue else { return }
            isHidden = !isShown
            invalidateIntrinsicContentSize()
            update()
        }
    }

    var color: NSColor = .secondaryLabelColor {
        didSet {
            guard color != oldValue else { return }
            CATransaction.begin()
            CATransaction.setDisableActions(true)
            for d in dots { d.backgroundColor = color.cgColor }
            CATransaction.commit()
        }
    }

    init() {
        super.init(frame: .zero)
        wantsLayer = true
        isHidden = true
        for d in dots {
            d.cornerRadius = Self.size / 2
            d.backgroundColor = color.cgColor
            layer?.addSublayer(d)
        }
        setContentHuggingPriority(.required, for: .horizontal)
        setContentCompressionResistancePriority(.required, for: .horizontal)
        // Reduce motion switched on or off while a row stays up starts or stops its pulse.
        NSWorkspace.shared.notificationCenter.addObserver(
            self, selector: #selector(displayOptionsChanged),
            name: NSWorkspace.accessibilityDisplayOptionsDidChangeNotification, object: nil)
    }

    deinit {
        NSWorkspace.shared.notificationCenter.removeObserver(self)
    }

    @objc func displayOptionsChanged() {
        update()
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var intrinsicContentSize: NSSize {
        guard isShown else { return NSSize(width: 0, height: Self.size) }
        return NSSize(width: 3 * Self.size + 2 * Self.gap + Self.trailing, height: Self.size)
    }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        let y = (bounds.height - Self.size) / 2
        for (i, d) in dots.enumerated() {
            d.frame = CGRect(x: CGFloat(i) * (Self.size + Self.gap), y: y, width: Self.size, height: Self.size)
        }
        CATransaction.commit()
    }

    override func viewDidMoveToWindow() {
        super.viewDidMoveToWindow()
        update()
    }

    override func viewDidChangeBackingProperties() {
        super.viewDidChangeBackingProperties()
        let scale = window?.backingScaleFactor ?? 2
        for d in dots { d.contentsScale = scale }
    }

    /// Starts the pulse when it should run and isn't, stops it when it shouldn't. A row
    /// reconfigured while its agent works (every retitle) leaves a running pulse alone.
    private func update() {
        let reduced = reduced()
        let run = isShown && window != nil && !reduced
        let running = dots[0].animation(forKey: Self.key) != nil
        guard !(run && running) else { return }
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        defer { CATransaction.commit() }
        guard run else {
            // Still: plainly visible under reduce motion, dim otherwise (hidden or offscreen).
            for d in dots {
                d.removeAnimation(forKey: Self.key)
                d.opacity = reduced ? 0.85 : 0.3
            }
            return
        }
        // One phase for every row, so two working tabs pulse together.
        let now = CACurrentMediaTime()
        let start = now - now.truncatingRemainder(dividingBy: Self.period)
        for (i, d) in dots.enumerated() {
            d.opacity = 0.3
            let pulse = CAKeyframeAnimation(keyPath: "opacity")
            pulse.values = [0.3, 0.85, 0.3, 0.3]
            pulse.keyTimes = [0, 0.25, 0.5, 1]
            pulse.timingFunctions = Array(repeating: CAMediaTimingFunction(name: .easeInEaseOut), count: 3)
            pulse.duration = Self.period
            pulse.repeatCount = .infinity
            pulse.beginTime = d.convertTime(start + Double(i) * Self.period / 6, from: nil)
            pulse.fillMode = .backwards
            pulse.isRemovedOnCompletion = false
            pulse.preferredFrameRateRange = CAFrameRateRange(minimum: 10, maximum: 30, preferred: 30)
            d.add(pulse, forKey: Self.key)
        }
    }
}
