import AppKit

/// A named set of tabs, after tty7's workspaces: every window shows one workspace; the others
/// keep their shells running in the daemon until a window switches to them.
final class Workspace {
    let id: UInt64
    var name: String
    var lastActive: Date
    /// Its tabs while no window shows it.
    var hiddenTabs: [TabLayout]
    var hiddenSelectedTab: Int

    init(id: UInt64, name: String, lastActive: Date = Date(), hiddenTabs: [TabLayout] = [],
         hiddenSelectedTab: Int = 0) {
        self.id = id
        self.name = name
        self.lastActive = lastActive
        self.hiddenTabs = hiddenTabs
        self.hiddenSelectedTab = hiddenSelectedTab
    }

    convenience init(layout l: WorkspaceLayout) {
        self.init(id: l.id, name: l.name, lastActive: Date(timeIntervalSince1970: TimeInterval(l.lastActive)),
                  hiddenTabs: l.tabs, hiddenSelectedTab: l.selectedTab)
    }
}

/// Default workspace names ("quiet-otter"), like tty7's codenames.
enum Codename {
    private static let adjectives = [
        "amber", "bold", "brisk", "calm", "clever", "cosmic", "crisp", "dapper", "eager", "fuzzy",
        "gentle", "golden", "happy", "hidden", "jolly", "keen", "lucky", "mellow", "misty", "nimble",
        "noble", "polar", "proud", "quiet", "rapid", "rusty", "shiny", "silent", "sly", "sunny",
        "swift", "tidy", "velvet", "vivid", "wild", "witty", "young", "zesty",
    ]
    private static let nouns = [
        "badger", "beacon", "comet", "cedar", "falcon", "fern", "fox", "harbor", "heron", "island",
        "koala", "lark", "lynx", "maple", "meadow", "moose", "nebula", "orca", "otter", "owl",
        "panda", "pebble", "pine", "puffin", "quartz", "raven", "river", "robin", "sparrow",
        "summit", "tiger", "tundra", "walrus", "willow", "wolf", "yak", "zebra",
    ]

    static func make(avoiding taken: Set<String>) -> String {
        for _ in 0..<64 {
            let name = "\(adjectives.randomElement()!)-\(nouns.randomElement()!)"
            if !taken.contains(name) { return name }
        }
        var n = 2
        while taken.contains("workspace-\(n)") { n += 1 }
        return "workspace-\(n)"
    }
}

/// An NSMenuItem that runs a closure.
final class BlockMenuItem: NSMenuItem {
    private let block: () -> Void

