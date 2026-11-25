import AppKit

/// The agent wall (⌃⌘A, `thurm wall`): the agents of a window's tabs gathered into one grid tab,
/// to watch them all at once, then put back in their tabs, splits and places. Panes move; their
/// programs keep running. The wall tab keeps the tabs they came from (`wall`), so it outlives a
/// restart.
enum AgentWall {
    /// Width over height of the terminal a grid cell should come closest to (about 100×30).
    static let cellAspect: CGFloat = 1.6

    /// Panes per row for `count` panes in `size`: rows of near-equal length, the longer ones on
    /// top, as many as make the cells closest to `cellAspect` on average (fewer rows on a tie).
    static func rows(count: Int, in size: CGSize) -> [Int] {
        guard count > 0 else { return [] }
        let area = size.width > 0 && size.height > 0 ? size : CGSize(width: 1600, height: 1000)
        var best = [count]
        var bestScore = CGFloat.infinity
        for r in 1...count {
            let rows = (0..<r).map { count / r + ($0 < count % r ? 1 : 0) }
            let height = area.height / CGFloat(r)
            let score = rows.reduce(0) { sum, n in
                sum + CGFloat(n) * abs(log(area.width / CGFloat(n) / height / cellAspect))
            } / CGFloat(count)
            if score < bestScore - 0.001 {
                bestScore = score
                best = rows
            }
        }
        return best
    }

    /// `panes` in rows of `rows` panes each, every pane in a row the same width and every row
    /// the same height.
    static func grid(_ panes: [PaneKey], rows: [Int]) -> SplitNode? {
        var rest = panes[...]
        let lines: [SplitNode] = rows.compactMap { n in
            let row = rest.prefix(n)
            rest = rest.dropFirst(n)
            return chain(row.map { .leaf($0) }, axis: .horizontal)
        }
        return chain(lines, axis: .vertical)?.equalized()
    }

    private static func chain(_ nodes: [SplitNode], axis: SplitAxis) -> SplitNode? {
        guard let last = nodes.last else { return nil }
        return nodes.dropLast().reversed().reduce(last) { .split(axis: axis, ratio: 0.5, first: $1, second: $0) }
    }

    /// Where an origin tab's agents go back to.
    struct Return: Equatable {
        var origin: WallOrigin
        /// The wall's panes that go back.
        var panes: [PaneKey]
        /// The tab's layout once they are back.
        var root: SplitNode
        /// Those that floated in the tab, floating again.
        var floating: [FloatingPane] = []
        /// The open tab (of `tabs`) that kept the rest of the origin's panes; nil for a new tab.
        var into: Int?

        static func == (a: Return, b: Return) -> Bool {
            a.origin == b.origin && a.panes == b.panes && a.root.layoutNode == b.root.layoutNode
                && a.floating == b.floating && a.into == b.into
        }
    }

