import AppKit

/// A named set of tabs, after tty7's workspaces: every window shows one workspace; the others
/// keep their shells running in the daemon until a window switches to them.
final class Workspace {
    let id: UInt64
    var name: String
    var lastActive: Date
    /// Its tabs while no window shows it. Assigning directly invalidates any cached live
    /// `TabItem`s so external updates (restore, tests, layout mutations) stay authoritative.
    var hiddenTabs: [TabLayout] {
        didSet {
            if !syncingHiddenTabs {
                invalidateCachedTabs()
            }
        }
    }
    var hiddenSelectedTab: Int
    /// Live `TabItem`s parked while no window shows this workspace in sidebar mode, eliminating
    /// `TabContentView` / `TerminalView` reconstruction on workspace switches. When non-nil, its
    /// elements correspond 1:1 with `hiddenTabs`.
    private(set) var cachedTabs: [TabItem]?
    private var syncingHiddenTabs = false
    /// Whose panes it shows: this Mac's daemon, or a `[[remote]]` host's (a remote workspace).
    let host: HostId

    init(id: UInt64, name: String, lastActive: Date = Date(), hiddenTabs: [TabLayout] = [],
         hiddenSelectedTab: Int = 0, host: HostId = localHost) {
        self.id = id
        self.name = name
        self.lastActive = lastActive
        self.hiddenTabs = hiddenTabs
        self.hiddenSelectedTab = hiddenSelectedTab
        self.host = host
    }

    convenience init(layout l: WorkspaceLayout) {
        self.init(id: l.id, name: l.name, lastActive: Date(timeIntervalSince1970: TimeInterval(l.lastActive)),
                  hiddenTabs: l.tabs, hiddenSelectedTab: l.selectedTab, host: l.host ?? localHost)
    }

    deinit {
        invalidateCachedTabs()
    }

    var isRemote: Bool { host != localHost }

    /// Parks live tabs when switching away from this workspace in sidebar mode while keeping
    /// `hiddenTabs` / `hiddenSelectedTab` synchronized for persistence, pickers, and the sidebar.
    func parkTabs(_ tabs: [TabItem], selected: Int) {
        let kept = tabs.filter { !$0.content.isEmpty }
        syncingHiddenTabs = true
        hiddenTabs = kept.map { $0.tabLayout() }
        hiddenSelectedTab = kept.isEmpty ? 0 : min(max(0, selected), kept.count - 1)
        cachedTabs = kept.isEmpty ? nil : kept
        syncingHiddenTabs = false
    }

    /// Takes the cached live tabs when switching back to this workspace in sidebar mode,
    /// honoring the current `hiddenSelectedTab`.
    func takeCachedTabs() -> (tabs: [TabItem], selected: Int)? {
        guard let cached = cachedTabs, cached.count == hiddenTabs.count, !cached.isEmpty else {
            invalidateCachedTabs()
            return nil
        }
        let sel = min(max(0, hiddenSelectedTab), cached.count - 1)
        syncingHiddenTabs = true
        cachedTabs = nil
        hiddenTabs = []
        hiddenSelectedTab = 0
        syncingHiddenTabs = false
        return (cached, sel)
    }

    /// Appends a live tab while preserving `cachedTabs` if already warm.
    func appendParkedTab(_ tab: TabItem) {
        let layout = tab.tabLayout()
        if var cached = cachedTabs, cached.count == hiddenTabs.count {
            cached.append(tab)
            syncingHiddenTabs = true
            cachedTabs = cached
            hiddenTabs.append(layout)
            syncingHiddenTabs = false
        } else {
            tab.content.detachAll()
            tab.isClosed = true
            hiddenTabs.append(layout)
        }
    }

    /// Toggles pin at `index` in both `hiddenTabs` and `cachedTabs` without invalidating the cache.
    func toggleHiddenPin(at index: Int) {
        guard hiddenTabs.indices.contains(index) else { return }
        let next = hiddenTabs[index].pinned != true
        syncingHiddenTabs = true
        hiddenTabs[index].pinned = next ? true : nil
        if let cached = cachedTabs, cached.indices.contains(index) {
            cached[index].pinned = next
        }
        syncingHiddenTabs = false
    }

