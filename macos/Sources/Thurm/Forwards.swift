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

/// The forward requests of plugins the app is carrying out, and who still has which forward.
///
/// A request the daemon gave up on (`ui.cancel`: its caller went away or waited too long), or
/// one of a connection to the daemon that is gone, must change nothing once it finishes: a
/// forward it opened is closed again, unless someone else has it. Forwards are shared (an open
/// of a host's port that has one gets that one, `new` false), so whoever got it since, a plugin
/// that waited or the user, keeps it; and while another open of that host is under way, the
/// close waits for it to say what it got.
struct ForwardRequests {
    /// A request, by the daemon connection it came on and the daemon's id for it: a daemon
    /// started again numbers its requests from 1 again.
    struct Key: Hashable {
        let connection: Int
        let id: UInt64
    }

    enum Finish: Equatable {
        /// Still asked for: answer it, and open the browser if it said so.
        case answer
        /// Nobody waits for it: show and open nothing.
        case abandoned
    }

    private var live: Set<Key> = []
    /// Opens under way (a plugin's or the user's), by host.
    private var opening: [HostId: Int] = [:]
    /// Forwards handed to someone who waited for them.
    private var held: Set<UInt64> = []
    /// Forwards opened for a request nobody waits for, by host, while another open of that host
    /// is under way.
    private var orphans: [UInt64: HostId] = [:]

    /// A plugin's request; `opening` is the host it opens a forward of (none for a close).
    mutating func begin(_ key: Key, opening host: HostId?) {
        live.insert(key)
        if let host { opening[host, default: 0] += 1 }
    }

    mutating func cancel(_ key: Key) { live.remove(key) }

    /// Whether request `key` is still waited for (an open checks before it starts).
    func isLive(_ key: Key) -> Bool { live.contains(key) }

    /// The connection to the daemon changed: none of its requests is waited for any more.
    mutating func cancelAll() { live.removeAll() }

    /// The user opens a forward of `host`.
    mutating func userOpens(_ host: HostId) { opening[host, default: 0] += 1 }

    /// The user's open of `host` finished with `result`; the forwards to close now.
    mutating func userOpened(_ host: HostId, result: [String: Any]?) -> [UInt64] {
        settle(host, waited: true, result: result)
    }

    /// Request `key` finished with `result` (`forward_open`'s, of `host`, or `forward_close`'s):
    /// whether to answer it, and the forwards to close now.
    mutating func finish(_ key: Key, opening host: HostId?, result: [String: Any]?) -> (Finish, close: [UInt64]) {
        let waited = live.remove(key) != nil
        guard let host else { return (waited ? .answer : .abandoned, []) }
        return (waited ? .answer : .abandoned, settle(host, waited: waited, result: result))
    }

    /// The forwards open now: the others are no one's any more.
    mutating func forwardsNow(_ ids: Set<UInt64>) {
        held.formIntersection(ids)
        orphans = orphans.filter { ids.contains($0.key) }
    }

    private mutating func settle(_ host: HostId, waited: Bool, result: [String: Any]?) -> [UInt64] {
        if let n = opening[host] { opening[host] = n > 1 ? n - 1 : nil }
        if let r = result, jsonString(r["error"]) == nil, let forward = jsonUInt64(r["id"]) {
            if waited {
                held.insert(forward)
                orphans[forward] = nil
            } else if jsonBool(r["new"]) == true, !held.contains(forward) {
                orphans[forward] = host
            }
        }
        guard opening[host] == nil else { return [] }
        let close = orphans.filter { $0.value == host }.map(\.key).sorted()
        for id in close { orphans[id] = nil }
        return close
    }
}

/// The remote hosts' port lists the app passes on to this Mac's daemon for its plugins
/// (`ports.report`): the latest of each pane, sent again whenever the app connects to the daemon,
/// and for each host just connected to, its own answer of what it lists (`initial`: running
/// already).
///
/// While that answer is on its way, the host's lists are held back: some are the lists its
/// daemon sends a client that joins (running already too), some are changes, and an answer that
/// comes after a change may be older than it. Once the answer is in, it goes first, as `initial`,
/// then what was held, in order, as changes, but for lists that only say again what is known.
struct RemotePorts {
    struct Entry {
        let host: HostId
        let pane: UInt64
        let ports: Any
        let complete: Any
    }

    /// A list to send, and whether it was running already.
    struct Send {
        let entry: Entry
        let initial: Bool
    }

