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
