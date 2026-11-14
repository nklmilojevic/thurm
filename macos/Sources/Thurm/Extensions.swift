import AppKit

/// The extension API of this Mac's daemon: the actions and key bindings that plugins, init.lua
/// and `[[commands]]` provide (the Commands menu and the palette show them), the app's own
/// actions offered to them, and what plugins ask the user.
final class Extensions {
    static let shared = Extensions()

    struct Action: Equatable {
        let name: String
        let title: String
        let category: String
        let source: String
        let hidden: Bool
    }

    struct Binding: Equatable {
        let key: String
        let action: String
        /// What the action gets on this key, as JSON; `nil` for none.
        var args: String? = nil
    }

    /// This Mac's daemon answers the extension API (an older one does not: the Commands menu
    /// then comes from `[[commands]]` in the config file).
    private(set) var available = false
    private(set) var actions: [Action] = []
    private(set) var bindings: [Binding] = []
    /// The names of the app's own actions: menu items with a `[keybindings]` name, which the
    /// daemon lists as the app's (a name the config or a plugin has is theirs).
    private(set) var appActions: Set<String> = []
    /// Every name the app could offer: those not offered because they were taken are offered
    /// once they are free again.
    private var menuActions: Set<String> = []
    /// An offer of the app's actions is under way (or being retried).
    private var registering = false
    /// The question a plugin asked that shows now: how to answer it (nothing, when it is
    /// taken down for another) and how to take it down; `pick` for the palette, which goes
    /// with the app's focus.
    private var shown: (id: UInt64, pick: Bool, cancel: () -> Void, dismiss: () -> Void)?
    /// Bumped with every connection to the daemon: an answer to a question of an earlier one
    /// is not sent (its ids may name another plugin's question now).
    private var connection = 0
    private var resignObserver: NSObjectProtocol?
    private var refreshQueued = false
    /// Reads of the actions that failed in a row, for a bounded retry.
    private var refreshFailures = 0
    /// App actions that only a person at the Mac may take: never offered to plugins or the CLI.
    static let personalActions: Set<String> = ["secure_keyboard_entry"]
    /// A question that came while the app was in the background: it shows once the user
    /// brings the app forward (the Dock icon bounces for it), not before.
    private var pending: [String: Any]?
    private var activeObserver: NSObjectProtocol?

    // MARK: Calling the daemon

    /// Calls `method` with `params`; `completion` gets the result, or the error.
    func call(_ method: String, _ params: Any? = nil, timeout: TimeInterval = 10,
              completion: ((Any?, String?) -> Void)? = nil) {
        let text = params.map { JSON.encode($0) } ?? ""
        Core.shared.requestAsync(object: ["Ext": ["method": method, "params": text]], timeout: timeout) { resp in
            let dict = resp as? [String: Any]
            if let e = dict.flatMap({ jsonString($0["error"]) }) {
                completion?(nil, e)
            } else if let result = dict.flatMap({ jsonString($0["Text"]) }) {
                completion?(JSON.decode(result), nil)
            } else {
                completion?(nil, resp == nil ? "Thurm's daemon is not connected" : "unexpected answer")
            }
        }
    }

    /// After (re)connecting to this Mac's daemon: offer it the app's actions, read its own.
    func connected() {
        invalidateQuestions()
        registering = false
        registerAppActions()
        refresh()
    }

    /// The connection to this Mac's daemon is gone: its questions go, unanswered, and so do
    /// its actions and keys: the menus fall back on the file's [[commands]] until a daemon says
    /// what it has (an older one says nothing).
    func disconnected() {
        invalidateQuestions()
        refreshFailures = 0
        let had = available || !actions.isEmpty || !bindings.isEmpty
        available = false
        actions = []
        bindings = []
        appActions = []
        if had { SessionManager.shared.applyKeys() }
    }

    private func invalidateQuestions() {
        connection += 1
        pending = nil
        if let s = shown {
            shown = nil
            s.dismiss()
        }
    }

