import XCTest
@testable import Thurm

final class ExtensionsTests: XCTestCase {
    /// Without the daemon's actions the menu has the config's commands; with them, every
    /// action but the app's own, and the daemon's bindings of the app's actions join
    /// `[keybindings]`.
    func testTheDaemonsActionsBuildTheCommandsMenu() {
        let ext = Extensions()
        let config = [UserCommand(name: "Deploy", key: "cmd+shift+d")]
        let (keys, commands) = ext.menuInput(keybindings: ["cmd+k": "new_tab"], commands: config)
        XCTAssertEqual(keys, ["cmd+k": "new_tab"])
        XCTAssertEqual(commands, config)

        ext.load(actions: [
            .init(name: "Deploy", title: "", category: "Commands", source: "config", hidden: false),
            .init(name: "git.sync", title: "Sync", category: "Git", source: "plugin:git", hidden: false),
            .init(name: "init.key.1", title: "cmd+u", category: "", source: "plugin:init", hidden: true),
            .init(name: "init.key.2", title: "", category: "", source: "plugin:init", hidden: true),
            .init(name: "split_down", title: "Split Down", category: "Shell", source: "Thurm.app", hidden: false),
        ], bindings: [
            .init(key: "cmd+shift+d", action: "Deploy"),
            .init(key: "cmd+u", action: "init.key.1"),
            .init(key: "cmd+shift+j", action: "split_down"),
        ], appActions: ["split_down"])
        let (merged, entries) = ext.menuInput(keybindings: ["cmd+k": "new_tab"], commands: config)
        XCTAssertEqual(merged, ["cmd+k": "new_tab", "cmd+shift+j": "split_down"])
        XCTAssertEqual(entries.map(\.name), ["Deploy", "git.sync", "init.key.1"])
        XCTAssertEqual(entries[0].key, "cmd+shift+d")
        XCTAssertEqual(entries[1].displayTitle, "Sync")
        XCTAssertEqual(entries[1].category, "Git")
        XCTAssertTrue(entries[2].hidden)
    }

    /// A second chord of an action, and a key with arguments, each get a hidden entry that
    /// runs the action (with those arguments); the visible entry keeps the first plain key.
    func testEveryKeyOfAnActionRunsIt() {
        let ext = Extensions()
        ext.load(actions: [
            .init(name: "git.checkout", title: "Checkout", category: "Git", source: "plugin:git", hidden: false),
        ], bindings: [
            .init(key: "cmd+shift+o", action: "git.checkout"),
            .init(key: "cmd+alt+o", action: "git.checkout"),
            .init(key: "cmd+alt+m", action: "git.checkout", args: "{\"branch\":\"main\"}"),
        ], appActions: [])
        let (_, entries) = ext.menuInput(keybindings: [:], commands: [])
        XCTAssertEqual(entries.map(\.key), ["cmd+shift+o", "cmd+alt+o", "cmd+alt+m"])
        XCTAssertEqual(entries.map(\.hidden), [false, true, true])
        XCTAssertEqual(entries.map(\.args), [nil, nil, "{\"branch\":\"main\"}"])
        XCTAssertTrue(entries.allSatisfy { $0.name == "git.checkout" })
    }

    /// A key bound with arguments to an app action (which takes none) is not bound.
    func testAppActionKeysWithArgumentsAreNotBound() {
        let ext = Extensions()
        ext.load(actions: [], bindings: [
            .init(key: "cmd+shift+j", action: "split_down", args: "{\"x\":1}"),
            .init(key: "cmd+shift+k", action: "split_down"),
        ], appActions: ["split_down"])
        let (keys, _) = ext.menuInput(keybindings: [:], commands: [])
        XCTAssertEqual(keys, ["cmd+shift+k": "split_down"])
    }

    /// The daemon's key for an app action replaces the file's for the same chord, however
    /// either spells it.
    func testTheDaemonsKeyWinsOverTheFilesSameChord() {
        let ext = Extensions()
        ext.load(actions: [], bindings: [
            .init(key: "CMD+T", action: "split_down"),
            .init(key: "shift+cmd+k", action: "split_down"),
        ], appActions: ["split_down"])
        let (keys, _) = ext.menuInput(keybindings: ["cmd+t": "new_tab", "cmd+shift+k": "clear",
                                                    "cmd+n": "new_window"], commands: [])
        XCTAssertEqual(keys, ["CMD+T": "split_down", "shift+cmd+k": "split_down",
                              "cmd+n": "new_window"])
    }