    /// Putting the wall back, for each origin by its old place: the panes still on the wall go
    /// back to their spots in the tab's layout. A tab that kept some of its panes takes them, with
    /// the panes it got meanwhile on its right (a later return to it has the earlier one's); the
    /// others come back as new tabs. Agents that ended, or have nowhere to go, stay on the wall.
    /// Panes that floated in their tab float again; `floating` holds the open tabs' own floating
    /// panes, which stay as they are.
    static func returns(origins: [WallOrigin], onWall: [PaneKey], tabs: [[PaneKey]],
                        floating: [Set<PaneKey>] = []) -> [Return] {
        var left = Set(onWall)
        var tabs = tabs
        var floating = floating
        var out: [Return] = []
        for origin in origins.sorted(by: { $0.index < $1.index }) {
            let panes = origin.tab.panes
            let back = panes.filter { left.contains($0) }
            guard !back.isEmpty else { continue }
            left.subtract(back)
            let into = tabs.firstIndex { tab in panes.contains { !back.contains($0) && tab.contains($0) } }
            let cards = into.flatMap { $0 < floating.count ? floating[$0] : nil } ?? []
            let kept = into.map { tabs[$0].filter { !cards.contains($0) } } ?? []
            var floats = (origin.tab.floating ?? []).filter { back.contains($0.pane) }
            // Only floating panes come back to a new tab: the frontmost one takes the splits.
            guard let layout = origin.tab.root.retaining({ back.contains($0) || kept.contains($0) })
                ?? (kept.isEmpty ? floats.popLast().map({ .pane($0.pane) }) : nil) else { continue }
            var root = SplitNode(layout: layout)
            for pane in kept where !root.contains(pane) {
                if let last = root.panes.last { root = root.inserting(pane, at: last, direction: .right) }
            }
            // Two origins can go back to one tab (their shells moved together): the next one
            // keeps what this one put back.
            if let into {
                tabs[into] = root.panes + cards + floats.map(\.pane)
                // The panes put back as cards stay cards for a later return to the same tab.
                while floating.count <= into { floating.append([]) }
                floating[into].formUnion(floats.map(\.pane))
            }
            out.append(Return(origin: origin, panes: back, root: root, floating: floats, into: into))
        }
        return out
    }

    /// Where a tab that comes back goes among `tabs`: after the tab holding pane `after`, first
    /// without one, else at `index`.
    static func position(after: UInt64?, index: Int, in tabs: [[PaneKey]]) -> Int {
        guard let after else { return 0 }
        if let i = tabs.firstIndex(where: { $0.contains { $0.id == after } }) { return i + 1 }
        return min(index, tabs.count)
    }

    /// A pane of the tab before `i` in `tabs`, for `WallOrigin.after`.
    static func anchor(before i: Int, in tabs: [TabItem]) -> UInt64? {
        i > 0 ? tabs[i - 1].content.paneIds.first?.id : nil
    }

    /// A pane of the tab before `i` in `tabs`, for `WallOrigin.after`.
    static func anchor(before i: Int, in tabs: [TerminalWindowController]) -> UInt64? {
        i > 0 ? tabs[i - 1].content.paneIds.first?.id : nil
    }
}

extension SessionManager {
    /// The wall tab of `c`'s window, if one is up.
    func agentWallTab(near c: TerminalWindowController) -> TabItem? {
        windowTabs(of: c).first { $0.wall != nil }
    }

    /// The wall tab's window controller, if one is up.
    func agentWall(near c: TerminalWindowController) -> TerminalWindowController? {
        agentWallTab(near: c)?.windowController
    }

    func isWallOrigin(_ tab: TabItem) -> Bool {
        guard let c = tab.windowController else { return false }
        let here = Set(tab.content.paneIds)
        return agentWallTab(near: c)?.wall?.contains { $0.tab.panes.contains(where: here.contains) } == true
    }

    func isWallOrigin(_ c: TerminalWindowController) -> Bool {
        isWallOrigin(c.selectedTab)
    }

    /// Gathers the agents of `c`'s window into a wall (`on` true), puts them back (false), or
    /// toggles.
    func toggleAgentWall(in c: TerminalWindowController, on: Bool? = nil) {
        guard !c.isQuick else {
            NSSound.beep()
            return
        }
        if let wall = agentWallTab(near: c) {
            if on != true { putBackAgents(wall) }
        } else if on != false {
            gatherAgents(in: c)
        }
    }

    private func runsAgent(_ pane: PaneKey) -> Bool { panes[pane]?.agent != nil }

    /// The size the wall's grid is for: the shown tab's (a tab in the background is not laid out).
    private func wallArea(near c: TerminalWindowController) -> CGSize {
        let shown = c.window?.tabGroup?.selectedWindow?.windowController as? TerminalWindowController
        return (shown ?? c).content.bounds.size
    }

