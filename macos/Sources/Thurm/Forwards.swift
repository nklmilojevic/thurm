import AppKit

// Forwards: a remote host's TCP port opened on this Mac's loopback (Rust:
// crates/thurm-remote/src/forward.rs). A `localhost` link in a remote pane, a remote row of
// Processes & Ports, or Forward Port… opens one; nothing is forwarded without such an action.

/// An open forward, as `forward_list` and the status callback report it.
struct ForwardInfo: Equatable {
    var id: UInt64
    var host: HostId
    var remotePort: Int
    var localPort: Int
    /// Where it opens: `http://localhost:<local>/`, or the link it was opened for.
    var url: String

    init?(json: Any?) {
        guard let d = json as? [String: Any], let id = jsonUInt64(d["id"]), let host = jsonString(d["host"]),
              let remote = jsonInt(d["remote_port"]), let local = jsonInt(d["local_port"]) else { return nil }
        self.id = id
        self.host = host
        remotePort = remote
        localPort = local
        url = jsonString(d["url"]) ?? "http://localhost:\(local)/"
    }

    /// `known`'s link, if `answer` (to `forward_get`) says that forward is still open on the
    /// same local port: a forward that closed since may have left its port to another program.
    static func liveLink(of known: ForwardInfo, answer: Any?) -> String? {
        guard let now = ForwardInfo(json: answer), now.id == known.id, now.localPort == known.localPort else {
            return nil
        }
        return now.url
    }

    /// The link a request opened `requested` for (`forward_open`'s answer), if the forward is still
    /// open on the same port. Not the link it holds now: another request on the same port may have
    /// given it its own since, and this one must open what it asked for.
    static func requestedLink(_ requested: ForwardInfo, answer: Any?) -> String? {
        liveLink(of: requested, answer: answer) == nil ? nil : requested.url
    }
}

/// What forwards say to the user.
enum ForwardText {
    /// The toast on a forward's first use: `devbox:5173 → localhost:5173`.
    static func toast(host: HostId, remotePort: Int, localPort: Int) -> String {
        "\(host):\(remotePort) → localhost:\(localPort)"
    }

    static func failure(host: HostId, _ why: String) -> String {
        "Could not forward from \(host): \(why)"
    }
}

/// What the user hears of a forward: on the pane it was started from, as a toast; or, when that
/// pane is gone (closed meanwhile, or none: Forward Port… with no terminal window), as an alert,
/// never on whichever pane has focus now. `copyLink`: the link to offer to copy.
enum ForwardNotice: Equatable {
    case toast(String, copyLink: String?)
    case alert(String, copyLink: String?)

    static func failure(host: HostId, _ why: String, onPane: Bool) -> ForwardNotice {
        let text = ForwardText.failure(host: host, why)
        return onPane ? .toast(text, copyLink: nil) : .alert(text, copyLink: nil)
    }

    /// A forward opened for the first time: where it went, and its link.
    static func opened(host: HostId, remotePort: Int, localPort: Int, url: String, onPane: Bool) -> ForwardNotice {
        let text = ForwardText.toast(host: host, remotePort: remotePort, localPort: localPort)
        return onPane ? .toast(text, copyLink: url) : .alert(text, copyLink: url)
    }
}

/// Which host Forward Port… is for: the focused pane's when it is remote; else the one
/// connected host; else the user picks among them.
enum ForwardHostChoice: Equatable {
    case host(HostId)
    case pick([HostId])
    case none

    static func of(focused: HostId, connected: [HostId]) -> ForwardHostChoice {
        if focused != localHost { return .host(focused) }
        switch connected.count {
        case 0: return .none
        case 1: return .host(connected[0])
        default: return .pick(connected)
        }
    }
}

/// What Forward Port… offers for what was typed: the typed port or link first, then the ports
/// the host's panes listen on.
enum ForwardPicker {
    enum Target: Equatable {
        case port(Int)
        case url(String)
    }

    struct Choice: Equatable {
        let title: String
        let detail: String
        let target: Target
    }

    /// A port number, or an `http(s)` link (to the host's localhost: the Rust side checks).
    static func parse(_ query: String) -> Target? {
        let q = query.trimmingCharacters(in: .whitespaces)
        if let port = Int(q), (1...65535).contains(port) { return .port(port) }
        let lower = q.lowercased()
        if lower.hasPrefix("http://") || lower.hasPrefix("https://") { return .url(q) }
        return nil
    }

    enum Manage: Equatable {
        case copyLink(ForwardInfo)
        case close(UInt64)
    }

    struct Entry: Equatable {
        let title: String
        let detail: String
        let action: Manage
    }

