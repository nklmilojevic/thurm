import AppKit

struct TabHistory {
    static let limit = 50

    private(set) var entries: [[PaneKey]] = []
    private(set) var cursor = -1

    static func sameTab(_ a: [PaneKey], _ b: [PaneKey]) -> Bool {
        !Set(a).isDisjoint(with: b)
    }

    mutating func visit(_ panes: [PaneKey]) {
        guard !panes.isEmpty else { return }
        if entries.indices.contains(cursor), Self.sameTab(entries[cursor], panes) {
            entries[cursor] = panes
            return
        }
        entries.removeSubrange((cursor + 1)...)
        entries.append(panes)
        if entries.count > Self.limit { entries.removeFirst(entries.count - Self.limit) }
        cursor = entries.count - 1
    }

    mutating func tabChanged(from old: [PaneKey], to new: [PaneKey]) {
        guard !new.isEmpty else { return }
        let kept = old.filter(new.contains)
        let anchor = kept.isEmpty ? old : kept
        for i in entries.indices where Self.sameTab(entries[i], anchor) {
            entries[i] = new
        }
    }

    mutating func swapPanes(_ a: PaneKey, _ b: PaneKey) {
        entries = entries.map { $0.map { $0 == a ? b : $0 == b ? a : $0 } }
    }

    mutating func back(fromCurrent: Bool = true, isOpen: ([PaneKey]) -> Bool) -> [PaneKey]? {
        let currentOpen = prune(isOpen)
        return move(to: currentOpen && fromCurrent ? cursor - 1 : cursor)
    }

    mutating func forward(isOpen: ([PaneKey]) -> Bool) -> [PaneKey]? {
        prune(isOpen)
        return move(to: cursor + 1)
    }

    private mutating func move(to index: Int) -> [PaneKey]? {
        guard entries.indices.contains(index) else { return nil }
        cursor = index
        return entries[index]
    }

    @discardableResult
    private mutating func prune(_ isOpen: ([PaneKey]) -> Bool) -> Bool {
        var kept: [[PaneKey]] = []
        var keptCursor = -1
        var currentOpen = false
        for (i, entry) in entries.enumerated() {
            let open = isOpen(entry)
            if open {
                if let last = kept.last, Self.sameTab(last, entry) {
                    kept[kept.count - 1] = entry
                } else {
                    kept.append(entry)
                }
            }
            if i == cursor {
                keptCursor = kept.count - 1
                currentOpen = open
            }
        }
        entries = kept
        cursor = keptCursor
        return currentOpen
    }
}

extension SessionManager {
    enum TabTarget {
        case shown(TerminalWindowController)
        case hosted(TabItem)
        case hidden(HiddenTab)
    }

    func isHistoryTab(_ tab: TabItem) -> Bool {
        guard let c = tab.windowController else { return false }
        return !c.isClosed && !tab.isClosed && !c.isQuick && tab.wall == nil
    }

    func isHistoryTab(_ c: TerminalWindowController) -> Bool {
        isHistoryTab(c.selectedTab)
    }

    func recordTabVisit(_ tab: TabItem) {
        guard isHistoryTab(tab) else { return }
        tabHistory.visit(tab.content.paneIds)
    }

    func recordTabVisit(_ c: TerminalWindowController) {
        recordTabVisit(c.selectedTab)
    }

    func tabTarget(_ panes: [PaneKey]) -> TabTarget? {
        for pane in panes {
            if let t = tab(for: pane), isHistoryTab(t), let c = t.windowController {
                return c.tabs.count > 1 ? .hosted(t) : .shown(c)
            }
        }
        for ws in workspaces {
            if let i = ws.hiddenTabs.firstIndex(where: { $0.wall == nil && TabHistory.sameTab($0.panes, panes) }) {
                return .hidden(HiddenTab(workspace: ws.id, index: i))
            }
        }
        return nil
    }

    private var isOnHistoryTab: Bool {
        currentController.map(isHistoryTab) ?? false
    }

    private func isOpen(_ panes: [PaneKey]) -> Bool {
        tabTarget(panes) != nil
    }

    var canGoBackInTabHistory: Bool {
        var h = tabHistory
        return h.back(fromCurrent: isOnHistoryTab, isOpen: isOpen) != nil
    }

    var canGoForwardInTabHistory: Bool {
        var h = tabHistory
        return h.forward(isOpen: isOpen) != nil
    }

    func goBackInTabHistory() {
        show(tabHistory.back(fromCurrent: isOnHistoryTab, isOpen: isOpen))
    }

    func goForwardInTabHistory() {
        show(tabHistory.forward(isOpen: isOpen))
    }

    private func show(_ panes: [PaneKey]?) {
        guard let target = panes.flatMap(tabTarget) else {
            NSSound.beep()
            return
        }
        switch target {
        case .shown(let c):
            c.window?.makeKeyAndOrderFront(nil)
        case .hosted(let t):
            if let c = t.windowController {
                c.selectTab(t, focus: true)
                c.window?.makeKeyAndOrderFront(nil)
            }
        case .hidden(let tab):
            showHiddenTab(tab, from: currentRegularController)
        }
    }
}