    private var lists: [HostId: [UInt64: Entry]] = [:]
    /// Hosts whose own answer of what they list is awaited, by the ask's number.
    private var learning: [HostId: Int] = [:]
    /// What `learning` hosts listed meanwhile, in order.
    private var held: [HostId: [Entry]] = [:]
    private var asks = 0

    /// The app connected to `host`: its lists wait for its answer to ask number (returned)
    /// `learned`.
    mutating func connected(_ host: HostId) -> Int {
        asks += 1
        learning[host] = asks
        lists[host] = nil
        held[host] = nil
        return asks
    }

    /// The connection to `host` is gone: its lists, its ask and what waited for it with it.
    mutating func disconnected(_ host: HostId) {
        learning[host] = nil
        lists[host] = nil
        held[host] = nil
    }

    /// A pane of `host` listed `ports` (live, from its daemon): the list to send now, or none
    /// while the host's answer is awaited (it waits for it).
    mutating func live(_ host: HostId, pane: UInt64, ports: Any, complete: Any) -> Send? {
        let e = Entry(host: host, pane: pane, ports: ports, complete: complete)
        if learning[host] != nil {
            held[host, default: []].append(e)
            return nil
        }
        keep(e)
        return Send(entry: e, initial: false)
    }

    /// Ask `ask` of `host` was answered with `snapshot` (nil: an error, a timeout, a daemon from
    /// before ports): what to send, in order. Nothing for an ask that is not the one awaited (of
    /// a connection that went, or that a newer one replaced).
    mutating func learned(_ host: HostId, ask: Int, snapshot: [Entry]?) -> [Send] {
        guard learning[host] == ask else { return [] }
        learning[host] = nil
        let waited = held.removeValue(forKey: host) ?? []
        guard let snapshot else {
            // Nothing to tell changes by: what came meanwhile is what runs, already.
            return waited.map { keep($0); return Send(entry: $0, initial: true) }
        }
        var out: [Send] = []
        for e in snapshot where !RemotePorts.isEmpty(e.ports) {
            keep(e)
            out.append(Send(entry: e, initial: true))
        }
        for e in waited {
            let known = lists[host]?[e.pane]
            if RemotePorts.same(known, e) { continue }
            keep(e)
            out.append(Send(entry: e, initial: false))
        }
        return out
    }

    /// Every list known now, by host and pane.
    var all: [Entry] {
        lists.keys.sorted().flatMap { host in
            (lists[host] ?? [:]).keys.sorted().compactMap { lists[host]?[$0] }
        }
    }

    /// Keeps `e` as its pane's latest; an empty list forgets the pane.
    private mutating func keep(_ e: Entry) {
        if RemotePorts.isEmpty(e.ports) {
            lists[e.host]?[e.pane] = nil
        } else {
            lists[e.host, default: [:]][e.pane] = e
        }
    }

    private static func isEmpty(_ ports: Any) -> Bool { (ports as? [Any])?.isEmpty ?? false }

