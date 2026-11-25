import AppKit

struct NavigationTarget {
    enum Destination: Equatable {
        case workspace(UInt64)
        case tab([PaneKey])
        case pane(PaneKey)
    }

    let title: String
    let detail: String
    let searchText: String
    let destination: Destination
    let tabWorkspaceID: UInt64?
    weak var tabController: TerminalWindowController?
    weak var tabItem: TabItem?

    init(title: String, detail: String, searchText: String = "", destination: Destination,
         tabController: TerminalWindowController? = nil, tabItem: TabItem? = nil,
         tabWorkspaceID: UInt64? = nil) {
        self.title = title
        self.detail = detail
        self.searchText = [title, detail, searchText].joined(separator: " ")
        self.destination = destination
        self.tabController = tabController
        self.tabItem = tabItem
        self.tabWorkspaceID = tabWorkspaceID
    }
}

extension SessionManager {
    func navigationTargets() -> [NavigationTarget] {
        var targets: [NavigationTarget] = []
        func joined(_ values: [String?]) -> String {
            values.compactMap { $0 }.filter { !$0.isEmpty }.joined(separator: " · ")
        }
        func paneTitle(_ id: PaneKey) -> String {
            guard let info = panes[id] else { return "Terminal" }
            if let agent = info.agent {
                return AgentPane(pane: id, info: info, agent: agent, controller: nil, hidden: nil).title
            }
            return info.title.isEmpty ? "Terminal" : info.title
        }
        func metadata(_ id: PaneKey) -> String {
            guard let info = panes[id] else { return id.host }
            return joined([id.host, info.title, info.cwd, info.git?.root, info.git?.branch,
                           info.agent?.name, info.agent?.topic])
        }
        func addTab(title: String?, panes ids: [PaneKey], focused: PaneKey?, workspace: Workspace?,
                    controller: TerminalWindowController? = nil, tabItem: TabItem? = nil) {
            guard let first = ids.first else { return }
            let selected = focused.flatMap { ids.contains($0) ? $0 : nil } ?? first
            let tabTitle = title.flatMap { $0.isEmpty ? nil : $0 } ?? paneTitle(selected)
            let hosts = Array(Set(ids.map(\.host))).sorted().joined(separator: ", ")
            let location = joined([workspace?.name, hosts])
            let repositories = Array(Set(ids.compactMap { panes[$0]?.git?.root }
                .map { ($0 as NSString).lastPathComponent })).sorted().joined(separator: ", ")
            targets.append(NavigationTarget(title: tabTitle, detail: joined(["Tab", location, repositories]),
                                            searchText: ids.map(metadata).joined(separator: " "),
                                            destination: .tab(ids), tabController: controller, tabItem: tabItem,
                                            tabWorkspaceID: workspace?.id))
            for id in ids {
                guard let info = panes[id] else { continue }
                let agent = info.agent.map { AgentPane(pane: id, info: info, agent: $0, controller: nil, hidden: nil) }
                let place = info.git.map { ($0.root as NSString).lastPathComponent } ?? info.cwd
                let detail = joined([agent == nil ? "Pane" : "Agent", workspace?.name, id.host,
                                     tabTitle, place, info.git?.branch, info.agent?.name, agent?.statusDetail])
                targets.append(NavigationTarget(title: paneTitle(id), detail: detail, searchText: metadata(id),
                                                destination: .pane(id)))
            }
        }
        for ws in workspacesByRecency {
            targets.append(NavigationTarget(title: ws.name, detail: "Workspace · " + ws.host,
                                            destination: .workspace(ws.id)))
            if let shown = controllerShowing(ws) {
                let ordered = windowTabs(of: shown)
                let others = liveTabs.filter { t in
                    t.workspaceID == ws.id && !ordered.contains { $0 === t }
                }
                for t in ordered + others {
                    let c = t.windowController
                    let title = t.titleOverride ?? (t.isSelected ? c?.window?.title : t.displayTitle(panes: panes))
                    addTab(title: title, panes: t.content.paneIds, focused: t.focusedPane,
                           workspace: ws, controller: c, tabItem: t)
                }
            } else {
                for tab in ws.hiddenTabs {
                    addTab(title: tab.title, panes: tab.panes, focused: tab.focusedKey, workspace: ws)
                }
            }
        }
        for t in liveTabs where workspace(t.workspaceID) == nil {
            let c = t.windowController
            let title = t.titleOverride ?? (t.isSelected ? c?.window?.title : t.displayTitle(panes: panes))
            addTab(title: title, panes: t.content.paneIds, focused: t.focusedPane,
                   workspace: nil, controller: c, tabItem: t)
        }
        return targets
    }