    /// The command palette's entries for open forwards: Copy Link (the toast's lasts only
    /// seconds, mostly spent in the browser) and Close, for each.
    static func manageEntries(_ forwards: [ForwardInfo]) -> [Entry] {
        forwards.flatMap { f -> [Entry] in
            let name = "\(f.host):\(f.remotePort)"
            return [
                Entry(title: "Copy Forward Link \(name)", detail: f.url, action: .copyLink(f)),
                Entry(title: "Close Forward \(name)", detail: "localhost:\(f.localPort)", action: .close(f.id)),
            ]
        }
    }

    /// `known`: (port, command) of the host's panes, by port.
    static func choices(query: String, host: HostId, known: [(port: Int, command: String)],
                        open: [ForwardInfo]) -> [Choice] {
        var out: [Choice] = []
        let typed = parse(query)
        switch typed {
        case .port(let p):
            out.append(Choice(title: "Forward \(host):\(p)", detail: "open it on this Mac", target: .port(p)))
        case .url(let u):
            out.append(Choice(title: "Open \(u)", detail: "through a forward from \(host)", target: .url(u)))
        case nil:
            break
        }
        let q = query.trimmingCharacters(in: .whitespaces).lowercased()
        for k in known where typed != .port(k.port) {
            if !q.isEmpty, typed == nil, !"\(k.port) \(k.command)".lowercased().contains(q) { continue }
            let forwarded = open.first { $0.host == host && $0.remotePort == k.port }
            let detail = forwarded.map { "\(k.command) · forwarded to localhost:\($0.localPort)" } ?? k.command
            out.append(Choice(title: "\(host):\(k.port)", detail: detail, target: .port(k.port)))
        }
        return out
    }
}

extension Remotes {
    /// The status callback's report of forwards: every open one, and what changed.
    func forwardsChanged(_ list: [Any], change: [String: Any]?) {
        forwards = list.compactMap { ForwardInfo(json: $0) }
        if jsonString(change?["change"]) == "closed", jsonString(change?["why"]) == "gone",
           let f = ForwardInfo(json: change?["forward"]) {
            tlog("forward \(f.host):\(f.remotePort) closed: the port went away")
        }
        RemoteHosts.shared.reload()
    }
}

extension SessionManager {
    /// Opens `host`'s `port`, or `url` (a link to its localhost), through a forward and then in the
    /// browser. The first use shows where it went, with Copy Link; a failure says why. Both on
    /// `source`, the pane it was started from, while that pane is still there (`ForwardNotice`).
    func openForwarded(host: HostId, port: Int? = nil, url: String? = nil, from source: TerminalView?) {
        var call: [String: Any] = ["op": "forward_open", "host": host]
        if let port { call["port"] = port }
        if let url { call["url"] = url }
        weak var source = source
        DispatchQueue.global(qos: .userInitiated).async {
            let v = Core.shared.remoteCall(call) as? [String: Any]
            DispatchQueue.main.async {
                let pane = source.flatMap { $0.window == nil ? nil : $0 }
                if let e = jsonString(v?["error"]) {
                    SessionManager.show(ForwardNotice.failure(host: host, e, onPane: pane != nil), on: pane)
                    return
                }
                guard let f = ForwardInfo(json: v) else { return }
                // Closed in the meantime (a move, the sweep): its port is no longer ours to open. Open,
                // and show, the link this request asked for.
                guard let link = SessionManager.requestedLink(f), let target = URL(string: link) else {
                    SessionManager.show(ForwardNotice.failure(host: host, "the forward closed before it opened",
                                                              onPane: pane != nil), on: pane)
                    return
                }
                // The link first: the notice may be an alert, which waits for the user.
                NSWorkspace.shared.open(target)
                if jsonBool(v?["new"]) == true {
                    SessionManager.show(ForwardNotice.opened(host: host, remotePort: f.remotePort, localPort: f.localPort,
                                                             url: link, onPane: pane != nil), on: pane, forward: f)
                }
            }
        }
    }

    /// `forward`: the one Copy Link copies (the link the notice shows, while that forward is
    /// still open), checked again when it is clicked.
    private static func show(_ notice: ForwardNotice, on pane: TerminalView?, forward: ForwardInfo? = nil) {
        switch notice {
        case .toast(let text, let link):
            let copy = forward.flatMap { f in link.map { url in ("Copy Link", { copyForwardLink(f, url: url) }) } }
            pane?.showToast(text, duration: 6, action: copy)
        case .alert(let text, let link):
            let alert = NSAlert()
            alert.messageText = text
            alert.addButton(withTitle: "OK")
            if let link, forward != nil {
                alert.informativeText = link
                alert.addButton(withTitle: "Copy Link")
            }
            if alert.runModal() == .alertSecondButtonReturn, let forward { copyForwardLink(forward, url: link) }
        }
    }

