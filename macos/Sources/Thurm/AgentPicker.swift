import AppKit

/// A pane running a coding agent, wherever it is: a window (`controller`) or a workspace no
/// window shows (`hidden`).
struct AgentPane {
    let pane: PaneKey
    let info: PaneInfo
    let agent: AgentState
    let controller: TerminalWindowController?
    let hidden: Workspace?

    /// What the agent works on: its session topic, the topic in the title it sets, or its name.
    var title: String {
        agent.topic ?? Self.programTopic(info.title, agent: agent.name) ?? agent.name
    }

    /// The remote host it runs on (nil on this Mac).
    var hostLabel: String? { pane.isRemote ? pane.host : nil }

    /// Its repository or directory.
    var place: String? {
        info.git.map { ($0.root as NSString).lastPathComponent }
            ?? info.cwd.map { pane.isRemote ? $0 : abbreviatePath($0) }
    }

    /// Its status in words ("Needs your permission to use Bash", "Done in 2m").
    var statusDetail: String {
        switch agent.status {
        case .needsInput: return agent.message.map { Self.shorten($0, agent: agent.name) } ?? "Needs input"
        case .working: return "Working"
        case .done:
            let done = agent.turnMs.map { "Done in \(formatDuration($0))" } ?? "Done"
            // The turn's summary (`ai.notifications`).
            return agent.message.map { "\(done) · \($0)" } ?? done
        case .error: return agent.message ?? "Failed"
        case .idle: return "Idle"
        }
    }

    /// The topic in the title the agent sets itself ("✳ Fix login bug", "⠐ Fix login bug"),
    /// without its status glyph; nil when it only names the agent.
    private static func programTopic(_ title: String, agent: String) -> String? {
        let t = String(title.drop { !$0.isLetter && !$0.isNumber })
            .trimmingCharacters(in: .whitespaces)
        return t.isEmpty || t == agent ? nil : t
    }

    /// "Claude needs your permission to use Bash" → "Needs your permission to use Bash",
    /// "Claude is waiting for your input" → "Waiting for your input".
    private static func shorten(_ message: String, agent: String) -> String {
        guard let first = agent.split(separator: " ").first, message.hasPrefix(first + " ") else { return message }
        var rest = message.dropFirst(first.count + 1)
        if rest.hasPrefix("is ") { rest = rest.dropFirst(3) }
        return rest.prefix(1).uppercased() + rest.dropFirst()
    }
}

extension SessionManager {
    /// Every agent pane: the tabs of `first`'s window first, then other windows, then hidden
    /// workspaces.
    func agentPanes(first: TerminalWindowController?) -> [AgentPane] {
        let mine = first.map { group(of: $0) } ?? []
        let others = liveControllers.filter { c in !mine.contains { $0 === c } }
        var out: [AgentPane] = []
        func add(_ id: PaneKey, in c: TerminalWindowController?, hidden: Workspace?) {
            guard let info = panes[id], let agent = info.agent else { return }
            out.append(AgentPane(pane: id, info: info, agent: agent, controller: c, hidden: hidden))
        }
        for c in mine + others {
            for id in c.content.paneIds { add(id, in: c, hidden: nil) }
        }
        for ws in workspaces where !isShown(ws) {
            for tab in ws.hiddenTabs {
                for id in tab.root.panes { add(id, in: nil, hidden: ws) }
            }
        }
        return out
    }

    /// Brings an agent's pane forward: its window and tab, its split, its workspace (shown in
    /// `host`'s window when no window shows it).
    func focusAgent(_ a: AgentPane, from host: TerminalWindowController?) {
        if let ws = a.hidden {
            if let c = host ?? currentController {
                switchWorkspace(in: c, to: ws)
            } else {
                openWorkspaceInNewWindow(ws)
            }
        }
        focusPane(a.pane, activate: true)
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
            let here = host.map { $0 === a.controller && $0.focusedPane == a.pane } ?? false
            let detail = [a.hostLabel, a.agent.name, a.place, a.hidden?.name, here ? "this pane" : a.statusDetail]
                .compactMap { $0 }
                .joined(separator: " · ")
            return CommandPalette.Item(title: a.title, detail: detail) {
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