    func activateNavigationTarget(_ target: NavigationTarget, from host: TerminalWindowController?) {
        func show(_ ws: Workspace) {
            if let shown = controllerShowing(ws) {
                (shown.window?.tabGroup?.selectedWindow ?? shown.window)?.makeKeyAndOrderFront(nil)
            } else if let host, !host.isClosed {
                switchWorkspace(in: host, to: ws)
            } else {
                openWorkspaceInNewWindow(ws)
            }
        }

        // Find the current location. Panes can move or close while the picker is open.
        switch target.destination {
        case .workspace(let id):
            guard let ws = workspace(id) else { return }
            show(ws)
            NSApp.activate()
        case .pane(let pane):
            guard panes[pane] != nil else { return }
            if let ws = workspace(containing: pane) { show(ws) }
            focusPane(pane, activate: true)
        case .tab(let ids):
            func showHiddenTab(in candidates: [Workspace]) -> Bool {
                guard let ws = candidates.first(where: { ws in
                    !isShown(ws) && ws.hiddenTabs.contains { tab in tab.panes.contains { ids.contains($0) } }
                }), let tab = ws.hiddenTabs.first(where: { tab in tab.panes.contains { ids.contains($0) } })
                else { return false }
                let focused = tab.focusedKey
                show(ws)
                focusPane(focused, activate: true)
                return true
            }
            if let t = target.tabItem, !t.isClosed, t.windowController?.isClosed == false,
               ids.contains(where: { t.content.contains($0) }) {
                focusPane(t.focusedPane, activate: true)
                return
            }
            // If the original tab was parked into a background workspace, check its surviving
            // panes in the source workspace before scanning other tabs on the old window.
            let sourceWS = target.tabWorkspaceID.flatMap { workspace($0) }
            if target.tabItem?.windowController == nil, let ws = sourceWS, showHiddenTab(in: [ws]) {
                return
            }
            if let c = target.tabController, !c.isClosed,
               target.tabWorkspaceID == nil || c.workspaceID == target.tabWorkspaceID,
               let t = c.tabs.first(where: { !$0.isClosed && ids.contains(where: $0.content.contains) }) {
                focusPane(t.focusedPane, activate: true)
                return
            }
            // Match surviving panes in the source workspace before a pane moved elsewhere.
            // The hidden tab's index can change when other tabs move or close.
            if let ws = sourceWS, showHiddenTab(in: [ws]) {
                return
            }
            if let t = ids.lazy.compactMap({ self.tab(for: $0) }).first {
                focusPane(t.focusedPane, activate: true)
                return
            }
            _ = showHiddenTab(in: workspaces)
        }
    }

    func showNavigationPicker() {
        let host = currentController
        let items = navigationTargets().map { target in
            CommandPalette.Item(title: target.title, detail: target.detail, searchText: target.searchText) {
                self.activateNavigationTarget(target, from: host)
            }
        }
        CommandPalette.shared.show(items: items, over: host?.window,
                                   placeholder: "Find a workspace, tab, pane, or agent…",
                                   footer: "↑↓ select · ↩ switch · esc cancel")
    }
}
