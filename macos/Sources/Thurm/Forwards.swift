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
}

/// The forward requests of plugins the app is carrying out, by the daemon's id for each. One
/// the daemon gave up on (`ui.cancel`: its caller went away or waited too long), or one of a
/// connection that is gone, must change nothing once it finishes.
struct ForwardRequests {
    enum Finish: Equatable {
        /// Still asked for: answer it, and open the browser if it said so.
        case answer
        /// Nobody waits for it: show and open nothing, and close the forward it opened, if any.
        case abandoned(close: UInt64?)
    }

    private var live: Set<UInt64> = []

    mutating func begin(_ id: UInt64) { live.insert(id) }

    mutating func cancel(_ id: UInt64) { live.remove(id) }

    /// The connection to the daemon changed: none of its requests is waited for any more.
    mutating func cancelAll() { live.removeAll() }

    /// Request `id` finished with `result` (`forward_open`'s or `forward_close`'s).
    mutating func finish(_ id: UInt64, result: [String: Any]?) -> Finish {
        if live.remove(id) != nil { return .answer }
        // Only a forward opened for it goes again: one it found open was someone else's.
        guard let r = result, jsonString(r["error"]) == nil, jsonBool(r["new"]) == true,
              let forward = jsonUInt64(r["id"]) else { return .abandoned(close: nil) }
        return .abandoned(close: forward)
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

/// How a forward's failure reaches the user: a toast on the focused pane, or an alert when no
/// pane is there to show one (Forward Port… with no terminal window open).
enum ForwardFailure: Equatable {
    case toast(String)
    case alert(String)

    static func report(host: HostId, _ why: String, hasView: Bool) -> ForwardFailure {
        let text = ForwardText.failure(host: host, why)
        return hasView ? .toast(text) : .alert(text)
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
        case copyLink(String)
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
                Entry(title: "Copy Forward Link \(name)", detail: f.url, action: .copyLink(f.url)),
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
        Extensions.shared.reportForwards(list, change: change)
        if jsonString(change?["change"]) == "closed", jsonString(change?["why"]) == "gone",
           let f = ForwardInfo(json: change?["forward"]) {
            tlog("forward \(f.host):\(f.remotePort) closed: the port went away")
        }
        RemoteHosts.shared.reload()
    }
}

extension SessionManager {
    /// Opens `host`'s `port`, or `url` (a link to its localhost), through a forward and then in the
    /// browser. The first use shows where it went, with Copy Link; a failure says why.
    func openForwarded(host: HostId, port: Int? = nil, url: String? = nil) {
        var call: [String: Any] = ["op": "forward_open", "host": host]
        if let port { call["port"] = port }
        if let url { call["url"] = url }
        DispatchQueue.global(qos: .userInitiated).async {
            let v = Core.shared.remoteCall(call) as? [String: Any]
            DispatchQueue.main.async {
                let view = SessionManager.shared.currentController?.content.focusedView
                if let e = jsonString(v?["error"]) {
                    switch ForwardFailure.report(host: host, e, hasView: view != nil) {
                    case .toast(let text):
                        view?.showToast(text, duration: 6)
                    case .alert(let text):
                        let alert = NSAlert()
                        alert.messageText = text
                        alert.runModal()
                    }
                    return
                }
                guard let f = ForwardInfo(json: v), let target = URL(string: f.url) else { return }
                if jsonBool(v?["new"]) == true {
                    view?.showToast(ForwardText.toast(host: host, remotePort: f.remotePort, localPort: f.localPort),
                                    duration: 6, action: ("Copy Link", { SessionManager.copyForwardLink(f.url) }))
                }
                NSWorkspace.shared.open(target)
            }
        }
    }

    /// The host Forward Port… is for: the focused pane's, else the front window's.
    var forwardHost: HostId? {
        let host = currentController.map { $0.focusedPane.host } ?? currentHost
        if host != localHost { return host }
        return Core.shared.connectedRemotes.first
    }

    /// Forward Port…: the focused host's known ports, or a port or link typed in.
    func showForwardPortPicker() {
        guard let host = forwardHost else {
            currentController?.content.focusedView?.showToast("No remote host is connected")
            return
        }
        let known = (Core.shared.remoteCall(["op": "known_ports", "host": host]) as? [[String: Any]] ?? [])
            .compactMap { k -> (port: Int, command: String)? in
                guard let p = jsonInt(k["port"]) else { return nil }
                return (p, jsonString(k["command"]) ?? "")
            }
        let open = Remotes.shared.forwards
        let items = { (query: String) -> [CommandPalette.Item] in
            ForwardPicker.choices(query: query, host: host, known: known, open: open).map { c in
                CommandPalette.Item(title: c.title, detail: c.detail) {
                    switch c.target {
                    case .port(let p): SessionManager.shared.openForwarded(host: host, port: p)
                    case .url(let u): SessionManager.shared.openForwarded(host: host, url: u)
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

    static func copyForwardLink(_ url: String) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(url, forType: .string)
    }

    /// `host` connected: every pane's port list at once, so its forwards don't go by lists of
    /// panes that ended while it was away (no event says so). An older daemon answers an error.
    func fetchRemotePorts(_ host: HostId) {
        Core.shared.requestAsync(object: ["Ports": ["pane": NSNull()]], host: host, timeout: 10) { resp in
            guard let v = JSON.variant(resp), v.name == "Ports", let panes = v.payload as? [Any] else { return }
            _ = Core.shared.remoteCall(["op": "ports_snapshot", "host": host, "panes": panes])
        }
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
        Extensions.shared.reportPorts(host: host, pane: pane, ports: payload["ports"] ?? [],
                                      complete: payload["complete"] ?? true)
    }
}