    /// The daemon's questions and the answers it takes, as `ui_contract.json` (which the
    /// protocol's own tests check too) has them: the app reads every field and answers in kind.
    func testPluginQuestionsAndAnswersMatchTheDaemon() throws {
        let url = URL(fileURLWithPath: #filePath)
            .deletingLastPathComponent().deletingLastPathComponent().deletingLastPathComponent()
            .deletingLastPathComponent()
            .appendingPathComponent("crates/thurm-proto/tests/ui_contract.json")
        let contract = try XCTUnwrap(JSONSerialization.jsonObject(with: Data(contentsOf: url)) as? [String: Any])
        let requests = try XCTUnwrap(contract["requests"] as? [[String: Any]]).map { UIQuestion($0)! }
        let replies = try XCTUnwrap(contract["replies"] as? [[String: Any]])

        let (toast, pick, input, confirm) = (requests[0], requests[1], requests[2], requests[3])
        XCTAssertEqual(toast.kind, "toast")
        XCTAssertEqual(toast.text, "saved")
        XCTAssertEqual(toast.pane, 4)
        XCTAssertEqual(pick.kind, "pick")
        XCTAssertEqual(pick.title, "Branch")
        XCTAssertEqual(pick.items.map(\.title), ["main", "dev"])
        XCTAssertEqual(pick.items.map(\.detail), ["origin", ""])
        XCTAssertEqual(input.placeholder, "name")
        XCTAssertEqual(input.value, "x")
        XCTAssertEqual(confirm.body, "It goes.")
        XCTAssertEqual(confirm.ok, "Delete")
        XCTAssertEqual(confirm.cancel, "Cancel")

        let sent: [[String: Any]] = [
            pick.reply(pick.picked(1)),
            input.reply("typed"),
            confirm.reply(true),
            input.reply(NSNull()),
        ]
        for (mine, theirs) in zip(sent, replies) {
            // Through JSON, as the daemon gets them.
            let encoded = try XCTUnwrap(JSON.decode(JSON.encode(mine)) as? NSDictionary)
            XCTAssertEqual(encoded, theirs as NSDictionary)
        }
    }

    /// A plugin's list to pick from shows with no window open: on the screen, near its top.
    func testAPickWithNoWindowShowsOnTheScreen() {
        let size = NSSize(width: 560, height: 340)
        let screen = NSRect(x: 0, y: 25, width: 1440, height: 875)
        let origin = CommandPalette.origin(size: size, over: nil, screen: screen)
        let frame = NSRect(origin: origin, size: size)
        XCTAssertTrue(screen.contains(frame), "\(frame)")
        XCTAssertEqual(frame.midX, screen.midX)
        let window = NSRect(x: 100, y: 100, width: 800, height: 600)
        let over = NSRect(origin: CommandPalette.origin(size: size, over: window, screen: screen), size: size)
        XCTAssertEqual(over.midX, window.midX)
        XCTAssertEqual(over.maxY, window.maxY - 80)
    }

    /// The main menu's named items are offered once each, but never secure keyboard entry.
    func testSecureKeyboardEntryIsNotOffered() {
        func item(_ title: String, _ name: String) -> NSMenuItem {
            let i = NSMenuItem(title: title, action: nil, keyEquivalent: "")
            i.identifier = NSUserInterfaceItemIdentifier("thurm.action.\(name)")
            return i
        }
        let shell = NSMenu(title: "Shell")
        shell.addItem(item("Split Down", "split_down"))
        shell.addItem(item("Secure Keyboard Entry", "secure_keyboard_entry"))
        shell.addItem(item("Split Down Again", "split_down"))
        shell.addItem(NSMenuItem(title: "No Name", action: nil, keyEquivalent: ""))
        let top = NSMenuItem(title: "Shell", action: nil, keyEquivalent: "")
        top.submenu = shell
        let main = NSMenu(title: "Main")
        main.addItem(top)
        let (names, list) = Extensions.offers(in: main)
        XCTAssertEqual(names, ["split_down"])
        XCTAssertEqual(list.count, 1)
        XCTAssertEqual(list.first?["name"] as? String, "split_down")
        XCTAssertEqual(list.first?["category"] as? String, "Shell")
    }

    /// A question goes over a terminal window, never over the palette (or another panel) that
    /// happens to be key; with no terminal window, over none.
    func testAQuestionGoesOverATerminalWindowNotThePalette() {
        _ = NSApplication.shared
        let palette = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                              styleMask: [.titled], backing: .buffered, defer: true)
        let plain = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                             styleMask: [.titled], backing: .buffered, defer: true)
        XCTAssertNil(Extensions.questionWindow(key: palette, current: nil))
        XCTAssertNil(Extensions.questionWindow(key: palette, current: palette))
        XCTAssertTrue(Extensions.questionWindow(key: palette, current: plain) === plain)
        XCTAssertNil(Extensions.questionWindow(key: nil, current: nil))
    }

    /// An action for a pane goes to its window's responder chain, not the key window's; one
    /// nothing there takes is not sent.
    func testAnActionForAPaneGoesToItsWindow() {
        _ = NSApplication.shared
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                              styleMask: [.titled], backing: .buffered, defer: true)
        let view = PaneActionView(frame: window.contentLayoutRect)
        window.contentView = view
        window.makeFirstResponder(view)
        let item = NSMenuItem(title: "Split", action: #selector(PaneActionView.splitForTest(_:)),
                              keyEquivalent: "")
        let found = Extensions.target(for: #selector(PaneActionView.splitForTest(_:)), item: item, in: window)
        XCTAssertTrue(found?.target === view)
        XCTAssertEqual(found?.ofWindow, true)
        // An item with a target of its own: that one, which is not of the window.
        let owner = PaneActionView(frame: .zero)
        item.target = owner
        let own = Extensions.target(for: #selector(PaneActionView.splitForTest(_:)), item: item, in: window)
        XCTAssertTrue(own?.target === owner)
        XCTAssertEqual(own?.ofWindow, false)
        item.target = nil
        let other = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 100, height: 100),
                             styleMask: [.titled], backing: .buffered, defer: true)
        XCTAssertNil(Extensions.target(for: #selector(PaneActionView.splitForTest(_:)),
                                       item: item, in: other))
        XCTAssertNil(Extensions.target(for: #selector(PaneActionView.splitForTest(_:)),
                                       item: item, in: nil))
    }
    /// A plugin's forward request the daemon gave up on (`ui.cancel`) is not carried out once
    /// the app has seen the cancel: a close is done as soon as it is asked, never queued behind
    /// a cancel, and an open cancelled before it starts never opens.
    func testACancelledForwardRequestIsNotCarriedOut() {
        let ext = Extensions.shared
        let saved = ext.remoteCall
        defer { ext.remoteCall = saved }
        let lock = NSLock()
        var calls: [String] = []
        ext.remoteCall = { op in
            lock.lock()
            calls.append(jsonString(op["op"]) ?? "")
            lock.unlock()
            return ["error": "not in a test"]
        }
        func seen() -> [String] {
            lock.lock()
            defer { lock.unlock() }
            return calls
        }
        func event(_ name: String, _ payload: [String: Any]) -> [String: Any] {
            ["name": name, "payload": JSON.encode(payload)]
        }

        ext.handle(event("ui.request", ["kind": "forward_close", "id": 901, "forward": 7]))
        XCTAssertEqual(seen(), ["forward_close"], "the close was queued, where a cancel could not stop it")
        ext.handle(event("ui.cancel", ["id": 901]))

        ext.handle(event("ui.request", ["kind": "forward_open", "id": 902, "host": "devbox", "port": 5173]))
        ext.handle(event("ui.cancel", ["id": 902]))
        RunLoop.main.run(until: Date().addingTimeInterval(0.5))
        XCTAssertEqual(seen(), ["forward_close"], "an open the daemon gave up on was started")

        ext.handle(event("ui.request", ["kind": "forward_open", "id": 903, "host": "devbox", "port": 5173]))
        let deadline = Date().addingTimeInterval(5)
        while seen().count < 2, Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.05))
        }
        XCTAssertEqual(seen(), ["forward_close", "forward_open"])
    }

    /// A cancel that lands after an open passed its check, before the call does anything: the
    /// call still runs, and what it did is undone. A forward it opened is closed again, nothing
    /// is answered and no browser opens; one it only found open stays. A close cannot be
    /// overtaken: it is done on the main thread as it is asked, where cancels are taken too.
    func testACancelBetweenTheCheckAndTheCallIsUndone() {
        // A forward opened for a request still waited for shows a toast, over the app's windows.
        _ = NSApplication.shared
        let ext = Extensions.shared
        let saved = ext.remoteCall
        defer { ext.remoteCall = saved }
        let lock = NSLock()
        var calls: [String] = []
        var cancelOnCall: UInt64?
        var openResult: [String: Any] = [:]
        ext.remoteCall = { op in
            let name = jsonString(op["op"]) ?? ""
            lock.lock()
            calls.append(name == "forward_close" ? "close \(jsonUInt64(op["id"]) ?? 0)" : name)
            let cancel = cancelOnCall
            let result = openResult
            lock.unlock()
            // The check passed; the daemon's cancel reaches the app now, before the open does
            // anything.
            if name == "forward_open", let cancel {
                DispatchQueue.main.sync {
                    ext.handle(["name": "ui.cancel", "payload": JSON.encode(["id": cancel])])
                }
            }
            switch name {
            case "forward_open", "forward_get": return result
            default: return ["id": 0]
            }
        }
        func seen() -> [String] {
            lock.lock()
            defer { lock.unlock() }
            return calls
        }
        func open(_ id: UInt64, result: [String: Any], cancel: Bool, until count: Int) -> [String] {
            lock.lock()
            calls = []
            cancelOnCall = cancel ? id : nil
            openResult = result
            lock.unlock()
            ext.handle(["name": "ui.request", "payload": JSON.encode(
                ["kind": "forward_open", "id": id, "host": "undo-test", "port": 5173])])
            let deadline = Date().addingTimeInterval(5)
            while seen().count < count, Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.05))
            }
            RunLoop.main.run(until: Date().addingTimeInterval(0.2))
            return seen()
        }
        let opened: [String: Any] = ["id": 71, "host": "undo-test", "remote_port": 5173, "local_port": 5173,
                                     "url": "http://localhost:5173/", "new": true]
        var found = opened
        found["id"] = 72
        found["new"] = false

        // Cancelled between the check and the call: the forward it opened is closed again.
        XCTAssertEqual(open(911, result: opened, cancel: true, until: 2), ["forward_open", "close 71"])
        // One it only found open is someone else's: it stays.
        XCTAssertEqual(open(912, result: found, cancel: true, until: 1), ["forward_open"])
        // Not cancelled: kept (and looked at again, before anything is shown for it).
        XCTAssertEqual(open(913, result: opened, cancel: false, until: 2), ["forward_open", "forward_get"])
        ext.forwardsNow([])
    }

    /// An open a plugin asked to browse goes to the browser only while its forward is still open
    /// on the same local port, as the app found it: one that closed meanwhile may have left the
    /// port to another program. The plugin then hears it closed, and nothing is opened.
    func testABrowsedForwardIsOpenedOnlyWhileItIsOpen() {
        _ = NSApplication.shared
        let ext = Extensions.shared
        let saved = (ext.remoteCall, ext.browse)
        defer {
            ext.remoteCall = saved.0
            ext.browse = saved.1
            ext.forwardReplies = nil
            ext.forwardsNow([])
        }
        let opened: [String: Any] = ["id": 81, "host": "browse-test", "remote_port": 5173, "local_port": 5174,
                                     "url": "http://localhost:5174/app", "new": true]
        let lock = NSLock()
        var now: Any = ["error": "no forward 81"]
        ext.remoteCall = { op in
            lock.lock()
            defer { lock.unlock() }
            return jsonString(op["op"]) == "forward_get" ? now : opened
        }
        var browsed: [String] = []
        var replies: [[String: Any]] = []
        ext.browse = { browsed.append($0.absoluteString) }
        ext.forwardReplies = { replies.append($0) }
        func open(_ id: UInt64, while state: Any) {
            lock.lock()
            now = state
            lock.unlock()
            replies = []
            ext.handle(["name": "ui.request", "payload": JSON.encode(
                ["kind": "forward_open", "id": id, "host": "browse-test", "port": 5173, "browse": true])])
            let deadline = Date().addingTimeInterval(5)
            while replies.isEmpty, Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.05))
            }
        }
        // Closed before the app could use it: not browsed, and the plugin hears why.
        open(921, while: ["error": "no forward 81"])
        XCTAssertEqual(browsed, [])
        XCTAssertTrue((jsonString(replies.first?["error"]) ?? "").contains("closed"), "\(replies)")
        // Open again under the same id, on another local port: not the one it answered.
        var moved = opened
        moved["local_port"] = 6000
        open(922, while: moved)
        XCTAssertEqual(browsed, [])
        XCTAssertNotNil(jsonString(replies.first?["error"]))
        // Still open: browsed at the link it asked for, and answered.
        open(923, while: opened)
        XCTAssertEqual(browsed, ["http://localhost:5174/app"])
        XCTAssertNil(replies.first?["error"])
        XCTAssertEqual(jsonUInt64((replies.first?["result"] as? [String: Any])?["id"]), 81)
    }
}

private final class PaneActionView: NSView {
    override var acceptsFirstResponder: Bool { true }
    @objc func splitForTest(_ sender: Any?) {}
}
