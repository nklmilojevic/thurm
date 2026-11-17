import XCTest
@testable import Thurm

final class ForwardsTests: XCTestCase {
    func testTheForwardLinkAction() {
        XCTAssertEqual(LinkAction(json: ["action": "forward", "port": 5173]), .forward(port: 5173))
        XCTAssertEqual(LinkAction(json: ["action": "open"]), .open)
        XCTAssertEqual(LinkAction(json: ["action": "copy_path", "path": "/etc/hosts"]), .copyPath(path: "/etc/hosts"))
        XCTAssertEqual(LinkAction(json: ["action": "reveal", "path": "/tmp/x"]), .reveal(path: "/tmp/x"))
        XCTAssertEqual(LinkAction(json: ["action": "ask"]), .ask)
        // An error, or an answer this build doesn't know, opens nothing.
        XCTAssertEqual(LinkAction(json: ["error": "bad call"]), .ignore)
        XCTAssertEqual(LinkAction(json: ["action": "teleport"]), .ignore)
    }

    func testToastText() {
        XCTAssertEqual(ForwardText.toast(host: "devbox", remotePort: 5173, localPort: 5173),
                       "devbox:5173 → localhost:5173")
        XCTAssertEqual(ForwardText.toast(host: "pi", remotePort: 80, localPort: 50123), "pi:80 → localhost:50123")
        XCTAssertEqual(ForwardText.failure(host: "pi", "pi is not connected"),
                       "Could not forward from pi: pi is not connected")
    }

    func testForwardInfoFromJSON() {
        let f = ForwardInfo(json: ["id": 3, "host": "devbox", "remote_port": 5173, "local_port": 5174,
                                   "url": "http://localhost:5174/app"])
        XCTAssertEqual(f?.id, 3)
        XCTAssertEqual(f?.remotePort, 5173)
        XCTAssertEqual(f?.localPort, 5174)
        XCTAssertEqual(f?.url, "http://localhost:5174/app")
        XCTAssertEqual(ForwardInfo(json: ["id": 3, "host": "devbox", "remote_port": 1, "local_port": 2])?.url,
                       "http://localhost:2/")
        XCTAssertNil(ForwardInfo(json: ["id": 3, "host": "devbox"]))
    }

    func testProcessPanelRowsOpenRemotePortsThroughAForward() {
        XCTAssertEqual(ProcessPanel.opening(pane: PaneKey(localHost, 4), port: 3000),
                       .local(URL(string: "http://localhost:3000")!))
        XCTAssertEqual(ProcessPanel.opening(pane: PaneKey("devbox", 4), port: 3000),
                       .forward(host: "devbox", port: 3000))
    }

    func testForwardPortPickerTakesAPortOrALink() {
        XCTAssertEqual(ForwardPicker.parse(" 5173 "), .port(5173))
        XCTAssertEqual(ForwardPicker.parse("http://localhost:8080/x"), .url("http://localhost:8080/x"))
        XCTAssertNil(ForwardPicker.parse("0"))
        XCTAssertNil(ForwardPicker.parse("70000"))
        XCTAssertNil(ForwardPicker.parse("vite"))

        let known = [(port: 5173, command: "node vite"), (port: 5432, command: "postgres")]
        let open = [ForwardInfo(json: ["id": 1, "host": "devbox", "remote_port": 5432, "local_port": 15432])!]
        // Nothing typed: the known ports, with what is forwarded already.
        let all = ForwardPicker.choices(query: "", host: "devbox", known: known, open: open)
        XCTAssertEqual(all.map(\.title), ["devbox:5173", "devbox:5432"])
        XCTAssertEqual(all[1].detail, "postgres · forwarded to localhost:15432")
        // A port typed: it comes first, and is not listed twice.
        let typed = ForwardPicker.choices(query: "5173", host: "devbox", known: known, open: open)
        XCTAssertEqual(typed.map(\.target), [.port(5173), .port(5432)])
        XCTAssertEqual(typed[0].title, "Forward devbox:5173")
        // Text filters the known ports.
        XCTAssertEqual(ForwardPicker.choices(query: "postg", host: "devbox", known: known, open: open).map(\.target),
                       [.port(5432)])
        // A link.
        let link = ForwardPicker.choices(query: "http://localhost:3000/", host: "devbox", known: [], open: [])
        XCTAssertEqual(link.map(\.target), [.url("http://localhost:3000/")])
    }

