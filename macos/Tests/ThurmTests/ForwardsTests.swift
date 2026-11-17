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
        // The connections it carries come with the list, not with an answer about one forward.
        XCTAssertNil(f?.connections)
        XCTAssertEqual(ForwardInfo(json: ["id": 3, "host": "devbox", "remote_port": 1, "local_port": 2,
                                          "connections": 2])?.connections, 2)
        XCTAssertNil(ForwardText.connections(nil))
        XCTAssertNil(ForwardText.connections(0))
        XCTAssertEqual(ForwardText.connections(1), "1 connection")
        XCTAssertEqual(ForwardText.connections(4), "4 connections")
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
        let waited = r.finish(key(1), opening: "devbox", result: opened(7))
        XCTAssertEqual(waited.0, .answer)
        XCTAssertEqual(waited.close, [])

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
        let cancelledClose = r.finish(key(5), opening: nil, result: opened(7))
        XCTAssertEqual(cancelledClose.0, .abandoned)
        XCTAssertEqual(cancelledClose.close, [])
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

    func testRemotePortsAreKeptUntilTheirHostGoes() {
        var p = RemotePorts()
        p.listed("devbox", pane: 2, ports: ports([5173]), complete: true)
        p.listed("devbox", pane: 3, ports: ports([8080]), complete: true)
        p.listed("pi", pane: 1, ports: ports([22]), complete: true)
        // An empty list is kept too: a daemon connected later must hear the servers went.
        p.listed("devbox", pane: 3, ports: [] as [Any], complete: true)
        XCTAssertEqual(p.all.map { "\($0.host)/\($0.pane)" }, ["devbox/2", "devbox/3", "pi/1"])
        // An empty list that could not read every process is kept: servers it missed may run,
        // and a daemon connected later must hear the pane is unsure.
        p.listed("devbox", pane: 4, ports: [] as [Any], complete: false)
        XCTAssertEqual(p.all.map { "\($0.host)/\($0.pane)" }, ["devbox/2", "devbox/3", "devbox/4", "pi/1"])
        XCTAssertEqual(p.all.first { $0.pane == 4 }.flatMap { jsonBool($0.complete) }, false)
        p.listed("devbox", pane: 4, ports: [] as [Any], complete: true)
        p.disconnected("devbox")
        XCTAssertEqual(p.all.map(\.host), ["pi"])
    }

    /// Connected again to this Mac's daemon, the app sends its remote lists again: those its
    /// plugins heard as running already; one they never heard (sent while the daemon was away, or
    /// taken without being passed on, as a second app's are) as it came, so a server that started
    /// meanwhile is still news to them. A pane whose servers went while the daemon was away is
    /// sent again too; one whose going plugins heard is not kept.
    func testRemoteListsSentAgainKeepWhatTheDaemonNeverHeard() {
        let ext = Extensions.shared
        var daemonUp = true
        var reported: [[String: Any]] = []
        ext.portsReported = { reported.append($0); return daemonUp }
        defer { ext.portsReported = nil }
        let host = "again-test"
        defer { ext.remoteHostDisconnected(host) }
        let list = { (port: Int) -> [[String: Any]] in [["port": port]] }
        // Heard while the daemon was there: pane 1 running already, pane 2 a change.
        ext.reportPorts(host: host, pane: 1, ports: list(5173), complete: true, initial: true)
        ext.reportPorts(host: host, pane: 2, ports: list(8080), complete: true, initial: false)
        ext.reportPorts(host: host, pane: 5, ports: list(5000), complete: true, initial: false)
        // The daemon goes; pane 3 starts a server, pane 4 is listed as running already.
        daemonUp = false
        ext.reportPorts(host: host, pane: 3, ports: list(3000), complete: true, initial: false)
        ext.reportPorts(host: host, pane: 4, ports: list(9000), complete: true, initial: true)
        ext.reportPorts(host: host, pane: 5, ports: [] as [Any], complete: true, initial: false)
        daemonUp = true
        // Pane 2's server went, heard: that is all there is to say of it, and it is not kept.
        ext.reportPorts(host: host, pane: 2, ports: [] as [Any], complete: true, initial: false)
        reported = []
        ext.sendRemotePortsAgain()
        let again = reported.filter { jsonString($0["host"]) == host }
            .map { "\($0["pane"]!):\(($0["ports"] as? [Any])?.count ?? -1)\(jsonBool($0["initial"]) == true ? "*" : "")" }
        XCTAssertEqual(again, ["1:1*", "3:1", "4:1*", "5:0"])
    }

    /// Whether a remote list is of servers running already is the host's daemon's to say, by
    /// `initial` on the lists it sends the app as it joins its watch; every other list is a
    /// change. No request of the app's own decides it: a server that starts right after the app
    /// connected is a change even when a list taken later would show it (a snapshot used to
    /// hide it), and a pane's first list is a change when the host did not mark it (a failed
    /// snapshot used to make it initial).
    func testOnlyAListTheHostMarkedInitialIsRunningAlready() {
        let ext = Extensions.shared
        var reported: [[String: Any]] = []
        ext.portsReported = { reported.append($0); return true }
        defer { ext.portsReported = nil }
        let host = "initial-test"
        func list(_ pane: Int, _ list: [Int], initial: Bool? = nil) -> [String: Any] {
            var payload: [String: Any] = ["pane": pane, "ports": ports(list), "complete": true]
            if let initial { payload["initial"] = initial }
            return payload
        }
        // As the app joins: pane 1 was listening already.
        SessionManager.shared.remotePortsChanged(list(1, [5173], initial: true), host: host)
        // Right after: pane 2 starts a server (its first list), pane 1 another.
        SessionManager.shared.remotePortsChanged(list(2, [8080], initial: false), host: host)
        SessionManager.shared.remotePortsChanged(list(1, [5173, 5174], initial: false), host: host)
        // A list without the key is not taken for one running already.
        SessionManager.shared.remotePortsChanged(list(3, [3000]), host: host)
        XCTAssertEqual(reported.map { "\($0["pane"]!)\(jsonBool($0["initial"]) == true ? "*" : "")" },
                       ["1*", "2", "1", "3"])
        ext.remoteHostDisconnected(host)
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

    func testForwardPortAsksWhichHostOnlyWhenItCannotTell() {
        XCTAssertEqual(ForwardHostChoice.of(focused: "devbox", connected: ["a", "devbox"]), .host("devbox"))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: ["devbox"]), .host("devbox"))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: ["a", "devbox"]), .pick(["a", "devbox"]))
        XCTAssertEqual(ForwardHostChoice.of(focused: localHost, connected: []), .none)
    }

    func testAForwardsLinkIsUsedOnlyWhileItIsOpenOnTheSamePort() {
        let known = ForwardInfo(json: ["id": 4, "host": "devbox", "remote_port": 8443, "local_port": 18443,
                                       "url": "https://localhost:18443/app"])!
        // Still open: its link as it is now.
        XCTAssertEqual(ForwardInfo.liveLink(of: known, answer: ["id": 4, "host": "devbox", "remote_port": 8443,
                                                                "local_port": 18443,
                                                                "url": "https://localhost:18443/admin"]),
                       "https://localhost:18443/admin")
        // Closed since: nothing to open or copy.
        XCTAssertNil(ForwardInfo.liveLink(of: known, answer: ["error": "that forward has closed"]))
        // Another forward, or the same id on another port: not this one.
        XCTAssertNil(ForwardInfo.liveLink(of: known, answer: ["id": 5, "host": "devbox", "remote_port": 8443,
                                                              "local_port": 18443]))
        XCTAssertNil(ForwardInfo.liveLink(of: known, answer: ["id": 4, "host": "devbox", "remote_port": 8443,
                                                              "local_port": 18444]))
    }

    func testARequestOpensItsOwnLinkNotTheOneReusedSince() {
        // Two links to one port, opened close together: the forward now holds the second.
        let first = ForwardInfo(json: ["id": 4, "host": "devbox", "remote_port": 3000, "local_port": 3000,
                                       "url": "http://localhost:3000/orders/1/cancel"])!
        let now: [String: Any] = ["id": 4, "host": "devbox", "remote_port": 3000, "local_port": 3000,
                                  "url": "http://localhost:3000/admin"]
        // The first request still opens what it asked for.
        XCTAssertEqual(ForwardInfo.requestedLink(first, answer: now), "http://localhost:3000/orders/1/cancel")
        // Lists' Open and Copy Link go by the link it holds now.
        XCTAssertEqual(ForwardInfo.liveLink(of: first, answer: now), "http://localhost:3000/admin")
        // Closed meanwhile, or on another port now: nothing.
        XCTAssertNil(ForwardInfo.requestedLink(first, answer: ["error": "that forward has closed"]))
        XCTAssertNil(ForwardInfo.requestedLink(first, answer: ["id": 4, "host": "devbox", "remote_port": 3000,
                                                               "local_port": 3001]))
    }

    func testEveryOpenForwardKeepsCopyLinkAndClose() {
        let f = ForwardInfo(json: ["id": 4, "host": "devbox", "remote_port": 8443, "local_port": 18443,
                                   "url": "https://localhost:18443/app"])!
        let entries = ForwardPicker.manageEntries([f])
        XCTAssertEqual(entries.map(\.title), ["Copy Forward Link devbox:8443", "Close Forward devbox:8443"])
        XCTAssertEqual(entries[0].action, .copyLink(f))
        XCTAssertEqual(entries[1].action, .close(4))
        XCTAssertTrue(ForwardPicker.manageEntries([]).isEmpty)
    }
}