    /// Removes any single-pane tab holding only `pane`, preserving `cachedTabs` for remaining tabs.
    func removeSinglePaneTab(_ pane: PaneKey) -> Bool {
        let before = hiddenTabs.count
        guard before > 0 else { return false }
        if let cached = cachedTabs, cached.count == hiddenTabs.count {
            var keptCached: [TabItem] = []
            var keptLayouts: [TabLayout] = []
            for (tab, layout) in zip(cached, hiddenTabs) {
                if layout.panes == [pane] {
                    tab.content.detachAll()
                    tab.isClosed = true
                } else {
                    keptCached.append(tab)
                    keptLayouts.append(layout)
                }
            }
            if keptLayouts.count != before {
                syncingHiddenTabs = true
                cachedTabs = keptCached.isEmpty ? nil : keptCached
                hiddenTabs = keptLayouts
                hiddenSelectedTab = min(hiddenSelectedTab, max(0, hiddenTabs.count - 1))
                syncingHiddenTabs = false
            }
        } else {
            hiddenTabs.removeAll { $0.panes == [pane] }
            hiddenSelectedTab = min(hiddenSelectedTab, max(0, hiddenTabs.count - 1))
        }
        return before > 0 && hiddenTabs.isEmpty
    }

    /// Retains only panes matching `keep`, preserving cached live `TabItem`s whenever possible.
    func retainHiddenPanes(_ keep: (PaneKey) -> Bool) {
        if let cached = cachedTabs, cached.count == hiddenTabs.count {
            var remaining: [TabItem] = []
            var changed = false
            for tab in cached {
                let toRemove = tab.content.paneIds.filter { !keep($0) }
                if toRemove.isEmpty {
                    remaining.append(tab)
                } else {
                    changed = true
                    var empty = false
                    for id in toRemove { empty = tab.content.remove(pane: id) }
                    if empty {
                        tab.content.detachAll()
                        tab.isClosed = true
                    } else {
                        remaining.append(tab)
                    }
                }
            }
            if changed {
                syncingHiddenTabs = true
                cachedTabs = remaining.isEmpty ? nil : remaining
                hiddenTabs = remaining.map { $0.tabLayout() }
                hiddenSelectedTab = remaining.isEmpty ? 0 : min(max(0, hiddenSelectedTab), remaining.count - 1)
                syncingHiddenTabs = false
            }
        } else {
            let kept = retainTabs(hiddenTabs, keep)
            if kept != hiddenTabs {
                hiddenTabs = kept
            }
            hiddenSelectedTab = min(hiddenSelectedTab, max(0, hiddenTabs.count - 1))
        }
    }

    /// Drops cached live tabs and detaches their views.
    func invalidateCachedTabs() {
        guard let old = cachedTabs else { return }
        cachedTabs = nil
        for tab in old {
            tab.content.detachAll()
            tab.isClosed = true
        }
    }
}

/// Default workspace names ("quiet-otter"), like tty7's codenames.
enum Codename {
    private static let adjectives = [
        "amber", "bold", "brisk", "calm", "clever", "cosmic", "crisp", "dapper", "eager", "fuzzy",
        "gentle", "golden", "happy", "hidden", "jolly", "keen", "lucky", "mellow", "misty", "nimble",
        "noble", "polar", "proud", "quiet", "rapid", "rusty", "shiny", "silent", "sly", "sunny",
        "swift", "tidy", "velvet", "vivid", "wild", "witty", "young", "zesty",
    ]
    private static let nouns = [
        "badger", "beacon", "comet", "cedar", "falcon", "fern", "fox", "harbor", "heron", "island",
        "koala", "lark", "lynx", "maple", "meadow", "moose", "nebula", "orca", "otter", "owl",
        "panda", "pebble", "pine", "puffin", "quartz", "raven", "river", "robin", "sparrow",
        "summit", "tiger", "tundra", "walrus", "willow", "wolf", "yak", "zebra",
    ]

    static func make(avoiding taken: Set<String>) -> String {
        for _ in 0..<64 {
            let name = "\(adjectives.randomElement()!)-\(nouns.randomElement()!)"
            if !taken.contains(name) { return name }
        }
        var n = 2
        while taken.contains("workspace-\(n)") { n += 1 }
        return "workspace-\(n)"
    }
}

/// An NSMenuItem that runs a closure.
final class BlockMenuItem: NSMenuItem {
    private let block: () -> Void