    /// Reads the actions and key bindings again, and applies them to the menus.
    func refresh() {
        let asked = connection
        call("actions.list") { value, error in
            // An answer from an earlier connection says nothing of the daemon there is now.
            guard self.connection == asked else { return }
            guard error == nil, let list = value as? [String: Any] else {
                // A daemon without the API: the Commands menu comes from the file. Any other
                // failure (a timeout, a reconnect) keeps what the menus have, and tries again.
                let unsupported = error.map { $0.contains("no actions or plugins") || $0.contains("unknown") } ?? false
                if unsupported {
                    self.refreshFailures = 0
                    if self.available {
                        self.available = false
                        SessionManager.shared.applyKeys()
                    }
                } else if self.refreshFailures < 5 {
                    self.refreshFailures += 1
                    DispatchQueue.main.asyncAfter(deadline: .now() + Double(self.refreshFailures)) { self.refresh() }
                }
                return
            }
            self.refreshFailures = 0
            let actions = (list["actions"] as? [[String: Any]] ?? []).compactMap { a -> Action? in
                guard let name = jsonString(a["name"]) else { return nil }
                return Action(name: name, title: jsonString(a["title"]) ?? "",
                              category: jsonString(a["category"]) ?? "",
                              source: jsonString(a["source"]) ?? "", hidden: jsonBool(a["hidden"]) ?? false)
            }
            let bindings = (list["bindings"] as? [[String: Any]] ?? []).compactMap { b -> Binding? in
                guard let key = jsonString(b["key"]), let action = jsonString(b["action"]) else { return nil }
                let args = b["args"].flatMap { $0 is NSNull ? nil : JSON.encode($0) }
                return Binding(key: key, action: action, args: args)
            }
            // The app's actions are those the daemon lists as the app's: a config reload or a
            // plugin can take a name over, and where a key goes follows.
            let owned = Set(actions.filter { $0.source == "Thurm.app" }.map(\.name))
            let changed = !self.available || actions != self.actions || bindings != self.bindings
                || owned != self.appActions
            self.load(actions: actions, bindings: bindings, appActions: owned)
            if changed { SessionManager.shared.applyKeys() }
            // Names of the app's that were taken and are free again (a plugin that had one
            // left): offer them now.
            let listed = Set(actions.map(\.name))
            if !self.registering, !self.menuActions.subtracting(listed).isEmpty {
                self.registerAppActions()
            }
        }
    }

    /// Takes the daemon's actions and bindings (and, for tests, the app's action names).
    func load(actions: [Action], bindings: [Binding], appActions: Set<String>? = nil) {
        available = true
        self.actions = actions
        self.bindings = bindings
        if let appActions { self.appActions = appActions }
    }