    private func opened(_ id: UInt64, new: Bool = true) -> [String: Any] {
        ["id": id, "host": "devbox", "remote_port": 5173, "local_port": 5173,
         "url": "http://localhost:5173/", "new": new]
    }

    private func key(_ id: UInt64, _ connection: Int = 1) -> ForwardRequests.Key {
        ForwardRequests.Key(connection: connection, id: id)
    }

    func testAForwardRequestTheDaemonGaveUpOnChangesNothing() {
        var r = ForwardRequests()
        // Still waited for: answered, nothing closed.
        r.begin(key(1), opening: "devbox")
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).0, .answer)
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [])

        // Cancelled while opening, nobody else has it: closed again.
        r.begin(key(2), opening: "devbox")
        r.cancel(key(2))
        let (finish, close) = r.finish(key(2), opening: "devbox", result: opened(8))
        XCTAssertEqual(finish, .abandoned)
        XCTAssertEqual(close, [8])
        // One it found open already is not its own to close; nor is anything after a failure.
        r.begin(key(3), opening: "devbox")
        r.cancel(key(3))
        XCTAssertEqual(r.finish(key(3), opening: "devbox", result: opened(9, new: false)).close, [])
        r.begin(key(4), opening: "devbox")
        r.cancel(key(4))
        XCTAssertEqual(r.finish(key(4), opening: "devbox", result: ["error": "devbox is not connected"]).close, [])
        // A cancelled close is not answered; it closes nothing more.
        r.begin(key(5), opening: nil)
        r.cancel(key(5))
        XCTAssertEqual(r.finish(key(5), opening: nil, result: opened(7)).0, .abandoned)
        XCTAssertEqual(r.finish(key(5), opening: nil, result: opened(7)).close, [])
    }

    /// Two plugins open the same port: the first opens it and is given up on, the second gets
    /// that forward (`new` false). It stays, whichever finishes first.
    func testAnAbandonedOpenLeavesAForwardAnotherCallerHas() {
        // The second finishes first.
        var r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        r.begin(key(2), opening: "devbox")
        r.cancel(key(1))
        XCTAssertEqual(r.finish(key(2), opening: "devbox", result: opened(7, new: false)).0, .answer)
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [])

        // The abandoned one finishes first: its close waits for the other open, which gets it.
        r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        r.begin(key(2), opening: "devbox")
        r.cancel(key(1))
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [])
        XCTAssertEqual(r.finish(key(2), opening: "devbox", result: opened(7, new: false)).close, [])

        // The other open fails: nobody has it, and it goes then.
        r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        r.begin(key(2), opening: "devbox")
        r.cancel(key(1))
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [])
        XCTAssertEqual(r.finish(key(2), opening: "devbox", result: ["error": "timed out"]).close, [7])

        // The user clicking the link meanwhile has it too.
        r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        r.userOpens("devbox")
        r.cancel(key(1))
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [])
        XCTAssertEqual(r.userOpened("devbox", result: opened(7, new: false)), [])
        // Another host's opens don't hold it back.
        r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        r.userOpens("pi")
        r.cancel(key(1))
        XCTAssertEqual(r.finish(key(1), opening: "devbox", result: opened(7)).close, [7])
        // A forward someone had, closed since and its id handed out again, is no one's.
        r = ForwardRequests()
        r.begin(key(1), opening: "devbox")
        _ = r.finish(key(1), opening: "devbox", result: opened(7))
        r.forwardsNow([])
        r.begin(key(2), opening: "devbox")
        r.cancel(key(2))
        XCTAssertEqual(r.finish(key(2), opening: "devbox", result: opened(7)).close, [7])
    }

    /// A daemon started again numbers its requests from 1 again: an old connection's request
    /// that finishes late leaves the new one's with the same id waited for.
    func testRequestsAreScopedToTheirConnection() {
        var r = ForwardRequests()
        r.begin(key(1, 1), opening: "devbox")
        r.cancelAll()
        r.begin(key(1, 2), opening: "devbox")
        XCTAssertEqual(r.finish(key(1, 1), opening: "devbox", result: ["error": "gone"]).0, .abandoned)
        XCTAssertEqual(r.finish(key(1, 2), opening: "devbox", result: opened(7)).0, .answer)
        // And a cancel of the old one's id leaves the new one alone.
        r.begin(key(2, 2), opening: nil)
        r.cancel(key(2, 1))
        XCTAssertEqual(r.finish(key(2, 2), opening: nil, result: opened(7)).0, .answer)
    }

    private func ports(_ list: [Int]) -> [[String: Any]] { list.map { ["port": $0] } }

    /// What `sends` says, as `host/pane:ports` with `*` for initial.
    private func said(_ sends: [RemotePorts.Send]) -> [String] {
        sends.map { s in
            let list = (s.entry.ports as? [[String: Any]] ?? []).compactMap { jsonInt($0["port"]) }
            return "\(s.entry.host)/\(s.entry.pane):\(list.map(String.init).joined(separator: ","))\(s.initial ? "*" : "")"
        }
    }

    private func live(_ p: inout RemotePorts, _ host: HostId, _ pane: UInt64, _ list: [Int]) -> RemotePorts.Send? {
        p.live(host, pane: pane, ports: ports(list), complete: true)
    }

    private func entry(_ host: HostId, _ pane: UInt64, _ list: [Int]) -> RemotePorts.Entry {
        RemotePorts.Entry(host: host, pane: pane, ports: ports(list), complete: true)
    }

    func testRemotePortsAreKeptAndTheirAskEndsWithTheConnection() {
        var p = RemotePorts()
        let ask = p.connected("devbox")
        XCTAssertEqual(said(p.learned("devbox", ask: ask, snapshot: [entry("devbox", 2, [5173]), entry("devbox", 3, [])])),
                       ["devbox/2:5173*"])
        XCTAssertEqual(live(&p, "pi", 1, [22]).map { said([$0]) }, ["pi/1:22"])
        _ = live(&p, "devbox", 3, [8080])
        // An empty list forgets the pane; the rest is there to send a daemon connected later.
        _ = live(&p, "devbox", 3, [])
        XCTAssertEqual(p.all.map { "\($0.host)/\($0.pane)" }, ["devbox/2", "pi/1"])

        // The host goes before its next answer comes: that late answer, and what waited for it,
        // are not taken; its lists go.
        let late = p.connected("devbox")
        XCTAssertNil(live(&p, "devbox", 2, [5173]))
        p.disconnected("devbox")
        XCTAssertEqual(p.learned("devbox", ask: late, snapshot: [entry("devbox", 2, [5173])]).count, 0)
        XCTAssertEqual(p.all.map(\.host), ["pi"])
        // Live again, nothing waits.
        XCTAssertNotNil(live(&p, "devbox", 2, [5173]))

        // Connected again twice: only the newest ask's answer counts, once.
        let old = p.connected("devbox")
        let new = p.connected("devbox")
        XCTAssertEqual(p.learned("devbox", ask: old, snapshot: [entry("devbox", 2, [1])]).count, 0)
        XCTAssertEqual(said(p.learned("devbox", ask: new, snapshot: [entry("devbox", 2, [5173])])), ["devbox/2:5173*"])
        XCTAssertEqual(p.learned("devbox", ask: new, snapshot: [entry("devbox", 2, [5173])]).count, 0)
    }

    /// A server that starts while the host's answer is on its way is a change, not one that
    /// was running already; the lists the host's daemon sends on joining say nothing new.
    func testAPortThatStartsWhileTheAnswerIsOnItsWayIsNews() {
        var p = RemotePorts()
        let ask = p.connected("devbox")
        // The join's lists (as the answer will say), then a server starting in pane 2.
        XCTAssertNil(live(&p, "devbox", 1, [5173]))
        XCTAssertNil(live(&p, "devbox", 2, [8080]))
        let sends = p.learned("devbox", ask: ask, snapshot: [entry("devbox", 1, [5173])])
        XCTAssertEqual(said(sends), ["devbox/1:5173*", "devbox/2:8080"])
    }

    /// An answer older than a change that came meanwhile does not undo it: the change goes after
    /// it, whatever it said.
    func testAnOlderAnswerDoesNotOverwriteALiveList() {
        var p = RemotePorts()
        let ask = p.connected("devbox")
        // Pane 1's server stopped and pane 2's started before the answer (taken earlier) came.
        XCTAssertNil(live(&p, "devbox", 1, []))
        XCTAssertNil(live(&p, "devbox", 2, [9000, 9001]))
        let sends = p.learned("devbox", ask: ask,
                              snapshot: [entry("devbox", 1, [5173]), entry("devbox", 2, [9000])])
        XCTAssertEqual(said(sends), ["devbox/1:5173*", "devbox/2:9000*", "devbox/1:", "devbox/2:9000,9001"])
        XCTAssertEqual(p.all.map { "\($0.pane)" }, ["2"])

        // No answer at all (an error, a timeout): what came meanwhile is what runs.
        let failed = p.connected("pi")
        XCTAssertNil(live(&p, "pi", 4, [3000]))
        XCTAssertEqual(said(p.learned("pi", ask: failed, snapshot: nil)), ["pi/4:3000*"])
    }
    func testAFailureWithoutAPaneIsAnAlert() {
        XCTAssertEqual(ForwardNotice.failure(host: "pi", "pi is not connected", onPane: true),
                       .toast("Could not forward from pi: pi is not connected", copyLink: nil))
        XCTAssertEqual(ForwardNotice.failure(host: "pi", "pi is not connected", onPane: false),
                       .alert("Could not forward from pi: pi is not connected", copyLink: nil))
    }

    func testAFirstForwardWithoutAPaneIsAnAlertWithItsLink() {
        let url = "http://localhost:5173/"
        XCTAssertEqual(ForwardNotice.opened(host: "devbox", remotePort: 5173, localPort: 5173, url: url, onPane: true),
                       .toast("devbox:5173 → localhost:5173", copyLink: url))
        XCTAssertEqual(ForwardNotice.opened(host: "devbox", remotePort: 5173, localPort: 5173, url: url, onPane: false),
                       .alert("devbox:5173 → localhost:5173", copyLink: url))
    }

    func testEventsDuringAPortsRequestAreAppliedAgainAfterItsReply() {
        var g = RequestGenerations()
        // Before any request: nothing is kept.
        g.event("devbox", ["pane": 1])
        let gen = g.start("devbox")
        g.event("devbox", ["pane": 2, "ports": [5173]])
        g.event("devbox", ["pane": 3])
        g.event("pi", ["pane": 9])
        let again = g.finish("devbox", gen)
        XCTAssertEqual(again?.compactMap { jsonUInt64($0["pane"]) }, [2, 3])
        // Done: later events apply as they come, and the same reply twice is old.
        g.event("devbox", ["pane": 4])
        XCTAssertNil(g.finish("devbox", gen))
        // A connection dropped while asking: neither its reply nor its events.
        let dropped = g.start("devbox")
        g.event("devbox", ["pane": 5])
        g.invalidate("devbox")
        XCTAssertNil(g.finish("devbox", dropped))
        // Asked twice: only the newer request's reply counts.
        let first = g.start("devbox")
        let second = g.start("devbox")
        XCTAssertNil(g.finish("devbox", first))
        XCTAssertEqual(g.finish("devbox", second)?.count, 0)
    }

    func testForwardPortAsksWhichHostOnlyWhenItCannotTell() {
        XCTAssertEqual(ForwardHostChoice.of(focused: "devbox", connected: ["a", "devbox"]), .host("devbox"))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: ["devbox"]), .host("devbox"))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: ["a", "devbox"]), .pick(["a", "devbox"]))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: []), .none)
    }

    func testAPortsReplyFromAnOldConnectionIsDropped() {
        var g = RequestGenerations()
        // Asked, then the connection drops before the answer: the answer is old.
        let first = g.start("devbox")
        XCTAssertTrue(g.isCurrent("devbox", first))
        g.invalidate("devbox")
        XCTAssertFalse(g.isCurrent("devbox", first))
        // Reconnected and asked again: only the newer answer counts, whichever comes last.
        let second = g.start("devbox")
        XCTAssertFalse(g.isCurrent("devbox", first))
        XCTAssertTrue(g.isCurrent("devbox", second))
        // Hosts are apart.
        let other = g.start("pi")
        XCTAssertTrue(g.isCurrent("devbox", second))
        XCTAssertTrue(g.isCurrent("pi", other))
    }

    func testEveryOpenForwardKeepsCopyLinkAndClose() {
        let f = ForwardInfo(json: ["id": 4, "host": "devbox", "remote_port": 8443, "local_port": 18443,
                                   "url": "https://localhost:18443/app"])!
        let entries = ForwardPicker.manageEntries([f])
        XCTAssertEqual(entries.map(\.title), ["Copy Forward Link devbox:8443", "Close Forward devbox:8443"])
        XCTAssertEqual(entries[0].action, .copyLink("https://localhost:18443/app"))
        XCTAssertEqual(entries[1].action, .close(4))
        XCTAssertTrue(ForwardPicker.manageEntries([]).isEmpty)
    }
}