    private func gatherAgents(in c: TerminalWindowController) {
        // Handoff tabs stay: closing one asks about its worktree.
        var taken: [(tab: TabItem, agents: [PaneKey])] = []
        var origins: [WallOrigin] = []
        let tabs = windowTabs(of: c).filter { !$0.isQuick }
        for (i, t) in tabs.enumerated() where t.handoffID == nil {
            let agents = t.content.paneIds.filter(runsAgent)
            guard !agents.isEmpty else { continue }
            taken.append((t, agents))
            origins.append(WallOrigin(index: i, after: AgentWall.anchor(before: i, in: tabs),
                                      tab: t.tabLayout()))
        }
        let agents = taken.flatMap(\.agents)
        guard agents.count > 1, let first = taken.first?.tab,
              let root = AgentWall.grid(agents, rows: AgentWall.rows(count: agents.count, in: wallArea(near: c)))
        else {
            c.content.focusedView?.showToast(agents.isEmpty ? "No agents in this window"
                                             : "One agent in this window: nothing to gather")
            return
        }
        let focused = agents.contains(c.focusedPane) ? c.focusedPane : agents[0]
        // Out of their tabs first, so their views there let go of the panes before the wall's
        // take them; emptied tabs close once the wall is in the window.
        var emptied: [TabItem] = []
        for (t, panes) in taken {
            var empty = false
            for pane in panes { empty = t.content.remove(pane: pane) }
            if empty {
                emptied.append(t)
            } else if t.isSelected {
                t.windowController?.updateTitle()
            }
        }
        if c.usesSingleWindowTabs && group(of: c).count == 1 {
            let wall = TabItem(root: root, focused: focused, zoomed: nil, title: "Agents")
            wall.wall = origins
            let at = c.tabs.firstIndex { $0 === first }.map { $0 + 1 } ?? c.tabs.count
            c.insertTab(wall, at: at, select: true)
            for t in emptied {
                _ = c.removeTab(t, detach: true)
            }
            c.window?.makeKeyAndOrderFront(nil)
            wall.content.focus(focused)
            focusChanged()
            scheduleLayoutSave()
            return
        }
        let wall = makeController(root: root, focused: focused, zoomed: nil, title: "Agents", frame: nil)
        wall.wall = origins
        if let anchor = first.windowController {
            attachAsTab(wall, to: anchor)
        }
        wall.window?.makeKeyAndOrderFront(nil)
        for t in emptied {
            if let wc = t.windowController {
                wc.closingWithoutConfirmation = true
                wc.window?.close()
            }
        }
        wall.content.focus(focused)
        focusChanged()
        scheduleLayoutSave()
    }

