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

    func testRemotePortsAreKeptAndTheirAskEndsWithTheConnection() {
        var p = RemotePorts()
        let ask = p.connected("devbox")
        XCTAssertTrue(p.isLearning("devbox"))
        p.listed("devbox", pane: 2, ports: [["port": 5173]], complete: true)
        p.listed("devbox", pane: 3, ports: [["port": 8080]], complete: true)
        p.listed("pi", pane: 1, ports: [["port": 22]], complete: true)
        // An empty list forgets the pane; the rest is there to send a daemon connected later.
        p.listed("devbox", pane: 3, ports: [] as [Any], complete: true)
        XCTAssertEqual(p.all.map { "\($0.host)/\($0.pane)" }, ["devbox/2", "pi/1"])

        // The host goes before its answer comes: the late answer is not taken, its lists go.
        p.disconnected("devbox")
        XCTAssertFalse(p.isLearning("devbox"))
        XCTAssertFalse(p.learned("devbox", ask: ask))
        XCTAssertEqual(p.all.map(\.host), ["pi"])

        // Connected again: only the newest ask's answer counts, once.
        let old = p.connected("devbox")
        let new = p.connected("devbox")
        XCTAssertFalse(p.learned("devbox", ask: old))
        XCTAssertTrue(p.isLearning("devbox"))
        XCTAssertTrue(p.learned("devbox", ask: new))
        XCTAssertFalse(p.isLearning("devbox"))
        XCTAssertFalse(p.learned("devbox", ask: new))
    }
    func testAFailureWithoutAPaneIsAnAlert() {
        XCTAssertEqual(ForwardFailure.report(host: "pi", "pi is not connected", hasView: true),
                       .toast("Could not forward from pi: pi is not connected"))
        XCTAssertEqual(ForwardFailure.report(host: "pi", "pi is not connected", hasView: false),
                       .alert("Could not forward from pi: pi is not connected"))
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
