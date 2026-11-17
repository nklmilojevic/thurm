import AppKit

// Forwards: a remote host's TCP port opened on this Mac's loopback (Rust:
// crates/thurm-remote/src/forward.rs). A `localhost` link in a remote pane, a remote row of
// Processes & Ports, Forward Port…, or a plugin's request (`thurm.forward.open`: code the user
// installed) opens one; nothing is forwarded without one of those.

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

/// The forwards being closed because nobody waits for them any more, by host. An open of a
/// host starts once none of its closes is under way: forwards are shared, so it could be
/// handed one of them just before it closes.
final class ForwardCloses: @unchecked Sendable {
    private let cond = NSCondition()
    private var closing: [HostId: Int] = [:]

    func begin(_ host: HostId) {
        cond.lock()
        closing[host, default: 0] += 1
        cond.unlock()
    }

    func end(_ host: HostId) {
        cond.lock()
        if let n = closing[host] { closing[host] = n > 1 ? n - 1 : nil }
        cond.broadcast()
        cond.unlock()
    }

    /// Waits until no close of `host` is under way. Off the main thread: a close can take
    /// seconds.
    func wait(_ host: HostId) {
        cond.lock()
        while closing[host] != nil { cond.wait() }
        cond.unlock()
    }
}

/// The remote hosts' port lists the app passes on to this Mac's daemon for its plugins
/// (`ports.report`): the latest of each pane, sent again whenever the app connects to the
/// daemon. Whether a list is of servers running already (`initial`) is the host's daemon's to
/// say: it marks the lists it sends the app as it joins the watch.
struct RemotePorts {
    struct Entry {
        let host: HostId
        let pane: UInt64
        let ports: Any
        let complete: Any
        /// The host's daemon sent it as running already, not as a change.
        let initial: Bool
        /// Which report it is, for `delivered`.
        let seq: Int
        /// This Mac's daemon passed it on to its plugins (`ports.report` answered `heard`): it
        /// took a list from an app that does not hold plugins' lists without passing it on.
        var delivered = false

        /// What it is said as when sent again to a daemon the app connects to: running already
        /// once plugins heard it (and a new daemon's knew nothing before); else as it came, a
        /// change they never heard.
        var initialWhenSentAgain: Bool { delivered || initial }
    }

    private var lists: [HostId: [UInt64: Entry]] = [:]
    private var seq = 0

    /// A pane of `host` listed `ports`: kept as its latest, an empty list too (a daemon this app
    /// connects to later must hear that the pane's servers went, and an incomplete one that
    /// servers may run unseen), until the host goes. The number of this report, for `delivered`.
    @discardableResult
    mutating func listed(_ host: HostId, pane: UInt64, ports: Any, complete: Any, initial: Bool = false) -> Int {
        seq += 1
        lists[host, default: [:]][pane] = Entry(host: host, pane: pane, ports: ports, complete: complete,
                                                initial: initial, seq: seq)
        return seq
    }

    /// This Mac's daemon passed report `seq` of `host`'s `pane` on to plugins (if it is still the
    /// latest). An empty, complete one is all said then, and goes: a closed pane's would pile up.
    mutating func delivered(_ host: HostId, pane: UInt64, seq: Int) {
        guard let e = lists[host]?[pane], e.seq == seq else { return }
        if (e.ports as? [Any])?.isEmpty ?? false, jsonBool(e.complete) != false {
            lists[host]?[pane] = nil
        } else {
            lists[host]?[pane]?.delivered = true
        }
    }

    /// The connection to `host` is gone: its lists with it.
    mutating func disconnected(_ host: HostId) {
        lists[host] = nil
    }

    /// Every list known now, by host and pane.
    var all: [Entry] {
        lists.keys.sorted().flatMap { host in
            (lists[host] ?? [:]).keys.sorted().compactMap { lists[host]?[$0] }
        }
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
        let closes = Extensions.shared.forwardCloses
        DispatchQueue.global(qos: .userInitiated).async {
            closes.wait(host)
            let v = Core.shared.remoteCall(call) as? [String: Any]
            DispatchQueue.main.async {
                // The user has it now: a plugin's abandoned open of it is not closed under them.
                Extensions.shared.userOpenedForward(host, result: v)
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
        // This Mac's plugins hear it too: running already only when the host's daemon says so.
        Extensions.shared.reportPorts(host: host, pane: pane, ports: payload["ports"] ?? [],
                                      complete: payload["complete"] ?? true,
                                      initial: jsonBool(payload["initial"]) ?? false)
    }
}