    /// Offers the main menu's named items to the daemon, so plugins, scripts and `thurm action
    /// run` can use them (`split_down`, `new_tab`...).
    private func registerAppActions(attempt: Int = 0) {
        var seen = Set<String>()
        var list: [[String: Any]] = []
        func walk(_ menu: NSMenu, top: String) {
            for item in menu.items {
                if let sub = item.submenu {
                    walk(sub, top: top.isEmpty ? item.title : top)
                    continue
                }
                guard let name = MainMenu.actionName(item.identifier),
                      !Extensions.personalActions.contains(name), seen.insert(name).inserted
                else { continue }
                list.append(["name": name, "title": item.title, "category": top])
            }
        }
        if let main = NSApp.mainMenu {
            for top in main.items {
                if let sub = top.submenu { walk(sub, top: top.title.isEmpty ? "Thurm" : top.title) }
            }
        }
        menuActions = seen
        guard !list.isEmpty else {
            appActions = []
            return
        }
        registering = true
        // A name the config or a plugin uses already stays theirs: the others are offered
        // (the daemon refuses a whole list with a taken name in it).
        let asked = connection
        call("actions.list") { value, error in
            guard self.connection == asked else { return }
            // Without the list, which names are free is unknown: try again a little later.
            guard error == nil, value is [String: Any] else {
                if attempt < 5 {
                    DispatchQueue.main.asyncAfter(deadline: .now() + Double(attempt + 1)) {
                        guard self.connection == asked else { return }
                        self.registerAppActions(attempt: attempt + 1)
                    }
                } else {
                    self.registering = false
                    tlog("could not offer the app's actions: \(error ?? "no list")")
                }
                return
            }
            let taken = Set(((value as? [String: Any])?["actions"] as? [[String: Any]] ?? [])
                .compactMap { a -> String? in
                    jsonString(a["source"]) == "Thurm.app" ? nil : jsonString(a["name"])
                })
            let free = list.filter { !taken.contains(jsonString($0["name"]) ?? "") }
            for name in seen.intersection(taken).sorted() {
                tlog("app action \(name) not offered: the name is taken")
            }
            let offered = seen.subtracting(taken)
            self.call("actions.register", ["actions": free]) { _, error in
                guard self.connection == asked else { return }
                // The app's names are those the daemon took: until then (or if it refused
                // them) a key bound to one is not routed to the app.
                let now: Set<String> = error == nil ? offered : []
                if now != self.appActions {
                    self.appActions = now
                    // Which names are the app's decides where a key goes: build the menus again.
                    if self.available { SessionManager.shared.applyKeys() }
                }
                guard let error else {
                    self.registering = false
                    return
                }
                if attempt < 5 {
                    DispatchQueue.main.asyncAfter(deadline: .now() + Double(attempt + 1)) {
                        guard self.connection == asked else { return }
                        self.registerAppActions(attempt: attempt + 1)
                    }
                } else {
                    self.registering = false
                    tlog("could not offer the app's actions: \(error)")
                }
            }
        }
    }

    /// What the menus are built from: `[keybindings]` with the daemon's bindings of the app's
    /// own actions added, and the Commands menu's entries with their keys.
    func menuInput(keybindings: [String: String], commands: [UserCommand]) -> ([String: String], [UserCommand]) {
        guard available else { return (keybindings, commands) }
        var keys = keybindings
        // An action's visible entry takes its first key without arguments; every other key
        // (another chord, or one with arguments) gets a hidden entry of its own.
        var shownKey: [String: String] = [:]
        var extra: [UserCommand] = []
        let known = Dictionary(actions.map { ($0.name, $0) }, uniquingKeysWith: { a, _ in a })
        for b in bindings {
            if appActions.contains(b.action) {
                // The app's actions take no arguments: a key bound with some is not taken
                // as a plain one.
                if b.args != nil {
                    tlog("keys: \(b.key) binds \(b.action) with arguments, which app actions don't take; ignored")
                    continue
                }
                keys[b.key] = b.action
            } else if b.args == nil, shownKey[b.action] == nil, known[b.action]?.hidden == false {
                shownKey[b.action] = b.key
            } else if known[b.action] != nil {
                extra.append(UserCommand(name: b.action, key: b.key, title: known[b.action]?.title ?? "",
                                         hidden: true, args: b.args))
            }
        }
        let entries = actions
            .filter { !appActions.contains($0.name) && !$0.hidden }
            .map { UserCommand(name: $0.name, key: shownKey[$0.name] ?? "", title: $0.title,
                               category: $0.category) }
        return (keys, entries + extra)
    }

    /// Runs action `name` for the focused pane, with `args` (JSON) when a key gives some;
    /// `completion` gets the error, if any.
    /// `noPane`: the focus is on no pane of this Mac's daemon (another host's), so the action
    /// gets none rather than the daemon's own focused one.
    func run(_ name: String, args: String? = nil, pane: UInt64?, noPane: Bool = false,
             completion: @escaping (String?) -> Void) {
        var params: [String: Any] = ["name": name, "pane": pane.map { $0 as Any } ?? NSNull()]
        if noPane { params["no_pane"] = true }
        if let args, let value = JSON.decode(args) { params["args"] = value }
        // An action may ask the user, or run a while: the answer can take long.
        call("actions.run", params, timeout: 30 * 60) { _, e in
            completion(e)
        }
    }

