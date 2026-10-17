import XCTest
@testable import Thurm

final class HoverCardTests: XCTestCase {
    private func key(_ id: UInt64, host: HostId = localHost) -> PaneKey { PaneKey(host, id) }

    private func info(_ id: UInt64, host: HostId = localHost, _ fields: [String: Any] = [:]) -> PaneInfo {
        var json: [String: Any] = ["id": id, "title": "", "alive": true]
        json.merge(fields) { $1 }
        return PaneInfo(json: json, host: host)!
    }

    private func agent(_ status: String, message: String? = nil, topic: String? = nil) -> [String: Any] {
        var a: [String: Any] = ["name": "Claude Code", "kind": "claude", "status": status]
        a["message"] = message
        a["topic"] = topic
        return ["agent": a]
    }

    func testOnePaneHasNoMapAndNoCount() {
        let model = HoverCardModel.make(root: .leaf(key(1)), focused: key(1), zoomed: nil, titleOverride: nil,
                                        host: localHost,
                                        panes: [key(1): info(1, ["title": "make test", "cwd": "/tmp/x"])])
        XCTAssertEqual(model.title, "make test")
        XCTAssertNil(model.subtitle)
        XCTAssertTrue(model.cells.isEmpty)
        XCTAssertEqual(model.panes.map(\.key), [key(1)])
        XCTAssertEqual(model.panes[0].place, abbreviatePath("/tmp/x"))
        XCTAssertTrue(model.panes[0].focused)
    }

    func testSplitsMapToTheUnitSquareInLayoutOrder() {
        // a | (b over c)
        let root = SplitNode.split(axis: .horizontal, ratio: 0.5, first: .leaf(key(1)),
                                   second: .split(axis: .vertical, ratio: 0.5, first: .leaf(key(2)), second: .leaf(key(3))))
        let model = HoverCardModel.make(root: root, focused: key(2), zoomed: nil, titleOverride: "work",
                                        host: localHost, panes: [:])
        XCTAssertEqual(model.title, "work")
        XCTAssertEqual(model.subtitle, "3 panes")
        XCTAssertEqual(model.panes.map(\.key), [key(1), key(2), key(3)])
        XCTAssertEqual(model.panes.map(\.focused), [false, true, false])
        XCTAssertEqual(model.cells.map(\.key), [key(1), key(2), key(3)])
        let union = model.cells.map(\.rect).reduce(CGRect.null) { $0.union($1) }
        XCTAssertEqual(union.minX, 0, accuracy: 0.001)
        XCTAssertEqual(union.minY, 0, accuracy: 0.001)
        XCTAssertEqual(union.maxX, 1, accuracy: 0.001)
        XCTAssertEqual(union.maxY, 1, accuracy: 0.001)
        XCTAssertEqual(model.cells[0].rect.width, 0.5, accuracy: 0.01)
        XCTAssertEqual(model.cells[1].rect.height, 0.5, accuracy: 0.01)
    }

    func testAgentPanesSayWhatTheyWaitFor() {
        let model = HoverCardModel.make(
            root: .leaf(key(1)), focused: key(1), zoomed: nil, titleOverride: nil, host: localHost,
            panes: [key(1): info(1, agent("NeedsInput", message: "Claude needs your permission to use Bash",
                                          topic: "Fix login bug"))])
        let pane = model.panes[0]
        XCTAssertEqual(pane.title, "Fix login bug")
        XCTAssertEqual(pane.status, .needsInput)
        XCTAssertEqual(pane.detail, "Needs your permission to use Bash")
        XCTAssertEqual(pane.icon, .claude)
    }

    func testAWaitingAgentStaysWhenPanesAreLeftOut() {
        var root = SplitNode.leaf(key(1))
        var panes = [key(1): info(1)]
        for id in UInt64(2)...8 {
            root = .split(axis: .horizontal, ratio: 0.5, first: root, second: .leaf(key(id)))
            panes[key(id)] = info(id)
        }
        panes[key(8)] = info(8, agent("NeedsInput"))
        let model = HoverCardModel.make(root: root, focused: key(1), zoomed: nil, titleOverride: nil,
                                        host: localHost, panes: panes)
        XCTAssertEqual(model.panes.count, HoverCardModel.maxPanes)
        XCTAssertEqual(model.more, 8 - HoverCardModel.maxPanes)
        // Still in layout order, the waiting one last.
        XCTAssertEqual(model.panes.last?.key, key(8))
        XCTAssertEqual(model.panes.map(\.key), [1, 2, 3, 4, 5, 8].map { key($0) })
        // The map still shows every pane.
        XCTAssertEqual(model.cells.count, 8)
    }

    func testProgramsGitAndRemotes() {
        let remote: HostId = "devbox"
        let root = SplitNode.split(axis: .vertical, ratio: 0.5, first: .leaf(key(1, host: remote)),
                                   second: .leaf(key(2, host: remote)))
        let model = HoverCardModel.make(
            root: root, focused: key(1, host: remote), zoomed: key(2, host: remote), titleOverride: nil, host: remote,
            panes: [
                key(1, host: remote): info(1, host: remote, [
                    "title": "persist.rs", "cwd": "/home/me/thurm",
                    "foreground": ["name": "nvim"], "program": ["id": "neovim", "name": "Neovim", "icon": "neovim"],
                    "git": ["root": "/home/me/thurm", "branch": "main", "added": 4, "removed": 1],
                ]),
                key(2, host: remote): info(2, host: remote, ["title": "zsh", "cwd": "/home/me", "alive": false]),
            ])
        XCTAssertEqual(model.subtitle, "2 panes · devbox · zsh zoomed")
        let editor = model.panes[0]
        XCTAssertEqual(editor.detail, "Running Neovim")
        XCTAssertEqual(editor.place, "main")
        XCTAssertEqual(editor.added, 4)
        XCTAssertEqual(editor.removed, 1)
        let shell = model.panes[1]
        XCTAssertEqual(shell.detail, "Exited")
        // A remote path is the host's: not abbreviated against this Mac's home.
        XCTAssertEqual(shell.place, "/home/me")
    }
}