    /// The host Forward Port… is for (`ForwardHostChoice`).
    var forwardHost: ForwardHostChoice {
        let focused = currentController.map { $0.focusedPane.host } ?? currentHost
        return ForwardHostChoice.of(focused: focused, connected: Core.shared.connectedRemotes)
    }

    /// Forward Port…: the host's known ports, or a port or link typed in; first which host, when
    /// a pane of this Mac has focus and several hosts are connected.
    func showForwardPortPicker() {
        let source = currentController?.content.focusedView
        switch forwardHost {
        case .host(let host):
            showForwardPortPicker(host: host, source: source)
        case .pick(let hosts):
            weak var source = source
            let items = hosts.map { host in
                CommandPalette.Item(title: host, detail: "forward one of its ports") {
                    // After this palette has closed.
                    DispatchQueue.main.async {
                        SessionManager.shared.showForwardPortPicker(host: host, source: source)
                    }
                }
            }
            CommandPalette.shared.show(items: items, over: currentController?.window,
                                       placeholder: "Forward a port of which host?", footer: "↩ choose")
        case .none:
            currentController?.content.focusedView?.showToast("No remote host is connected")
        }
    }

    private func showForwardPortPicker(host: HostId, source: TerminalView?) {
        let known = (Core.shared.remoteCall(["op": "known_ports", "host": host]) as? [[String: Any]] ?? [])
            .compactMap { k -> (port: Int, command: String)? in
                guard let p = jsonInt(k["port"]) else { return nil }
                return (p, jsonString(k["command"]) ?? "")
            }
        let open = Remotes.shared.forwards
        weak var source = source
        let items = { (query: String) -> [CommandPalette.Item] in
            ForwardPicker.choices(query: query, host: host, known: known, open: open).map { c in
                CommandPalette.Item(title: c.title, detail: c.detail) {
                    switch c.target {
                    case .port(let p): SessionManager.shared.openForwarded(host: host, port: p, from: source)
                    case .url(let u): SessionManager.shared.openForwarded(host: host, url: u, from: source)
                    }
                }
            }
        }
        let hint = known.isEmpty ? "No ports listed on \(host) yet: type one" : "↩ forward and open"
        CommandPalette.shared.show(items: items(""), over: currentController?.window,
                                   placeholder: "Forward a port of \(host): number or link…", footer: hint,
                                   onQuery: { query in
                                       CommandPalette.shared.updateSearch(items: items(query), hint: hint)
                                   })
    }

    /// `f`'s link as it is now, or nil once it has closed (`forward_get`).
    static func liveLink(of f: ForwardInfo) -> String? {
        ForwardInfo.liveLink(of: f, answer: Core.shared.remoteCall(["op": "forward_get", "id": NSNumber(value: f.id)]))
    }

    /// `f`'s request link (`ForwardInfo.requestedLink`), or nil once it has closed.
    static func requestedLink(_ f: ForwardInfo) -> String? {
        ForwardInfo.requestedLink(f, answer: Core.shared.remoteCall(["op": "forward_get", "id": NSNumber(value: f.id)]))
    }

    /// Copies `f`'s link if it is still open; a list or a toast may show one that closed since.
    /// `url`: a notice's own link, copied as shown; else the forward's link as it is now (the
    /// lists' Copy Link).
    static func copyForwardLink(_ f: ForwardInfo, url shown: String? = nil) {
        guard let current = liveLink(of: f) else {
            NSSound.beep()
            return
        }
        let url = shown ?? current
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(url, forType: .string)
    }

    /// Closes forward `id` off the main thread: closing waits until its listeners are closed,
    /// which must not hold up the app (the change comes back through the status callback).
    static func closeForward(_ id: UInt64) {
        DispatchQueue.global(qos: .userInitiated).async {
            _ = Core.shared.remoteCall(["op": "forward_close", "id": NSNumber(value: id)])
        }
    }

    /// Opens `f`'s link if it is still open.
    static func openForwardLink(_ f: ForwardInfo) {
        guard let link = liveLink(of: f), let url = URL(string: link) else {
            NSSound.beep()
            return
        }
        NSWorkspace.shared.open(url)
    }

    /// `host`'s connection dropped: what listens there is unknown until it is back.
    func remotePortsUnknown(_ host: HostId) {
        _ = Core.shared.remoteCall(["op": "host_disconnected", "host": host])
    }

    /// A `PortsChanged` from a remote host: its forwards learn whether their port is still there.
    func remotePortsChanged(_ payload: [String: Any]?, host: HostId) {
        guard host != localHost, let payload, let pane = jsonUInt64(payload["pane"]) else { return }
        _ = Core.shared.remoteCall(["op": "ports_changed", "host": host, "pane": NSNumber(value: pane),
                                    "ports": payload["ports"] ?? [], "complete": payload["complete"] ?? true])
    }
}
