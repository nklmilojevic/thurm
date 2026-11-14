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
    /// The names of the app's own actions: menu items with a `[keybindings]` name.
    private(set) var appActions: Set<String> = []
    /// The question a plugin asked that shows now: how to answer it (nothing, when it is
    /// taken down for another) and how to take it down.
    private var shown: (id: UInt64, cancel: () -> Void, dismiss: () -> Void)?
    private var refreshQueued = false

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
        registerAppActions()
        refresh()
    }

    /// Reads the actions and key bindings again, and applies them to the menus.
    func refresh() {
        call("actions.list") { value, error in
            guard error == nil, let list = value as? [String: Any] else {
                if self.available {
                    self.available = false
                    SessionManager.shared.applyKeys()
                }
                return
            }
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
            let changed = !self.available || actions != self.actions || bindings != self.bindings
            self.load(actions: actions, bindings: bindings)
            if changed { SessionManager.shared.applyKeys() }
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
    private func registerAppActions() {
        var seen = Set<String>()
        var list: [[String: Any]] = []
        func walk(_ menu: NSMenu, top: String) {
            for item in menu.items {
                if let sub = item.submenu {
                    walk(sub, top: top.isEmpty ? item.title : top)
                    continue
                }
                guard let name = MainMenu.actionName(item.identifier), seen.insert(name).inserted
                else { continue }
                list.append(["name": name, "title": item.title, "category": top])
            }
        }
        if let main = NSApp.mainMenu {
            for top in main.items {
                if let sub = top.submenu { walk(sub, top: top.title.isEmpty ? "Thurm" : top.title) }
            }
        }
        appActions = seen
        guard !list.isEmpty else { return }
        call("actions.register", ["actions": list]) { _, error in
            if let error { tlog("could not offer the app's actions: \(error)") }
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
    func run(_ name: String, args: String? = nil, pane: UInt64?, completion: @escaping (String?) -> Void) {
        var params: [String: Any] = ["name": name, "pane": pane.map { $0 as Any } ?? NSNull()]
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
        NSApp.sendAction(action, to: item.target, from: item)
        answer(nil)
    }

    /// Shows what a plugin asks for: a toast, a list to pick from, a line to type, or a yes/no.
    private func show(_ req: [String: Any]) {
        guard let id = jsonUInt64(req["id"]) else { return }
        let kind = jsonString(req["kind"]) ?? ""
        let pane = jsonUInt64(req["pane"]).map { PaneKey(localHost, $0) }
        if kind == "toast" {
            let view = pane.flatMap { SessionManager.shared.view(for: $0) }
                ?? SessionManager.shared.currentController?.content.focusedView
            view?.showToast(jsonString(req["text"]) ?? "", duration: 4)
            return
        }
        // One question at a time: the one showing is answered with nothing, and goes.
        if let s = shown {
            shown = nil
            s.cancel()
            s.dismiss()
        }
        var answered = false
        let reply: (Any) -> Void = { [weak self] value in
            guard !answered else { return }
            answered = true
            if self?.shown?.id == id { self?.shown = nil }
            self?.call("ui.reply", ["id": id, "result": value])
        }
        NSApp.activate()
        // Activation is only a request: a plugin waiting on an app in the background bounces
        // its icon instead.
        if !NSApp.isActive { NSApp.requestUserAttention(.informationalRequest) }
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
            shown = (id, { reply(NSNull()) }, { CommandPalette.shared.close() })
            CommandPalette.shared.show(items: items, over: window, placeholder: placeholder,
                                       footer: title.isEmpty || placeholder == title ? nil : title,
                                       onCancel: { reply(NSNull()) })
        case "input", "confirm":
            let alert = NSAlert()
            alert.messageText = title
            // Before it shows: a modal alert answers before `present` returns.
            shown = (id, { reply(NSNull()) }, { [weak alert] in
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