    init(_ title: String, key: String = "", mods: NSEvent.ModifierFlags = [.command], block: @escaping () -> Void) {
        self.block = block
        super.init(title: title, action: #selector(run), keyEquivalent: key)
        keyEquivalentModifierMask = mods
        target = self
    }

    required init(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    @objc private func run() { block() }
}

extension SessionManager {
    func workspace(_ id: UInt64) -> Workspace? {
        workspaces.first { $0.id == id }
    }

    /// Takes `pane`'s own tab out of the background workspaces; one left empty goes too.
    func takeHiddenTab(_ pane: PaneKey) {
        for ws in workspaces where !isShown(ws) {
            if ws.removeSinglePaneTab(pane) {
                workspaces.removeAll { $0 === ws }
            }
        }
    }

    /// The workspace showing `pane`, or holding it in a hidden tab.
    func workspace(containing pane: PaneKey) -> Workspace? {
        if let t = tab(for: pane) { return workspace(t.workspaceID) }
        return workspaces.first { $0.hiddenTabs.contains { $0.panes.contains(pane) } }
    }

    var currentWorkspace: Workspace? {
        currentController.flatMap { workspace($0.workspaceID) }
    }

    @discardableResult
    func makeWorkspace(name: String? = nil, host: HostId = localHost) -> Workspace {
        let id = (workspaces.map(\.id).max() ?? 0) + 1
        let taken = Set(workspaces.map(\.name))
        // A remote host's first workspace is named after it.
        var fallback = Codename.make(avoiding: taken)
        if host != localHost {
            fallback = taken.contains(host) ? "\(host)-\(Codename.make(avoiding: taken))" : host
        }
        let ws = Workspace(id: id, name: name ?? fallback, host: host)
        workspaces.append(ws)
        return ws
    }

    /// The controllers of `c`'s window (its tab group), in tab order.
    func group(of c: TerminalWindowController) -> [TerminalWindowController] {
        guard let w = c.window else { return [c] }
        return (w.tabGroup?.windows ?? [w])
            .compactMap { $0.windowController as? TerminalWindowController }
            .filter { !$0.isClosed }
    }

    /// Every live tab of `c`'s window (hosted `TabItem`s in sidebar mode, plus any sibling
    /// native `tabGroup` controllers' tabs in native mode), in tab order.
    func windowTabs(of c: TerminalWindowController) -> [TabItem] {
        group(of: c).flatMap(\.tabs).filter { !$0.isClosed }
    }

    /// A controller of the window showing `ws`, preferring its selected tab.
    func controllerShowing(_ ws: Workspace) -> TerminalWindowController? {
        let shown = liveControllers.filter { $0.workspaceID == ws.id }
        return shown.first { $0.window?.tabGroup?.selectedWindow === $0.window } ?? shown.first
    }

    func isShown(_ ws: Workspace) -> Bool { controllerShowing(ws) != nil }

    /// Most recently used first.
    var workspacesByRecency: [Workspace] {
        workspaces.sorted { $0.lastActive > $1.lastActive }
    }

    /// Makes the window of `c` show `target` in place: its tabs replace the window's tabs,
    /// whose panes keep running in the (now hidden) previous workspace. When another window
    /// shows `target` already, that window comes to the front instead.
    func switchWorkspace(in c: TerminalWindowController, to target: Workspace) {
        // The quick terminal shows no workspace: switch the last regular window.
        if c.isQuick {
            if let r = currentRegularController {
                switchWorkspace(in: r, to: target)
            } else {
                openWorkspaceInNewWindow(target)
            }
            return
        }
        guard target.id != c.workspaceID else { return }
        if let other = controllerShowing(target) {
            (other.window?.tabGroup?.selectedWindow ?? other.window)?.makeKeyAndOrderFront(nil)
            return
        }
        guard let hostWindow = c.window else { return }

        // Single-window sidebar mode: swap the active TabContentView in place on the same
        // TerminalWindow / TabSplitViewController / TabSidebarViewController — zero NSWindow churn!
        if c.usesSingleWindowTabs && group(of: c).count == 1 {
            let targetTabs: [TabItem]
            let targetSelected: Int
            if let cached = target.takeCachedTabs() {
                targetTabs = cached.tabs
                targetSelected = cached.selected
            } else {
                var created = target.hiddenTabs.map { TabItem(layout: $0) }
                if created.isEmpty {
                    let size = gridSize(forPoints: c.content.bounds.size)
                    if let pane = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil, host: target.host) {
                        created.append(TabItem(root: .leaf(pane), focused: pane, zoomed: nil))
                    }
                }
                guard !created.isEmpty else { return }
                targetSelected = min(max(0, target.hiddenSelectedTab), created.count - 1)
                target.hiddenTabs = []
                target.hiddenSelectedTab = 0
                targetTabs = created
            }

            let previousWorkspace = workspace(c.workspaceID)
            let (oldTabs, oldSelected) = c.replaceTabs(targetTabs, selectedIndex: targetSelected, workspaceID: target.id)
            if let previousWorkspace {
                previousWorkspace.parkTabs(oldTabs, selected: oldSelected)
            } else {
                let keptIDs = Set(targetTabs.map(ObjectIdentifier.init))
                for tab in oldTabs where !keptIDs.contains(ObjectIdentifier(tab)) {
                    tab.content.detachAll()
                    tab.isClosed = true
                }
            }
            prewarmWorkspaceTabs(targetTabs, excluding: c.selectedTab)
            c.window?.makeKeyAndOrderFront(nil)
            c.sidebar?.reload()
            if let window = c.window, let view = c.content.focusedView,
               !((window.firstResponder as? NSView)?.isDescendant(of: c.content) ?? false) {
                window.makeFirstResponder(view)
            }
            c.content.focusedView?.reportFocus()
            tabSelected(c.selectedTab, in: c)
            return
        }

        // Native-tab mode: batch window creation, tabGroup attachment, and old-window teardown
        // under `isSwitchingWorkspace` to suppress KVO storms and redundant layout passes.
        let old = group(of: c)
        let selectedWindow = hostWindow.tabGroup?.selectedWindow ?? hostWindow
        if let current = workspace(c.workspaceID) {
            let kept = old.flatMap(\.tabs).filter { !$0.content.isEmpty }
            let selController = selectedWindow.windowController as? TerminalWindowController
            let selTab = selController?.selectedTab
            current.hiddenTabs = kept.map { $0.tabLayout() }
            current.hiddenSelectedTab = selTab.flatMap { st in kept.firstIndex { $0 === st } } ?? 0
        }

        var created: [TerminalWindowController] = []
        var selected: TerminalWindowController?
        withWorkspaceSwitchBatch {
            var anchor = selectedWindow
            func add(_ nc: TerminalWindowController) {
                nc.workspaceID = target.id
                guard let w = nc.window else { return }
                w.setFrame(selectedWindow.frame, display: false)
                anchor.addTabbedWindow(w, ordered: .above)
                anchor = w
                created.append(nc)
            }
            for tab in target.hiddenTabs {
                let t = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey, zoomed: tab.zoomedKey,
                                       title: tab.title, frame: nil, floating: tab.floating,
                                       floatingHidden: tab.floatingHidden)
                t.adopt(tab)
                add(t)
            }
            if created.isEmpty {
                let size = gridSize(forPoints: c.content.bounds.size)
                if let pane = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil, host: target.host) {
                    add(makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil))
                }
            }
            guard !created.isEmpty else { return }
            let chosen = created[min(max(0, target.hiddenSelectedTab), created.count - 1)]
            selected = chosen
            target.hiddenTabs = []
            target.hiddenSelectedTab = 0
            target.lastActive = Date()
            // The old tabs leave every listing now; only lay out `selected`'s sidebar eagerly
            // (non-selected tabs defer until shown).
            for o in old { o.isClosed = true }
            let shownSplit = (selectedWindow.windowController as? TerminalWindowController)?.tabSplit
            chosen.tabSplit?.match(shownSplit)
            for n in created where n !== chosen {
                n.pendingSidebarMatch = chosen.tabSplit ?? shownSplit
            }
            chosen.window?.makeKeyAndOrderFront(nil)
            for o in old {
                o.closingWithoutConfirmation = true
                o.window?.close()
            }
        }
        guard let selected else { return }
        for n in created { n.updateTitle() }
        (selected.window as? TerminalWindow)?.finishWorkspaceSwitchSync()
        prewarmWorkspaceTabs(created.flatMap(\.tabs), excluding: selected.selectedTab)
        refreshSidebars()
        focusChanged()
        scheduleLayoutSave()
    }

    /// A hidden workspace's tab clicked in the sidebar: `c`'s window switches to the workspace,
    /// on that tab.
    func showHiddenTab(_ tab: HiddenTab, from c: TerminalWindowController?) {
        guard let ws = workspace(tab.workspace), tab.index < ws.hiddenTabs.count else { return }
        ws.hiddenSelectedTab = tab.index
        if let c = c ?? currentController {
            switchWorkspace(in: c, to: ws)
        } else {
            openWorkspaceInNewWindow(ws)
        }
    }

    func togglePin(_ tab: TabItem) {
        tab.pinned.toggle()
        scheduleLayoutSave()
    }

    func togglePin(_ c: TerminalWindowController) {
        togglePin(c.selectedTab)
    }

    func isPinned(_ tab: HiddenTab) -> Bool {
        guard let ws = workspace(tab.workspace), tab.index < ws.hiddenTabs.count else { return false }
        return ws.hiddenTabs[tab.index].pinned == true
    }

    func togglePin(_ tab: HiddenTab) {
        guard let ws = workspace(tab.workspace), tab.index < ws.hiddenTabs.count else { return }
        ws.toggleHiddenPin(at: tab.index)
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// Adds `pane` to hidden workspace `ws`. With `reveal`, select the new tab in `c`'s window,
    /// or open a window if there is none. Otherwise, keep the workspace hidden.
    func addHiddenTab(_ pane: PaneKey, handoff: String?, to ws: Workspace, in c: TerminalWindowController?,
                      reveal: Bool) {
        if ws.cachedTabs != nil {
            let tab = TabItem(root: .leaf(pane), focused: pane, zoomed: nil)
            tab.handoffID = handoff
            ws.appendParkedTab(tab)
        } else {
            ws.hiddenTabs.append(TabLayout(title: nil, root: .pane(pane), focused: pane.id, zoomed: nil, handoff: handoff))
        }
        if reveal {
            ws.hiddenSelectedTab = ws.hiddenTabs.count - 1
            if let c {
                switchWorkspace(in: c, to: ws)
            } else {
                openWorkspaceInNewWindow(ws)
            }
        } else {
            let label = ws.isRemote ? "\(ws.name) (\(ws.host))" : ws.name
            (currentRegularController ?? c)?.content.focusedView?.showToast(
                "New tab in workspace \(label) · ⇧⌘O to switch", duration: 5)
            refreshSidebars()
            scheduleLayoutSave()
        }
    }

    /// Moves `tab` out of its window into a new workspace, in the background.
    func moveTabToNewWorkspace(_ tab: TabItem) {
        guard let c = tab.windowController, !c.isQuick, windowTabs(of: c).count > 1 else {
            NSSound.beep()
            return
        }
        moveInBackground(tab, to: makeWorkspace(host: workspace(c.workspaceID)?.host ?? tab.host))
    }

    /// Moves `c`'s selected tab out of its window into a new workspace, in the background.
    func moveTabToNewWorkspace(_ c: TerminalWindowController) {
        moveTabToNewWorkspace(c.selectedTab)
    }

    private func moveInBackground(_ tab: TabItem, to ws: Workspace) {
        guard let c = tab.windowController else { return }
        if c.usesSingleWindowTabs && group(of: c).count == 1 && c.tabs.count > 1 {
            _ = c.removeTab(tab, detach: false)
            ws.appendParkedTab(tab)
        } else {
            ws.hiddenTabs.append(tab.tabLayout())
            c.closingWithoutConfirmation = true
            c.window?.close()
        }
        currentRegularController?.content.focusedView?.showToast("Tab moved to workspace \(ws.name)", duration: 4)
        refreshSidebars()
        scheduleLayoutSave()
    }

    func moveTargets(for tab: TabItem) -> [Workspace] {
        guard let c = tab.windowController, !c.isQuick, !c.isClosed, !tab.isClosed,
              c.window != nil, tab.wall == nil else { return [] }
        let host = workspace(c.workspaceID)?.host ?? tab.host
        return workspacesByRecency.filter { $0.id != c.workspaceID && $0.host == host && !isShown($0) }
    }

    func moveTargets(for c: TerminalWindowController) -> [Workspace] {
        moveTargets(for: c.selectedTab)
    }

    func moveTab(_ tab: TabItem, to ws: Workspace) {
        guard moveTargets(for: tab).contains(where: { $0 === ws }), let c = tab.windowController else {
            NSSound.beep()
            return
        }
        if windowTabs(of: c).count > 1 {
            moveInBackground(tab, to: ws)
            return
        }
        if c.usesSingleWindowTabs && group(of: c).count == 1 {
            tab.content.removeFromSuperview()
            ws.appendParkedTab(tab)
            workspaces.removeAll { $0.id == c.workspaceID }
            c.workspaceID = 0
            ws.hiddenSelectedTab = ws.hiddenTabs.count - 1
            switchWorkspace(in: c, to: ws)
            currentController?.content.focusedView?.showToast("Tab moved to workspace \(ws.name)", duration: 4)
            return
        }
        ws.hiddenTabs.append(tab.tabLayout())
        c.content.isHidden = true
        c.content.detachAll()
        workspaces.removeAll { $0.id == c.workspaceID }
        c.workspaceID = 0
        ws.hiddenSelectedTab = ws.hiddenTabs.count - 1
        switchWorkspace(in: c, to: ws)
        currentController?.content.focusedView?.showToast("Tab moved to workspace \(ws.name)", duration: 4)
    }

    func moveTab(_ c: TerminalWindowController, to ws: Workspace) {
        moveTab(c.selectedTab, to: ws)
    }

    func fillMoveTabMenu(_ menu: NSMenu, forTab tab: TabItem?) {
        menu.removeAllItems()
        let targets = tab.map(moveTargets(for:)) ?? []
        for ws in targets {
            menu.addItem(BlockMenuItem(TabSidebarViewController.heading(ws)) { [weak tab] in
                guard let tab else { return }
                SessionManager.shared.moveTab(tab, to: ws)
            })
        }
        if targets.isEmpty {
            let none = NSMenuItem(title: "No Other Workspaces", action: nil, keyEquivalent: "")
            none.isEnabled = false
            menu.addItem(none)
        }
    }

    func fillMoveTabMenu(_ menu: NSMenu, for c: TerminalWindowController?) {
        fillMoveTabMenu(menu, forTab: c?.selectedTab)
    }

    /// Thurm has one window. Another one (a tab dragged out of the tab bar) goes into a
    /// workspace in the background: the window with the most tabs stays, on a tie the one that
    /// is not key (a dragged-out tab becomes key).
    func foldExtraWindows() {
        var groups: [[TerminalWindowController]] = []
        for c in regularControllers where c.window != nil && !groups.contains(where: { $0.contains { $0 === c } }) {
            groups.append(group(of: c))
        }
        guard groups.count > 1 else { return }
        let key = NSApp.keyWindow
        func rank(_ g: [TerminalWindowController]) -> (Int, Int) {
            (g.flatMap(\.tabs).count, g.contains { $0.window === key } ? 0 : 1)
        }
        let keep = groups.indices.max { rank(groups[$0]) < rank(groups[$1]) } ?? 0
        let keptWorkspace = groups[keep].first?.workspaceID
        for (i, g) in groups.enumerated() where i != keep {
            let tabs = g.flatMap(\.tabs).filter { !$0.content.isEmpty }
            var ws = g.first.flatMap { workspace($0.workspaceID) }
            if ws == nil || ws?.id == keptWorkspace {
                ws = makeWorkspace(host: g.first?.host ?? localHost)
            }
            for c in g {
                c.closingWithoutConfirmation = true
                c.window?.close()
            }
            guard let ws, !tabs.isEmpty else { continue }
            ws.hiddenTabs += tabs.map { $0.tabLayout() }
            groups[keep].first?.content.focusedView?.showToast(
                "\(tabs.count == 1 ? "Tab" : "Tabs") moved to workspace \(ws.name)", duration: 4)
        }
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// Shows a hidden workspace in a new window (restore with no window left).
    func openWorkspaceInNewWindow(_ ws: Workspace, frame: NSRect? = nil) {
        if config.sidebarTabs {
            let items: [TabItem]
            let selected: Int
            if let cached = ws.takeCachedTabs() {
                items = cached.tabs
                selected = cached.selected
            } else {
                items = ws.hiddenTabs.map { TabItem(layout: $0) }
                guard !items.isEmpty else { return }
                selected = min(max(0, ws.hiddenSelectedTab), items.count - 1)
                ws.hiddenTabs = []
                ws.hiddenSelectedTab = 0
            }
            let c = makeController(tabs: items, selectedIndex: selected, frame: frame)
            c.workspaceID = ws.id
            showAsNewWindow(c, frame: frame)
            ws.lastActive = Date()
            prewarmWorkspaceTabs(items, excluding: c.selectedTab)
            scheduleLayoutSave()
            return
        }
        var created: [TerminalWindowController] = []
        withWorkspaceSwitchBatch {
            var previous: TerminalWindowController?
            for tab in ws.hiddenTabs {
                let c = makeController(root: SplitNode(layout: tab.root), focused: tab.focusedKey, zoomed: tab.zoomedKey,
                                       title: tab.title, frame: frame, floating: tab.floating,
                                       floatingHidden: tab.floatingHidden)
                c.workspaceID = ws.id
                c.adopt(tab)
                if let prev = previous { attachAsTab(c, to: prev) } else { showAsNewWindow(c, frame: frame) }
                previous = c
                created.append(c)
            }
            guard !created.isEmpty else { return }
            created[min(max(0, ws.hiddenSelectedTab), created.count - 1)].window?.makeKeyAndOrderFront(nil)
            ws.hiddenTabs = []
            ws.lastActive = Date()
        }
        guard !created.isEmpty else { return }
        for c in created { c.updateTitle() }
        if let sel = created.first(where: { $0.window?.tabGroup?.selectedWindow === $0.window }) ?? created.first {
            (sel.window as? TerminalWindow)?.finishWorkspaceSwitchSync()
        }
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// A `thurm://workspace/<name>` URL: brings the named workspace forward, opening it when hidden.
    /// Unknown names do nothing; no workspace is created.
    func showWorkspace(named name: String) {
        guard let ws = workspaces.first(where: { $0.name.caseInsensitiveCompare(name) == .orderedSame }) else {
            return
        }
        if let c = controllerShowing(ws) {
            c.window?.makeKeyAndOrderFront(nil)
        } else if let c = currentRegularController {
            switchWorkspace(in: c, to: ws)
        } else {
            openWorkspaceInNewWindow(ws)
        }
    }

    /// A `thurm://new-workspace` URL: a new local workspace whose shell starts in `cwd`, shown
    /// in the front window.
    func newWorkspace(cwd: String?) {
        let size = gridSize(forPoints: currentRegularController?.content.bounds.size ?? defaultContentSize())
        guard let pane = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil, cwd: cwd) else { return }
        newWindow(pane: pane, workspace: makeWorkspace(host: pane.host))
    }

    /// ⌘⇧N: a new workspace with a fresh shell (on `host`), in the front window.
    func newWorkspace(host: HostId = localHost) {
        let ws = makeWorkspace(host: host)
        if let c = currentController {
            switchWorkspace(in: c, to: ws)
        } else {
            newWindow(workspace: ws)
        }
        currentController?.content.focusedView?.showToast("Workspace \(ws.name)")
    }

    func renameWorkspace(_ ws: Workspace) {
        let alert = NSAlert()
        alert.messageText = "Rename Workspace"
        let field = NSTextField(string: ws.name)
        field.frame = NSRect(x: 0, y: 0, width: 260, height: 24)
        alert.accessoryView = field
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let name = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        ws.name = name
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// Ends the workspace's shells and forgets it. Its window moves on to the most recent
    /// hidden workspace, or closes when there is none.
    func closeWorkspace(_ ws: Workspace) {
        let shownIn = controllerShowing(ws)
        let tabs = shownIn.map { windowTabs(of: $0).count } ?? ws.hiddenTabs.count
        let alert = NSAlert()
        alert.messageText = "Close workspace “\(ws.name)”?"
        alert.informativeText = "Its \(tabs) \(tabs == 1 ? "tab" : "tabs") and the programs running in them are closed."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Close Workspace")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        let ids = shownIn.map { windowTabs(of: $0).flatMap { $0.content.paneIds } }
            ?? ws.hiddenTabs.flatMap { $0.panes }
        for id in ids { sendClosePane(id) }
        ws.invalidateCachedTabs()
        workspaces.removeAll { $0 === ws }
        if let c = shownIn {
            let old = group(of: c)
            for o in old { o.workspaceID = 0 }
            if let next = workspacesByRecency.first(where: { !isShown($0) }) {
                switchWorkspace(in: c, to: next)
            } else {
                for o in old {
                    o.closingWithoutConfirmation = true
                    o.window?.close()
                }
            }
        }
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// `c`, the last tab of its workspace, is closing: its window moves on to the most recent
    /// hidden workspace instead of closing (which would quit the app), and the emptied workspace
    /// goes. False when there is no other workspace to show.
    func showNextWorkspace(closing c: TerminalWindowController) -> Bool {
        guard !isTerminating, !c.isQuick, !c.isClosed, windowTabs(of: c).count == 1,
              let next = workspacesByRecency.first(where: { $0.id != c.workspaceID && !isShown($0) })
        else { return false }
        workspaces.removeAll { $0.id == c.workspaceID }
        c.workspaceID = 0
        switchWorkspace(in: c, to: next)
        return c.isClosed || c.workspaceID == next.id
    }

    /// ⌘⇧O: pick a workspace by name.
    func showWorkspaceSwitcher() {
        let current = currentController?.workspaceID
        var items: [CommandPalette.Item] = workspacesByRecency.map { ws in
            let tabs = isShown(ws) ? (controllerShowing(ws).map { windowTabs(of: $0).count } ?? 0) : ws.hiddenTabs.count
            var detail = "\(tabs) \(tabs == 1 ? "tab" : "tabs")"
            if ws.isRemote { detail = "\(ws.host) · \(Remotes.shared.phaseLabel(ws.host)) · " + detail }
            if ws.id == current {
                detail += " · this window"
            } else if isShown(ws) {
                detail += " · open"
            } else {
                detail += " · \(Self.relative(ws.lastActive))"
            }
            let rename = {
                SessionManager.shared.renameWorkspace(ws)
                SessionManager.shared.showWorkspaceSwitcher()
            }
            return CommandPalette.Item(title: ws.name, detail: detail, rename: rename) {
                if let c = SessionManager.shared.currentController {
                    SessionManager.shared.switchWorkspace(in: c, to: ws)
                } else {
                    SessionManager.shared.openWorkspaceInNewWindow(ws)
                }
            }
        }
        items.append(CommandPalette.Item(title: "New Workspace", detail: "", shortcut: "⌘N") {
            SessionManager.shared.newWorkspace()
        })
        for host in config.remoteNames {
            items.append(CommandPalette.Item(title: "New Workspace on \(host)",
                                             detail: Remotes.shared.phaseLabel(host)) {
                SessionManager.shared.newWorkspace(host: host)
            })
        }
        CommandPalette.shared.show(items: items, over: currentController?.window,
                                   placeholder: "Switch to workspace…", footer: "↩ switch   ⌘R rename")
    }

    private static func relative(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return f.localizedString(for: date, relativeTo: Date())
    }

    /// The workspace list plus actions for Window > Workspaces.
    func fillWorkspaceMenu(_ menu: NSMenu) {
        menu.removeAllItems()
        let current = currentController?.workspaceID
        for (i, ws) in workspacesByRecency.enumerated() {
            let item = BlockMenuItem(ws.name, key: i < 9 ? "\(i + 1)" : "", mods: [.command, .control]) {
                if let c = SessionManager.shared.currentController {
                    SessionManager.shared.switchWorkspace(in: c, to: ws)
                } else {
                    SessionManager.shared.openWorkspaceInNewWindow(ws)
                }
            }
            item.state = ws.id == current ? .on : .off
            menu.addItem(item)
        }
        menu.addItem(.separator())
        menu.addItem(BlockMenuItem("New Workspace", key: "n", mods: [.command]) {
            SessionManager.shared.newWorkspace()
        })
        menu.addItem(BlockMenuItem("Switch Workspace…", key: "o", mods: [.command, .shift]) {
            SessionManager.shared.showWorkspaceSwitcher()
        })
        if let ws = currentWorkspace {
            menu.addItem(BlockMenuItem("Rename Workspace…") { SessionManager.shared.renameWorkspace(ws) })
            menu.addItem(BlockMenuItem("Close Workspace…") { SessionManager.shared.closeWorkspace(ws) })
        }
    }

    // MARK: Layout

    /// One workspace per window: the group's selected tab decides; a window that has none, an
    /// unknown one, or one another window shows already (a tab dragged out into a new window)
    /// gets a new workspace. The larger window keeps a shared workspace.
    func normalizeWorkspaces(groups: [[TerminalWindowController]], selected: [TerminalWindowController]) {
        var owner: [UInt64: Int] = [:]
        let bySize = groups.indices.sorted { groups[$0].flatMap(\.tabs).count > groups[$1].flatMap(\.tabs).count }
        var assigned = [UInt64](repeating: 0, count: groups.count)
        for gi in bySize {
            var id = selected[gi].workspaceID
            if id == 0 || workspace(id) == nil || owner[id] != nil {
                id = makeWorkspace(host: selected[gi].host).id
            }
            owner[id] = gi
            assigned[gi] = id
        }
        for (gi, g) in groups.enumerated() {
            for c in g { c.workspaceID = assigned[gi] }
        }
    }

    func workspaceLayouts() -> [WorkspaceLayout] {
        let shown = Set(liveControllers.map(\.workspaceID))
        // Hidden workspaces lose panes that ended meanwhile, and go when none is left.
        for ws in workspaces where !shown.contains(ws.id) {
            // An offline host's panes are unknown until it is back: keep them.
            ws.retainHiddenPanes { key in
                panes[key] != nil || (key.isRemote && !Core.shared.isConnected(key.host))
            }
        }
        workspaces.removeAll { !shown.contains($0.id) && $0.hiddenTabs.isEmpty }
        return workspaces.map { ws in
            let hidden = !shown.contains(ws.id)
            return WorkspaceLayout(id: ws.id, name: ws.name, lastActive: UInt64(ws.lastActive.timeIntervalSince1970),
                                   tabs: hidden ? ws.hiddenTabs : [], selectedTab: hidden ? ws.hiddenSelectedTab : 0,
                                   host: ws.isRemote ? ws.host : nil)
        }
    }
}