    private func putBackAgents(_ wall: TabItem) {
        guard let origins = wall.wall, let host = wall.windowController else { return }
        var tabs = windowTabs(of: host).filter { $0 !== wall && !$0.isQuick }
        let plan = AgentWall.returns(origins: origins, onWall: wall.content.paneIds,
                                     tabs: tabs.map(\.content.paneIds),
                                     floating: tabs.map { Set($0.content.floating.map(\.pane)) })
        let existing = tabs
        let focused = wall.content.focusedPane
        wall.wall = nil
        // Off the wall first (see gatherAgents).
        for r in plan {
            for pane in r.panes { wall.content.remove(pane: pane) }
        }
        let singleWindow = host.usesSingleWindowTabs && group(of: host).count == 1
        for r in plan {
            if let i = r.into {
                let t = existing[i]
                t.content.setRoot(r.root)
                t.content.addFloating(r.floating, hidden: r.origin.tab.floatingHidden ?? false)
                let key = r.origin.tab.focusedKey
                if t.content.contains(key) && !t.content.isCardHidden(key) {
                    t.content.setFocusedPane(key)
                }
                if let z = r.origin.tab.zoomedKey, r.root.contains(z) { t.content.zoom(z, true) }
                if t.isSelected { t.windowController?.updateTitle() }
                continue
            }
            let tab = r.origin.tab
            let pane = r.root.contains(tab.focusedKey) || r.floating.contains(where: { $0.pane == tab.focusedKey })
                ? tab.focusedKey : r.root.panes[0]
            let zoomed = tab.zoomedKey.flatMap { r.root.contains($0) ? $0 : nil }
            let at = AgentWall.position(after: r.origin.after, index: r.origin.index,
                                        in: tabs.map(\.content.paneIds))
            if singleWindow {
                let t = TabItem(root: r.root, focused: pane, zoomed: zoomed,
                                floating: r.floating, floatingHidden: tab.floatingHidden ?? false,
                                title: tab.title)
                t.pinned = tab.pinned ?? false
                let insertIdx: Int
                if at < tabs.count, let hostIdx = host.tabs.firstIndex(where: { $0 === tabs[at] }) {
                    insertIdx = hostIdx
                } else if let last = tabs.last, let hostIdx = host.tabs.firstIndex(where: { $0 === last }) {
                    insertIdx = hostIdx + 1
                } else {
                    insertIdx = host.tabs.count
                }
                host.insertTab(t, at: insertIdx, select: false)
                tabs.insert(t, at: at)
            } else {
                let t = makeController(root: r.root, focused: pane, zoomed: zoomed, title: tab.title, frame: nil,
                                       floating: r.floating, floatingHidden: tab.floatingHidden)
                t.workspaceID = host.workspaceID
                t.pinned = tab.pinned ?? false
                let anchor = at < tabs.count ? tabs[at].windowController : tabs.last?.windowController ?? host
                if let w = t.window, let anchorWindow = anchor?.window {
                    w.setFrame(anchorWindow.frame, display: false)
                    anchorWindow.addTabbedWindow(w, ordered: at < tabs.count ? .below : .above)
                }
                tabs.insert(t.selectedTab, at: at)
            }
        }
        if wall.content.isEmpty {
            if singleWindow {
                _ = host.removeTab(wall, detach: true)
            } else {
                host.closingWithoutConfirmation = true
                host.window?.close()
            }
        } else {
            // Agents with nowhere to go, or opened on the wall, stay as a tab of their own.
            wall.titleOverride = nil
        }
        focusPane(focused, activate: false)
        focusChanged()
        scheduleLayoutSave()
    }

    /// An agent started in `pane`, outside the wall of its window: it joins the wall, and goes
    /// back to its tab with the others. Not from a tab that already gave the wall agents: its
    /// layout there is already taken.
    func agentStarted(_ pane: PaneKey) {
        guard let t = tab(for: pane), let c = t.windowController, !c.isQuick, t.wall == nil, t.handoffID == nil,
              let wall = agentWallTab(near: c), var origins = wall.wall, !isWallOrigin(t) else { return }
        let tabs = windowTabs(of: c).filter { $0 !== wall && !$0.isQuick }
        let i = tabs.firstIndex { $0 === t } ?? tabs.count
        origins.append(WallOrigin(index: i, after: AgentWall.anchor(before: i, in: tabs),
                                  tab: t.tabLayout()))
        let watched = t.isSelected && (c.window.map { ($0.tabGroup?.selectedWindow ?? $0) === $0 } ?? false)
        let emptied = t.content.remove(pane: pane)
        let agents = wall.content.paneIds + [pane]
        let rows = AgentWall.rows(count: agents.count, in: wallArea(near: c))
        guard let root = AgentWall.grid(agents, rows: rows) else { return }
        wall.wall = origins
        wall.content.setRoot(root)
        if wall.isSelected { wall.windowController?.updateTitle() }
        if emptied {
            if c.usesSingleWindowTabs && group(of: c).count == 1 {
                _ = c.removeTab(t, detach: true)
            } else {
                c.closingWithoutConfirmation = true
                c.window?.close()
            }
        } else if t.isSelected {
            c.updateTitle()
        }
        // Its tab was showing: the wall shows instead, the keys still going to it.
        if watched { focusPane(pane, activate: false) }
        focusChanged()
        scheduleLayoutSave()
    }
}
