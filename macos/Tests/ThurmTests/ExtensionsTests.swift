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
        let saved = (ext.remoteCall, ext.forwardReplies, ext.forwardRequestDone)
        defer {
            ext.remoteCall = saved.0
            ext.forwardReplies = saved.1
            ext.forwardRequestDone = saved.2
        }
        let lock = NSLock()
        var calls: [String] = []
        ext.remoteCall = { op in
            lock.lock()
            calls.append(jsonString(op["op"]) ?? "")
            lock.unlock()
            return ["error": "not in a test"]
        }
        var replies: [UInt64: [[String: Any]]] = [:]
        var finished: Set<UInt64> = []
        ext.forwardReplies = { r in replies[jsonUInt64(r["id"]) ?? 0, default: []].append(r) }
        ext.forwardRequestDone = { finished.insert($0) }
        func seen() -> [String] {
            lock.lock()
            defer { lock.unlock() }
            return calls
        }
        func event(_ name: String, _ payload: [String: Any]) -> [String: Any] {
            ["name": name, "payload": JSON.encode(payload)]
        }
        func waitDone(_ id: UInt64) {
            let deadline = Date().addingTimeInterval(5)
            while !finished.contains(id), Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            }
            XCTAssertTrue(finished.contains(id), "request \(id) never finished")
        }

        // A close starts as soon as it is asked: a cancel that comes after cannot stop it, and
        // only keeps it from being answered.
        ext.handle(event("ui.request", ["kind": "forward_close", "id": 901, "forward": 7]))
        ext.handle(event("ui.cancel", ["id": 901]))
        waitDone(901)
        XCTAssertEqual(seen(), ["forward_close"])
        XCTAssertNil(replies[901], "a cancelled close was answered")
        // Not cancelled: closed, and answered.
        ext.handle(event("ui.request", ["kind": "forward_close", "id": 904, "forward": 8]))
        waitDone(904)
        XCTAssertEqual(seen(), ["forward_close", "forward_close"])
        XCTAssertEqual(replies[904]?.count, 1)

        // An open cancelled before it starts: not started, not answered.
        ext.handle(event("ui.request", ["kind": "forward_open", "id": 902, "host": "devbox", "port": 5173]))
        ext.handle(event("ui.cancel", ["id": 902]))
        waitDone(902)
        XCTAssertEqual(seen(), ["forward_close", "forward_close"], "an open the daemon gave up on was started")
        XCTAssertNil(replies[902], "a cancelled open was answered")

        // One still waited for: started, and answered (here, with the core's error).
        ext.handle(event("ui.request", ["kind": "forward_open", "id": 903, "host": "devbox", "port": 5173]))
        waitDone(903)
        XCTAssertEqual(seen(), ["forward_close", "forward_close", "forward_open"])
        XCTAssertEqual(replies[903]?.count, 1)
        XCTAssertEqual(jsonString(replies[903]?.first?["error"]), "not in a test")
    }

    /// A cancel that lands after an open passed its check, before the call does anything: the
    /// call still runs, and what it did is undone. A forward it opened is closed again, nothing
    /// is answered and no browser opens; one it only found open stays. A close cannot be
    /// overtaken: it is done on the main thread as it is asked, where cancels are taken too.
    func testACancelBetweenTheCheckAndTheCallIsUndone() {
        // A forward opened for a request still waited for shows a toast, over the app's windows.
        _ = NSApplication.shared
        let ext = Extensions.shared
        let saved = (ext.remoteCall, ext.forwardReplies, ext.forwardRequestDone, ext.browse)
        defer {
            ext.remoteCall = saved.0
            ext.forwardReplies = saved.1
            ext.forwardRequestDone = saved.2
            ext.browse = saved.3
            ext.forwardsNow([])
        }
        // Every open asks for the browser: only one that is answered opens it.
        var browsed: [String] = []
        ext.browse = { browsed.append($0.absoluteString) }
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
        var replies: [UInt64: [[String: Any]]] = [:]
        var finished: Set<UInt64> = []
        ext.forwardReplies = { r in replies[jsonUInt64(r["id"]) ?? 0, default: []].append(r) }
        ext.forwardRequestDone = { finished.insert($0) }
        /// Asks for an open and waits until the app is done with it: the calls it made, and what
        /// the plugin was answered.
        func open(_ id: UInt64, result: [String: Any], cancel: Bool,
                  calls expected: Int) -> (calls: [String], replies: [[String: Any]]) {
            lock.lock()
            calls = []
            cancelOnCall = cancel ? id : nil
            openResult = result
            lock.unlock()
            ext.handle(["name": "ui.request", "payload": JSON.encode(
                ["kind": "forward_open", "id": id, "host": "undo-test", "port": 5173, "browse": true])])
            let deadline = Date().addingTimeInterval(5)
            while !finished.contains(id), Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            }
            XCTAssertTrue(finished.contains(id), "request \(id) never finished")
            // A forward it opened is closed off the main thread, after the request is done.
            func count() -> Int {
                lock.lock()
                defer { lock.unlock() }
                return calls.count
            }
            while count() < expected, Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            }
            RunLoop.main.run(until: Date().addingTimeInterval(0.1))
            lock.lock()
            defer { lock.unlock() }
            return (calls, replies[id] ?? [])
        }
        let opened: [String: Any] = ["id": 71, "host": "undo-test", "remote_port": 5173, "local_port": 5173,
                                     "url": "http://localhost:5173/", "new": true]
        var found = opened
        found["id"] = 72
        found["new"] = false

        // Cancelled between the check and the call: the forward it opened is closed again, and
        // the plugin, which waits no more, is not answered.
        let cancelled = open(911, result: opened, cancel: true, calls: 2)
        XCTAssertEqual(cancelled.calls, ["forward_open", "close 71"])
        XCTAssertEqual(cancelled.replies.count, 0, "a cancelled open was answered: \(cancelled.replies)")
        // One it only found open is someone else's: it stays. Not answered either.
        let foundOpen = open(912, result: found, cancel: true, calls: 1)
        XCTAssertEqual(foundOpen.calls, ["forward_open"])
        XCTAssertEqual(foundOpen.replies.count, 0, "a cancelled open was answered: \(foundOpen.replies)")
        XCTAssertEqual(browsed, [], "a cancelled open opened the browser")
        // Not cancelled: kept (and looked at again, before anything is shown for it), and answered
        // with it.
        let kept = open(913, result: opened, cancel: false, calls: 2)
        XCTAssertEqual(kept.calls, ["forward_open", "forward_get"])
        XCTAssertEqual(kept.replies.count, 1)
        XCTAssertNil(kept.replies.first?["error"])
        XCTAssertEqual(jsonUInt64((kept.replies.first?["result"] as? [String: Any])?["id"]), 71)
        XCTAssertEqual(browsed, ["http://localhost:5173/"])
    }

    /// A forward closed because the plugin that asked for it went away is closed before any
    /// open of its host starts: an open that came meanwhile, handed that same forward, would
    /// lose it to the close.
    func testAnOpenWaitsForItsHostsAbandonedCloses() {
        // The open still waited for shows a toast, over the app's windows.
        _ = NSApplication.shared
        let ext = Extensions.shared
        let saved = (ext.remoteCall, ext.forwardReplies, ext.forwardRequestDone)
        let closing = DispatchSemaphore(value: 0)
        defer {
            ext.remoteCall = saved.0
            ext.forwardReplies = saved.1
            ext.forwardRequestDone = saved.2
            ext.forwardsNow([])
        }
        let opened: [String: Any] = ["id": 91, "host": "close-test", "remote_port": 5173, "local_port": 5173,
                                     "url": "http://localhost:5173/", "new": true]
        let lock = NSLock()
        var calls: [String] = []
        var cancelOnCall: UInt64? = 931
        ext.remoteCall = { op in
            let name = jsonString(op["op"]) ?? ""
            lock.lock()
            calls.append(name)
            let cancel = cancelOnCall
            cancelOnCall = nil
            lock.unlock()
            if name == "forward_open", let cancel {
                DispatchQueue.main.sync {
                    ext.handle(["name": "ui.cancel", "payload": JSON.encode(["id": cancel])])
                }
            }
            // The close takes a while (its listeners let go).
            if name == "forward_close" { closing.wait() }
            return name == "forward_close" ? ["id": 0] : opened
        }
        var finished: Set<UInt64> = []
        ext.forwardReplies = { _ in }
        ext.forwardRequestDone = { finished.insert($0) }
        func seen() -> [String] {
            lock.lock()
            defer { lock.unlock() }
            return calls
        }
        func run(until done: () -> Bool) {
            let deadline = Date().addingTimeInterval(5)
            while !done(), Date() < deadline {
                RunLoop.main.run(until: Date().addingTimeInterval(0.02))
            }
        }
        func open(_ id: UInt64) {
            ext.handle(["name": "ui.request", "payload": JSON.encode(
                ["kind": "forward_open", "id": id, "host": "close-test", "port": 5173])])
        }

        // Cancelled once it called: what it opened is being closed.
        open(931)
        run { finished.contains(931) && seen().count == 2 }
        XCTAssertEqual(seen(), ["forward_open", "forward_close"])
        // Another open of that host meanwhile: it waits for the close.
        open(932)
        RunLoop.main.run(until: Date().addingTimeInterval(0.3))
        XCTAssertEqual(seen(), ["forward_open", "forward_close"], "an open went ahead of a close")
        closing.signal()
        run { finished.contains(932) }
        XCTAssertEqual(Array(seen().prefix(3)), ["forward_open", "forward_close", "forward_open"])
    }

    /// An open a plugin asked to browse goes to the browser only while its forward is still open
    /// on the same local port, as the app found it: one that closed meanwhile may have left the
    /// port to another program. The plugin then hears it closed, and nothing is opened.
    func testABrowsedForwardIsOpenedOnlyWhileItIsOpen() {
        _ = NSApplication.shared
        let ext = Extensions.shared
        let saved = (ext.remoteCall, ext.browse, ext.forwardReplies)
        defer {
            ext.remoteCall = saved.0
            ext.browse = saved.1
            ext.forwardReplies = saved.2
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

    /// A close waits for the forward's listeners to let go, up to two seconds: neither a plugin's
    /// close nor the close of a forward a given-up open left holds up the main thread for it.
    func testClosingAForwardDoesNotHoldUpTheMainThread() {
        let ext = Extensions.shared
        let saved = (ext.remoteCall, ext.forwardReplies, ext.forwardRequestDone)
        defer {
            ext.remoteCall = saved.0
            ext.forwardReplies = saved.1
            ext.forwardRequestDone = saved.2
            ext.forwardsNow([])
        }
        let lock = NSLock()
        var closesEnded = 0
        ext.remoteCall = { op in
            switch jsonString(op["op"]) {
            case "forward_close":
                Thread.sleep(forTimeInterval: 0.5)
                lock.lock()
                closesEnded += 1
                lock.unlock()
                return ["id": op["id"] ?? 0]
            case "forward_open":
                // Given up on while it opens.
                DispatchQueue.main.sync {
                    ext.handle(["name": "ui.cancel", "payload": JSON.encode(["id": 932])])
                }
                return ["id": 91, "host": "slow-close", "remote_port": 5173, "local_port": 5173,
                        "url": "http://localhost:5173/", "new": true]
            default:
                return ["error": "not in a test"]
            }
        }
        var finished: Set<UInt64> = []
        ext.forwardReplies = { _ in }
        ext.forwardRequestDone = { finished.insert($0) }
        func ended() -> Int {
            lock.lock()
            defer { lock.unlock() }
            return closesEnded
        }

        let start = Date()
        ext.handle(["name": "ui.request", "payload": JSON.encode(["kind": "forward_close", "id": 931, "forward": 9])])
        XCTAssertLessThan(Date().timeIntervalSince(start), 0.2, "a plugin's close held up the main thread")

        ext.handle(["name": "ui.request", "payload": JSON.encode(
            ["kind": "forward_open", "id": 932, "host": "slow-close", "port": 5173])])
        let deadline = Date().addingTimeInterval(5)
        while !finished.contains(932), Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.01))
        }
        XCTAssertTrue(finished.contains(932))
        // The given-up open is done with while the close of its forward still runs.
        XCTAssertLessThan(ended(), 2, "the close of a given-up open's forward held up the main thread")
        while ended() < 2, Date() < deadline {
            RunLoop.main.run(until: Date().addingTimeInterval(0.02))
        }
        XCTAssertEqual(ended(), 2)
    }

    /// On connecting, the app tells the daemon its forwards only from a list it read: a failed
    /// read (the core answers an error object) is not taken for "no forwards".
    func testOnlyAForwardListReadIsReported() {
        XCTAssertNil(Extensions.forwardList(["error": "not set up"]))
        XCTAssertNil(Extensions.forwardList(nil))
        XCTAssertEqual(Extensions.forwardList([] as [Any])?.count, 0)
        XCTAssertEqual(Extensions.forwardList([["id": 1]] as [Any])?.count, 1)
    }
}

private final class PaneActionView: NSView {
    override var acceptsFirstResponder: Bool { true }
    @objc func splitForTest(_ sender: Any?) {}
}
