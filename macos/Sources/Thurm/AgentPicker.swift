import AppKit

/// A pane running a coding agent, wherever it is: a window (`controller` / `tab`) or a workspace no
/// window shows (`hidden`).
struct AgentPane {
    let pane: PaneKey
    let info: PaneInfo
    let agent: AgentState
    let controller: TerminalWindowController?
    let hidden: Workspace?
    var tab: TabItem? = nil

    /// What the agent works on: its session topic, the topic in the title it sets, or its name.
    var title: String { agent.title(paneTitle: info.title) }

    /// The age of the daemon's last reported change. Missing timestamps show no age.
    func activityAge(now: Date = Date()) -> String? { agent.activityAge(now: now) }

    /// The remote host it runs on (nil on this Mac).
    var hostLabel: String? { pane.isRemote ? pane.host : nil }

    /// Its repository or directory.
    var place: String? {
        info.git.map { ($0.root as NSString).lastPathComponent }
            ?? info.cwd.map { pane.isRemote ? $0 : abbreviatePath($0) }
    }

    /// Its status in words ("Needs your permission to use Bash", "Done in 2m").
    var statusDetail: String { agent.statusDetail }
}

extension SessionManager {
    /// Every agent pane: the tabs of `first`'s window first, then other windows, then hidden
    /// workspaces.
    func agentPanes(first: TerminalWindowController?) -> [AgentPane] {
        let mine = first.map { windowTabs(of: $0) } ?? []
        let others = liveTabs.filter { t in !mine.contains { $0 === t } }
        var out: [AgentPane] = []
        func add(_ id: PaneKey, in tab: TabItem?, hidden: Workspace?) {
            guard let info = panes[id], let agent = info.agent else { return }
            out.append(AgentPane(pane: id, info: info, agent: agent,
                                 controller: tab?.windowController, hidden: hidden, tab: tab))
        }
        for t in mine + others {
            for id in t.content.paneIds { add(id, in: t, hidden: nil) }
        }
        for ws in workspaces where !isShown(ws) {
            for tab in ws.hiddenTabs {
                for id in tab.panes { add(id, in: nil, hidden: ws) }
            }
        }
        return out
    }

    /// Brings an agent's pane forward: its window and tab, its split, its workspace (shown in
    /// `host`'s window when no window shows it).
    func focusAgent(_ a: AgentPane, from host: TerminalWindowController?) {
        focusPane(a.pane, activate: true, from: host)
    }

    /// ⌘⇧A: pick a coding agent, the ones waiting for you first; or start a new one.
    func showAgentPicker() {
        let host = currentController
        let agents = agentPanes(first: host).enumerated()
            .sorted { a, b in
                a.element.agent.status.urgency != b.element.agent.status.urgency
                    ? a.element.agent.status.urgency > b.element.agent.status.urgency
                    : a.offset < b.offset
            }
            .map(\.element)
        var items: [CommandPalette.Item] = agents.map { a in
            let here = host.map {
                (a.tab != nil ? $0.selectedTab === a.tab : $0 === a.controller) && $0.focusedPane == a.pane
            } ?? false
            let detail = [a.hostLabel, a.agent.name, a.place, a.hidden?.name, here ? "this pane" : a.statusDetail]
                .compactMap { $0 }
                .joined(separator: " · ")
            return CommandPalette.Item(title: a.title, detail: detail, liveShortcut: { now in
                guard let info = SessionManager.shared.panes[a.pane], let agent = info.agent else { return "" }
                return AgentPane(pane: a.pane, info: info, agent: agent, controller: a.controller,
                                 hidden: a.hidden, tab: a.tab)
                    .activityAge(now: now) ?? ""
            }) {
                SessionManager.shared.focusAgent(a, from: host)
            }
        }
        let daemon = currentHost
        for preset in presets(for: daemon) {
            let name = preset.name
            let on = daemon == localHost ? "" : " on \(daemon)"
            items.append(CommandPalette.Item(title: "Launch \(name)\(on)", detail: preset.command.joined(separator: " ")) {
                SessionManager.shared.newTab(from: host, preset: name)
            })
        }
        CommandPalette.shared.show(items: items, over: host?.window,
                                   placeholder: agents.isEmpty ? "No agents running. Start one…" : "Switch to agent…",
                                   footer: "↩ switch")
    }
}