    // MARK: What the daemon says

    /// An `Ext` event from this Mac's daemon: `{name, payload}` with the payload as JSON text.
    func handle(_ event: [String: Any]) {
        guard let name = jsonString(event["name"]) else { return }
        let payload = jsonString(event["payload"]).flatMap { JSON.decode($0) } as? [String: Any] ?? [:]
        switch name {
        case "actions.changed":
            // They come in bursts, one per action a plugin adds.
            guard !refreshQueued else { return }
            refreshQueued = true
            DispatchQueue.main.asyncAfter(deadline: .now() + 0.05) {
                self.refreshQueued = false
                self.refresh()
            }
        case "action.invoke":
            invoke(payload)
        case "ui.request":
            show(payload)
        case "ui.cancel":
            if let id = jsonUInt64(payload["id"]), let s = shown, s.id == id {
                shown = nil
                s.dismiss()
            }
            if let id = jsonUInt64(payload["id"]), let p = pending, jsonUInt64(p["id"]) == id {
                pending = nil
            }
        default:
            break
        }
    }

    /// Carries out one of the app's actions for a plugin: its menu item, on the pane it names.
    private func invoke(_ payload: [String: Any]) {
        guard let id = jsonUInt64(payload["id"]) else { return }
        let name = jsonString(payload["name"]) ?? ""
        func answer(_ error: String?) {
            var reply: [String: Any] = ["id": id, "result": NSNull()]
            if let error { reply["error"] = error }
            call("actions.result", reply)
        }
        guard !Extensions.personalActions.contains(name) else {
            answer("\"\(name)\" is only for the person at this Mac")
            return
        }
        guard let item = MainMenu.item(forAction: name), let action = item.action else {
            answer("the app has no action \"\(name)\"")
            return
        }
        if let ctx = payload["ctx"] as? [String: Any], let pane = jsonUInt64(ctx["pane"]) {
            // Never on another pane than the one named: not this app's, or gone.
            let key = PaneKey(localHost, pane)
            guard SessionManager.shared.view(for: key) != nil else {
                answer("pane \(pane) is not in this app's windows")
                return
            }
            SessionManager.shared.focusPane(key, activate: false)
        }
        item.menu?.update()
        guard item.isEnabled else {
            answer("\"\(name)\" is not available now (is a Thurm window in front?)")
            return
        }
        guard NSApp.sendAction(action, to: item.target, from: item) else {
            answer("nothing in the app's front window takes \"\(name)\" now")
            return
        }
        answer(nil)
    }