    /// Whether `e` says what `known` (none: no ports) says.
    private static func same(_ known: Entry?, _ e: Entry) -> Bool {
        guard let known else { return isEmpty(e.ports) }
        return (known.ports as AnyObject).isEqual(e.ports) && (known.complete as AnyObject).isEqual(e.complete)
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

/// The `Ports` request of each host's current connection: a reply to an earlier one (a
/// connection that dropped meanwhile, or one replaced by a reconnect) is not applied.
/// While a `Ports` request is out, the `PortsChanged` events that arrive are kept too: the
/// reply may be older than they are (the daemon looked for it before they were sent), so they
/// are applied again after it, in the order they came.
struct RequestGenerations {
    private var current: [HostId: Int] = [:]
    private var next = 0
    private var arrived: [HostId: [[String: Any]]] = [:]

    /// A request goes out on `host`'s connection now; it supersedes any before it.
    mutating func start(_ host: HostId) -> Int {
        next += 1
        current[host] = next
        arrived[host] = []
        return next
    }

    /// `host`'s connection dropped: no reply pending for it applies any more, nor the events
    /// kept for it.
    mutating func invalidate(_ host: HostId) {
        current[host] = nil
        arrived[host] = nil
    }

    func isCurrent(_ host: HostId, _ generation: Int) -> Bool {
        current[host] == generation
    }

    /// A `PortsChanged` of `host` came: kept while its request is out.
    mutating func event(_ host: HostId, _ payload: [String: Any]) {
        if current[host] != nil { arrived[host, default: []].append(payload) }
    }

    /// The reply to request `generation` came: the events to apply again after it, or nil when
    /// the reply is old (its connection dropped, or a newer request went out).
    mutating func finish(_ host: HostId, _ generation: Int) -> [[String: Any]]? {
        guard current[host] == generation else { return nil }
        current[host] = nil
        return arrived.removeValue(forKey: host) ?? []
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
        Extensions.shared.forwardsNow(Set(forwards.map(\.id)))
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
    /// browser. The first use shows where it went, with Copy Link; a failure says why. Both on
    /// `source`, the pane it was started from, while that pane is still there (`ForwardNotice`).
    func openForwarded(host: HostId, port: Int? = nil, url: String? = nil, from source: TerminalView?) {
        var call: [String: Any] = ["op": "forward_open", "host": host]
        if let port { call["port"] = port }
        if let url { call["url"] = url }
        Extensions.shared.userOpensForward(host)
        weak var source = source
        DispatchQueue.global(qos: .userInitiated).async {
            let v = Core.shared.remoteCall(call) as? [String: Any]
            DispatchQueue.main.async {
                // The user has it now: a plugin's abandoned open of it is not closed under them.
                Extensions.shared.userOpenedForward(host, result: v)
                let pane = source.flatMap { $0.window == nil ? nil : $0 }
                if let e = jsonString(v?["error"]) {
                    SessionManager.show(ForwardNotice.failure(host: host, e, onPane: pane != nil), on: pane)
                    return
                }
                guard let f = ForwardInfo(json: v), let target = URL(string: f.url) else { return }
                if jsonBool(v?["new"]) == true {
                    SessionManager.show(ForwardNotice.opened(host: host, remotePort: f.remotePort, localPort: f.localPort,
                                                             url: f.url, onPane: pane != nil), on: pane)
                }
                NSWorkspace.shared.open(target)
            }
        }
    }

    private static func show(_ notice: ForwardNotice, on pane: TerminalView?) {
        switch notice {
        case .toast(let text, let link):
            pane?.showToast(text, duration: 6, action: link.map { url in ("Copy Link", { copyForwardLink(url) }) })
        case .alert(let text, let link):
            let alert = NSAlert()
            alert.messageText = text
            alert.addButton(withTitle: "OK")
            if let link {
                alert.informativeText = link
                alert.addButton(withTitle: "Copy Link")
            }
            if alert.runModal() == .alertSecondButtonReturn, let link { copyForwardLink(link) }
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

    static func copyForwardLink(_ url: String) {
        let pb = NSPasteboard.general
        pb.clearContents()
        pb.setString(url, forType: .string)
    }

    /// `host` connected: every pane's port list at once, so its forwards don't go by lists of
    /// panes that ended while it was away (no event says so). An older daemon answers an error.
    func fetchRemotePorts(_ host: HostId) {
        let generation = Remotes.shared.portsRequests.start(host)
        Core.shared.requestAsync(object: ["Ports": ["pane": NSNull()]], host: host, timeout: 10) { resp in
            // From a connection that has dropped (or been replaced) since: what it says is old.
            guard let arrived = Remotes.shared.portsRequests.finish(host, generation) else { return }
            guard let v = JSON.variant(resp), v.name == "Ports", let panes = v.payload as? [Any] else { return }
            _ = Core.shared.remoteCall(["op": "ports_snapshot", "host": host, "panes": panes])
            // Events that came while it was on its way may be newer than it: again, in order.
            for payload in arrived { SessionManager.shared.applyRemotePorts(payload, host: host) }
        }
    }

    /// `host`'s connection dropped: what listens there is unknown until it is back.
    func remotePortsUnknown(_ host: HostId) {
        Remotes.shared.portsRequests.invalidate(host)
        _ = Core.shared.remoteCall(["op": "host_disconnected", "host": host])
    }

    /// A `PortsChanged` from a remote host: its forwards learn whether their port is still there.
    func remotePortsChanged(_ payload: [String: Any]?, host: HostId) {
        guard host != localHost, let payload else { return }
        Remotes.shared.portsRequests.event(host, payload)
        applyRemotePorts(payload, host: host)
    }

    fileprivate func applyRemotePorts(_ payload: [String: Any], host: HostId) {
        guard let pane = jsonUInt64(payload["pane"]) else { return }
        _ = Core.shared.remoteCall(["op": "ports_changed", "host": host, "pane": NSNumber(value: pane),
                                    "ports": payload["ports"] ?? [], "complete": payload["complete"] ?? true])
        Extensions.shared.reportPorts(host: host, pane: pane, ports: payload["ports"] ?? [],
                                      complete: payload["complete"] ?? true)
    }
}