    init(_ title: String, key: String = "", mods: NSEvent.ModifierFlags = [.command], block: @escaping () -> Void) {
        self.block = block
        super.init(title: title, action: #selector(run), keyEquivalent: key)
        keyEquivalentModifierMask = mods
        target = self
    }

    required init(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    @objc private func run() { block() }
}

extension SessionManager {
    func workspace(_ id: UInt64) -> Workspace? {
        workspaces.first { $0.id == id }
    }

    var currentWorkspace: Workspace? {
        currentController.flatMap { workspace($0.workspaceID) }
    }

    @discardableResult
    func makeWorkspace(name: String? = nil) -> Workspace {
        let id = (workspaces.map(\.id).max() ?? 0) + 1
        let ws = Workspace(id: id, name: name ?? Codename.make(avoiding: Set(workspaces.map(\.name))))
        workspaces.append(ws)
        return ws
    }

    /// The controllers of `c`'s window (its tab group), in tab order.
    func group(of c: TerminalWindowController) -> [TerminalWindowController] {
        guard let w = c.window else { return [c] }
        return (w.tabGroup?.windows ?? [w])
            .compactMap { $0.windowController as? TerminalWindowController }
            .filter { !$0.isClosed }
    }

    /// A controller of the window showing `ws`, preferring its selected tab.
    func controllerShowing(_ ws: Workspace) -> TerminalWindowController? {
        let shown = liveControllers.filter { $0.workspaceID == ws.id }
        return shown.first { $0.window?.tabGroup?.selectedWindow === $0.window } ?? shown.first
    }

    func isShown(_ ws: Workspace) -> Bool { controllerShowing(ws) != nil }

    /// Most recently used first.
    var workspacesByRecency: [Workspace] {
        workspaces.sorted { $0.lastActive > $1.lastActive }
    }

    /// Makes the window of `c` show `target` in place: its tabs replace the window's tabs,
    /// whose panes keep running in the (now hidden) previous workspace. When another window
    /// shows `target` already, that window comes to the front instead.
    func switchWorkspace(in c: TerminalWindowController, to target: Workspace) {
        // The quick terminal shows no workspace: switch the last regular window.
        if c.isQuick {
            if let r = currentRegularController {
                switchWorkspace(in: r, to: target)
            } else {
                openWorkspaceInNewWindow(target)
            }
            return
        }
        guard target.id != c.workspaceID else { return }
        if let other = controllerShowing(target) {
            (other.window?.tabGroup?.selectedWindow ?? other.window)?.makeKeyAndOrderFront(nil)
            return
        }
        guard let hostWindow = c.window else { return }
        let old = group(of: c)
        let selectedWindow = hostWindow.tabGroup?.selectedWindow ?? hostWindow
        if let current = workspace(c.workspaceID) {
            let kept = old.filter { !$0.content.isEmpty }
            current.hiddenTabs = kept.map { $0.content.tabLayout(title: $0.titleOverride) }
            current.hiddenSelectedTab = kept.firstIndex { $0.window === selectedWindow } ?? 0
        }

        // The new tabs join this window's tab group (so position, size and full screen stay),
        // then the old ones leave it.
        var created: [TerminalWindowController] = []
        var anchor = selectedWindow
        func add(_ nc: TerminalWindowController) {
            nc.workspaceID = target.id
            guard let w = nc.window else { return }
            w.setFrame(selectedWindow.frame, display: false)
            anchor.addTabbedWindow(w, ordered: .above)
            anchor = w
            created.append(nc)
        }
        for tab in target.hiddenTabs {
            add(makeController(root: SplitNode(layout: tab.root), focused: tab.focused, zoomed: tab.zoomed,
                               title: tab.title, frame: nil))
        }
        if created.isEmpty {
            let size = gridSize(forPoints: c.content.bounds.size)
            if let pane = createPane(cols: size.cols, rows: size.rows, inheritFrom: nil) {
                add(makeController(root: .leaf(pane), focused: pane, zoomed: nil, title: nil, frame: nil))
            }
        }
        guard !created.isEmpty else { return }
        let selected = created[min(max(0, target.hiddenSelectedTab), created.count - 1)]
        target.hiddenTabs = []
        target.hiddenSelectedTab = 0
        target.lastActive = Date()
        selected.window?.makeKeyAndOrderFront(nil)
        for o in old {
            o.closingWithoutConfirmation = true
            o.window?.close()
        }
        refreshSidebars()
        focusChanged()
        scheduleLayoutSave()
    }

    /// Shows a hidden workspace in a new window (restore with no window left).
    func openWorkspaceInNewWindow(_ ws: Workspace, frame: NSRect? = nil) {
        var previous: TerminalWindowController?
        var created: [TerminalWindowController] = []
        for tab in ws.hiddenTabs {
            let c = makeController(root: SplitNode(layout: tab.root), focused: tab.focused, zoomed: tab.zoomed,
                                   title: tab.title, frame: frame)
            c.workspaceID = ws.id
            if let prev = previous { attachAsTab(c, to: prev) } else { showAsNewWindow(c, frame: frame) }
            previous = c
            created.append(c)
        }
        guard !created.isEmpty else { return }
        created[min(max(0, ws.hiddenSelectedTab), created.count - 1)].window?.makeKeyAndOrderFront(nil)
        ws.hiddenTabs = []
        ws.lastActive = Date()
        scheduleLayoutSave()
    }

    /// ⌘⇧N: a new workspace with a fresh shell, in the front window.
    func newWorkspace() {
        let ws = makeWorkspace()
        if let c = currentController {
            switchWorkspace(in: c, to: ws)
        } else {
            newWindow(workspace: ws)
        }
        currentController?.content.focusedView?.showToast("Workspace \(ws.name)")
    }

    func renameWorkspace(_ ws: Workspace) {
        let alert = NSAlert()
        alert.messageText = "Rename Workspace"
        let field = NSTextField(string: ws.name)
        field.frame = NSRect(x: 0, y: 0, width: 260, height: 24)
        alert.accessoryView = field
        alert.addButton(withTitle: "Rename")
        alert.addButton(withTitle: "Cancel")
        alert.window.initialFirstResponder = field
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        let name = field.stringValue.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !name.isEmpty else { return }
        ws.name = name
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// Ends the workspace's shells and forgets it. Its window moves on to the most recent
    /// hidden workspace, or closes when there is none.
    func closeWorkspace(_ ws: Workspace) {
        let shownIn = controllerShowing(ws)
        let tabs = shownIn.map { group(of: $0).count } ?? ws.hiddenTabs.count
        let alert = NSAlert()
        alert.messageText = "Close workspace “\(ws.name)”?"
        alert.informativeText = "Its \(tabs) \(tabs == 1 ? "tab" : "tabs") and the programs running in them are closed."
        alert.alertStyle = .warning
        alert.addButton(withTitle: "Close Workspace")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }

        let ids = shownIn.map { group(of: $0).flatMap { $0.content.paneIds } }
            ?? ws.hiddenTabs.flatMap { $0.root.panes }
        for id in ids { sendClosePane(id) }
        workspaces.removeAll { $0 === ws }
        if let c = shownIn {
            let old = group(of: c)
            for o in old { o.workspaceID = 0 }
            if let next = workspacesByRecency.first(where: { !isShown($0) }) {
                switchWorkspace(in: c, to: next)
            } else {
                for o in old {
                    o.closingWithoutConfirmation = true
                    o.window?.close()
                }
            }
        }
        refreshSidebars()
        scheduleLayoutSave()
    }

    /// ⌘⇧O: pick a workspace by name.
    func showWorkspaceSwitcher() {
        let current = currentController?.workspaceID
        var items: [CommandPalette.Item] = workspacesByRecency.map { ws in
            let tabs = isShown(ws) ? (controllerShowing(ws).map { group(of: $0).count } ?? 0) : ws.hiddenTabs.count
            var detail = "\(tabs) \(tabs == 1 ? "tab" : "tabs")"
            if ws.id == current {
                detail += " · this window"
            } else if isShown(ws) {
                detail += " · open"
            } else {
                detail += " · \(Self.relative(ws.lastActive))"
            }
            let rename = {
                SessionManager.shared.renameWorkspace(ws)
                SessionManager.shared.showWorkspaceSwitcher()
            }
            return CommandPalette.Item(title: ws.name, detail: detail, rename: rename) {
                if let c = SessionManager.shared.currentController {
                    SessionManager.shared.switchWorkspace(in: c, to: ws)
                } else {
                    SessionManager.shared.openWorkspaceInNewWindow(ws)
                }
            }
        }
        items.append(CommandPalette.Item(title: "New Workspace", detail: "", shortcut: "⇧⌘N") {
            SessionManager.shared.newWorkspace()
        })
        CommandPalette.shared.show(items: items, over: currentController?.window,
                                   placeholder: "Switch to workspace…", footer: "↩ switch   ⌘R rename")
    }

    private static func relative(_ date: Date) -> String {
        let f = RelativeDateTimeFormatter()
        f.unitsStyle = .abbreviated
        return f.localizedString(for: date, relativeTo: Date())
    }

    /// The workspace list plus actions, for the sidebar header and Window > Workspaces.
    func fillWorkspaceMenu(_ menu: NSMenu) {
        menu.removeAllItems()
        let current = currentController?.workspaceID
        for (i, ws) in workspacesByRecency.enumerated() {
            let item = BlockMenuItem(ws.name, key: i < 9 ? "\(i + 1)" : "", mods: [.command, .control]) {
                if let c = SessionManager.shared.currentController {
                    SessionManager.shared.switchWorkspace(in: c, to: ws)
                } else {
                    SessionManager.shared.openWorkspaceInNewWindow(ws)
                }
            }
            item.state = ws.id == current ? .on : .off
            menu.addItem(item)
        }
        menu.addItem(.separator())
        menu.addItem(BlockMenuItem("New Workspace", key: "n", mods: [.command, .shift]) {
            SessionManager.shared.newWorkspace()
        })
        menu.addItem(BlockMenuItem("Switch Workspace…", key: "o", mods: [.command, .shift]) {
            SessionManager.shared.showWorkspaceSwitcher()
        })
        if let ws = currentWorkspace {
            menu.addItem(BlockMenuItem("Rename Workspace…") { SessionManager.shared.renameWorkspace(ws) })
            menu.addItem(BlockMenuItem("Close Workspace…") { SessionManager.shared.closeWorkspace(ws) })
        }
    }

    // MARK: Layout

    /// One workspace per window: the group's selected tab decides; a window that has none, an
    /// unknown one, or one another window shows already (a tab dragged out into a new window)
    /// gets a new workspace. The larger window keeps a shared workspace.
    func normalizeWorkspaces(groups: [[TerminalWindowController]], selected: [TerminalWindowController]) {
        var owner: [UInt64: Int] = [:]
        let bySize = groups.indices.sorted { groups[$0].count > groups[$1].count }
        var assigned = [UInt64](repeating: 0, count: groups.count)
        for gi in bySize {
            var id = selected[gi].workspaceID
            if id == 0 || workspace(id) == nil || owner[id] != nil {
                id = makeWorkspace().id
            }
            owner[id] = gi
            assigned[gi] = id
        }
        for (gi, g) in groups.enumerated() {
            for c in g { c.workspaceID = assigned[gi] }
        }
    }

    func workspaceLayouts() -> [WorkspaceLayout] {
        let shown = Set(liveControllers.map(\.workspaceID))
        // Hidden workspaces lose panes that ended meanwhile, and go when none is left.
        for ws in workspaces where !shown.contains(ws.id) {
            ws.hiddenTabs = retainTabs(ws.hiddenTabs) { panes[$0] != nil }
            ws.hiddenSelectedTab = min(ws.hiddenSelectedTab, max(0, ws.hiddenTabs.count - 1))
        }
        workspaces.removeAll { !shown.contains($0.id) && $0.hiddenTabs.isEmpty }
        return workspaces.map { ws in
            let hidden = !shown.contains(ws.id)
            return WorkspaceLayout(id: ws.id, name: ws.name, lastActive: UInt64(ws.lastActive.timeIntervalSince1970),
                                   tabs: hidden ? ws.hiddenTabs : [], selectedTab: hidden ? ws.hiddenSelectedTab : 0)
        }
    }
}