    /// Shows what a plugin asks for: a toast, a list to pick from, a line to type, or a yes/no.
    private func show(_ req: [String: Any]) {
        guard let id = jsonUInt64(req["id"]) else { return }
        let kind = jsonString(req["kind"]) ?? ""
        let pane = jsonUInt64(req["pane"]).map { PaneKey(localHost, $0) }
        if kind == "toast" {
            // Over the pane it names, if the app shows it; over the focused one when it names
            // none. Never over another pane than the one named.
            let view: TerminalView?
            if let pane {
                view = SessionManager.shared.view(for: pane)
                if view == nil { tlog("toast for pane \(pane.id), which this app doesn't show") }
            } else {
                view = SessionManager.shared.currentController?.content.focusedView
            }
            view?.showToast(jsonString(req["text"]) ?? "", duration: 4)
            return
        }
        // The app in the background: the question waits until the user comes, rather than
        // taking the focus from what they are doing. One waits at a time.
        if !NSApp.isActive {
            if let old = pending, let oldId = jsonUInt64(old["id"]) {
                call("ui.reply", ["id": oldId, "result": NSNull()])
            }
            pending = req
            NSApp.requestUserAttention(.informationalRequest)
            if activeObserver == nil {
                activeObserver = NotificationCenter.default.addObserver(
                    forName: NSApplication.didBecomeActiveNotification, object: nil, queue: .main
                ) { [weak self] _ in
                    guard let self, let req = self.pending else { return }
                    self.pending = nil
                    self.show(req)
                }
            }
            return
        }
        // One question at a time: the one showing is answered with nothing, and goes.
        if let s = shown {
            shown = nil
            s.cancel()
            s.dismiss()
        }
        var answered = false
        let asked = connection
        let reply: (Any) -> Void = { [weak self] value in
            guard !answered else { return }
            answered = true
            if self?.shown?.id == id { self?.shown = nil }
            // Not to a daemon that came since: the id may be another plugin's question there.
            guard self?.connection == asked else { return }
            self?.call("ui.reply", ["id": id, "result": value])
        }
        let window = NSApp.keyWindow ?? SessionManager.shared.currentController?.window
        let title = jsonString(req["title"]) ?? ""
        switch kind {
        case "pick":
            let items = (req["items"] as? [[String: Any]] ?? []).enumerated().map { index, item in
                CommandPalette.Item(title: jsonString(item["title"]) ?? "", detail: jsonString(item["detail"]) ?? "") {
                    reply(["index": index, "id": item["id"] ?? NSNull()])
                }
            }
            let placeholder = jsonString(req["placeholder"]).flatMap { $0.isEmpty ? nil : $0 }
                ?? (title.isEmpty ? "Pick one…" : title)
            shown = (id, true, { reply(NSNull()) }, { CommandPalette.shared.close() })
            // The palette hides when the app loses the focus: the question is answered with
            // nothing then, rather than left waiting behind it.
            if resignObserver == nil {
                resignObserver = NotificationCenter.default.addObserver(
                    forName: NSApplication.didResignActiveNotification, object: nil, queue: .main
                ) { [weak self] _ in
                    guard let self, let s = self.shown, s.pick else { return }
                    self.shown = nil
                    s.cancel()
                    s.dismiss()
                }
            }
            CommandPalette.shared.show(items: items, over: window, placeholder: placeholder,
                                       footer: title.isEmpty || placeholder == title ? nil : title,
                                       onCancel: { reply(NSNull()) })
        case "input", "confirm":
            let alert = NSAlert()
            alert.messageText = title
            // Before it shows: a modal alert answers before `present` returns.
            shown = (id, false, { reply(NSNull()) }, { [weak alert] in
                guard let alert else { return }
                if let parent = alert.window.sheetParent {
                    parent.endSheet(alert.window, returnCode: .cancel)
                } else {
                    NSApp.abortModal()
                }
            })
            if kind == "input" {
                let field = NSTextField(string: jsonString(req["value"]) ?? "")
                field.placeholderString = jsonString(req["placeholder"])
                field.frame = NSRect(x: 0, y: 0, width: 300, height: 24)
                alert.accessoryView = field
                alert.addButton(withTitle: "OK")
                alert.addButton(withTitle: "Cancel")
                alert.window.initialFirstResponder = field
                present(alert, over: window) { response in
                    reply(response == .alertFirstButtonReturn ? field.stringValue : NSNull())
                }
            } else {
                alert.informativeText = jsonString(req["body"]) ?? ""
                alert.addButton(withTitle: jsonString(req["ok"]).flatMap { $0.isEmpty ? nil : $0 } ?? "OK")
                alert.addButton(withTitle: jsonString(req["cancel"]).flatMap { $0.isEmpty ? nil : $0 } ?? "Cancel")
                present(alert, over: window) { response in
                    reply(response == .alertFirstButtonReturn)
                }
            }
        default:
            reply(NSNull())
        }
    }

    /// Shows `alert` as a sheet on `window`, or on its own without one.
    private func present(_ alert: NSAlert, over window: NSWindow?, done: @escaping (NSApplication.ModalResponse) -> Void) {
        if let window, window.attachedSheet == nil {
            alert.beginSheetModal(for: window, completionHandler: done)
        } else {
            done(alert.runModal())
        }
    }
}
