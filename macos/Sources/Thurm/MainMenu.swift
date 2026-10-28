import AppKit

/// Programmatic main menu (no nib). Actions are sent to the first responder, so they reach the
/// focused TerminalView, its TerminalWindowController or the AppDelegate.
enum MainMenu {
    // Function-key characters used as key equivalents.
    private static let upArrow = "\u{F700}"
    private static let downArrow = "\u{F701}"
    private static let leftArrow = "\u{F702}"
    private static let rightArrow = "\u{F703}"

    /// The Shell menu's Commands submenu: the config's `[[commands]]`, refilled on every load.
    private static let commandsMenu = NSMenu(title: "Commands")
    private static var commandsItem: NSMenuItem?
    /// Built-in items whose key `applyKeys` changed, with the key they had.
    private static var changed: [(item: NSMenuItem, key: String, mods: NSEvent.ModifierFlags)] = []
    /// Hidden items `applyKeys` added to keep a built-in key working next to a bound one.
    private static var extraKeys: [NSMenuItem] = []

    /// Applies `[keybindings]` and `[[commands]]` keys to the menus, from the built-in keys up, and
    /// returns warnings for what it cannot use. A binding gives its combination to one action
    /// (shown on its item; the key the item had keeps working) or, bound to "none", to nothing.
    /// A command's key comes before both: the first item with a key equivalent gets it, so the
    /// built-in one gives it up (and has it back once no command claims it).
    @discardableResult
    static func applyKeys(_ bindings: [String: String], commands: [UserCommand],
                          in main: NSMenu? = NSApp.mainMenu) -> [String] {
        for c in changed {
            c.item.keyEquivalent = c.key
            c.item.keyEquivalentModifierMask = c.mods
        }
        changed = []
        for extra in extraKeys { extra.menu?.removeItem(extra) }
        extraKeys = []
        var warnings: [String] = []
        if let main {
            for (spec, action) in bindings.sorted(by: { $0.key < $1.key }) {
                guard let (key, mods) = keyEquivalent(spec) else {
                    warnings.append("keybindings: cannot read \"\(spec)\"")
                    continue
                }
                let name = action.trimmingCharacters(in: .whitespaces)
                var target: NSMenuItem?
                if name != "none" {
                    // A typo, or a blank, leaves the combination's default alone.
                    guard let found = actionItem(name, in: main) else {
                        warnings.append("keybindings: unknown action \"\(name)\"")
                        continue
                    }
                    target = found
                }
                displace(chord(key, mods), in: main)
                if let target { bind(target, key, mods) }
            }
        }
        commandsMenu.removeAllItems()
        for command in commands {
            let (key, mods) = keyEquivalent(command.key) ?? ("", [])
            if !key.isEmpty, let main { displace(chord(key, mods), in: main) }
            let entry = item(command.name, #selector(AppDelegate.runUserCommand(_:)), key, mods)
            entry.representedObject = command.name
            commandsMenu.addItem(entry)
        }
        commandsItem?.isHidden = commands.isEmpty
        return warnings
    }

    /// The identifier of a `[keybindings]` action's item. Namespaced: AppKit names an item
    /// without one after its selector ("checkForUpdates:").
    static func actionIdentifier(_ name: String) -> NSUserInterfaceItemIdentifier {
        NSUserInterfaceItemIdentifier("thurm.action." + name)
    }

    /// The item of a `[keybindings]` action name, a visible one before a hidden alias.
    private static func actionItem(_ name: String, in menu: NSMenu) -> NSMenuItem? {
        actionItem(actionIdentifier(name), in: menu)
    }

    private static func actionItem(_ id: NSUserInterfaceItemIdentifier, in menu: NSMenu) -> NSMenuItem? {
        var hidden: NSMenuItem?
        for item in menu.items {
            if let sub = item.submenu, sub !== commandsMenu, let found = actionItem(id, in: sub) {
                if !found.isHidden { return found }
                hidden = hidden ?? found
            }
            if item.identifier == id {
                if !item.isHidden { return item }
                hidden = hidden ?? item
            }
        }
        return hidden
    }

    /// Shows `key` on `item`; a hidden copy right after it keeps the key it had.
    private static func bind(_ item: NSMenuItem, _ key: String, _ mods: NSEvent.ModifierFlags) {
        if !item.keyEquivalent.isEmpty, let menu = item.menu {
            let extra = NSMenuItem(title: item.title, action: item.action, keyEquivalent: item.keyEquivalent)
            extra.keyEquivalentModifierMask = item.keyEquivalentModifierMask
            extra.target = item.target
            extra.tag = item.tag
            extra.representedObject = item.representedObject
            extra.identifier = item.identifier
            extra.isHidden = true
            extra.allowsKeyEquivalentWhenHidden = true
            menu.insertItem(extra, at: menu.index(of: item) + 1)
            extraKeys.append(extra)
        }
        setKey(item, key, mods)
    }

    /// Changes an item's key, remembering the first one it had for the next `applyKeys`.
    private static func setKey(_ item: NSMenuItem, _ key: String, _ mods: NSEvent.ModifierFlags) {
        if !changed.contains(where: { $0.item === item }) {
            changed.append((item, item.keyEquivalent, item.keyEquivalentModifierMask))
        }
        item.keyEquivalent = key
        item.keyEquivalentModifierMask = key.isEmpty ? [] : mods
    }

    /// Takes the key equivalent `target` off every item under `menu` that has it.
    private static func displace(_ target: String, in menu: NSMenu) {
        for item in menu.items {
            if let sub = item.submenu, sub !== commandsMenu { displace(target, in: sub) }
            if !item.keyEquivalent.isEmpty,
               chord(item.keyEquivalent, item.keyEquivalentModifierMask) == target {
                setKey(item, "", [])
            }
        }
    }

    /// The digit each key types by position, on the digit row and the keypad (0x5A is F20, not a
    /// digit). Layouts that need Shift for digits, like AZERTY, still send these keys' codes.
    static let digitKeys: [UInt16: String] = {
        var digits: [UInt16: String] = [:]
        for (code, char) in KeyMapping.usLayout where ("0"..."9").contains(char) {
            digits[code] = String(char)
        }
        let keypad: [UInt16: String] = [
            0x52: "0", 0x53: "1", 0x54: "2", 0x55: "3", 0x56: "4", 0x57: "5", 0x58: "6", 0x59: "7",
            0x5B: "8", 0x5C: "9",
        ]
        return digits.merging(keypad) { digit, _ in digit }
    }()

    /// A ⌘+digit the menu did not match, by key position: on AZERTY ⌘1 types "&", so the menu
    /// finds nothing. Fires the item that holds ⌘+that digit, the way the menu would, and returns
    /// true; false when no enabled item has it (a `[keybindings]` "none" leaves it unbound).
    static func physicalFallback(_ event: NSEvent) -> Bool {
        guard event.type == .keyDown,
              event.modifierFlags.intersection([.command, .shift, .control, .option]) == [.command],
              let digit = digitKeys[event.keyCode],
              let item = keyedItem(digit, [.command], in: NSApp.mainMenu),
              let action = item.action,
              let target = NSApp.target(forAction: action, to: item.target, from: item),
              enabled(item, target: target)
        else { return false }
        NSApp.sendAction(action, to: target, from: item)
        return true
    }

    /// Whether the menu would enable `item` with this target: its validation decides, as when the
    /// menu matches the key itself, so a disabled action stays disabled.
    static func enabled(_ item: NSMenuItem, target: Any) -> Bool {
        guard item.menu?.autoenablesItems ?? true else { return item.isEnabled }
        if let validator = target as? NSMenuItemValidation { return validator.validateMenuItem(item) }
        if let validator = target as? NSUserInterfaceValidations { return validator.validateUserInterfaceItem(item) }
        return true
    }

    /// The first item under `menu` with this key equivalent, hidden copies included.
    static func keyedItem(_ key: String, _ mods: NSEvent.ModifierFlags, in menu: NSMenu?) -> NSMenuItem? {
        guard let menu else { return nil }
        let wanted = chord(key, mods)
        for item in menu.items {
            if let found = item.submenu.flatMap({ keyedItem(key, mods, in: $0) }) { return found }
            if !item.keyEquivalent.isEmpty, chord(item.keyEquivalent, item.keyEquivalentModifierMask) == wanted {
                return item
            }
        }
        return nil
    }

    /// A key equivalent in one spelling: an uppercase letter is the lowercase one with Shift.
    private static func chord(_ key: String, _ mods: NSEvent.ModifierFlags) -> String {
        var m = mods.intersection([.command, .shift, .option, .control])
        if key != key.lowercased() { m.insert(.shift) }
        return "\(m.rawValue)+\(key.lowercased())"
    }

    /// A `[keybindings]` chord ("cmd+shift+l", "ctrl+alt+f5") as a menu key equivalent; nil for
    /// one a menu cannot show.
    static func keyEquivalent(_ chord: String) -> (String, NSEvent.ModifierFlags)? {
        let spec = chord.trimmingCharacters(in: .whitespaces).lowercased()
        var parts = spec.split(separator: "+").map { $0.trimmingCharacters(in: .whitespaces) }
        // "cmd++" is the plus key.
        if spec.hasSuffix("++") { parts.append("plus") }
        guard let key = parts.popLast(), !key.isEmpty else { return nil }
        var mods: NSEvent.ModifierFlags = []
        for m in parts {
            switch m {
            case "cmd", "command", "super": mods.insert(.command)
            case "ctrl", "control": mods.insert(.control)
            case "alt", "opt", "option": mods.insert(.option)
            case "shift": mods.insert(.shift)
            default: return nil
            }
        }
        let function = { (code: Int) in String(UnicodeScalar(UInt32(code))!) }
        let named: [String: String] = [
            "plus": "+", "minus": "-", "equal": "=", "comma": ",", "period": ".", "slash": "/",
            "backslash": "\\", "semicolon": ";", "quote": "'", "grave": "`", "backtick": "`",
            "left_bracket": "[", "right_bracket": "]", "enter": "\r", "return": "\r",
            "esc": "\u{1b}", "escape": "\u{1b}", "space": " ", "tab": "\t", "backspace": "\u{8}",
            "delete": function(NSDeleteFunctionKey), "left": leftArrow, "right": rightArrow,
            "up": upArrow, "down": downArrow, "pageup": function(NSPageUpFunctionKey),
            "page_up": function(NSPageUpFunctionKey), "pagedown": function(NSPageDownFunctionKey),
            "page_down": function(NSPageDownFunctionKey), "home": function(NSHomeFunctionKey),
            "end": function(NSEndFunctionKey),
        ]
        if let k = named[key] { return (k, mods) }
        if key.count > 1, key.hasPrefix("f"), let n = Int(key.dropFirst()), (1...35).contains(n) {
            return (function(NSF1FunctionKey + n - 1), mods)
        }
        return key.count == 1 ? (key, mods) : nil
    }

    static func build() -> NSMenu {
        let main = NSMenu(title: "Main Menu")
        main.addItem(container(appMenu()))
        main.addItem(container(shellMenu()))
        main.addItem(container(editMenu()))
        main.addItem(container(viewMenu()))
        let window = windowMenu()
        main.addItem(container(window))
        let help = helpMenu()
        main.addItem(container(help))
        NSApp.windowsMenu = window
        NSApp.helpMenu = help
        return main
    }

    private static func container(_ menu: NSMenu) -> NSMenuItem {
        let item = NSMenuItem(title: menu.title, action: nil, keyEquivalent: "")
        item.submenu = menu
        return item
    }

    /// `name` is the item's `[keybindings]` action name.
    private static func item(_ title: String, _ action: Selector?, _ key: String = "",
                             _ modifiers: NSEvent.ModifierFlags = [.command], tag: Int = 0,
                             name: String? = nil) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.keyEquivalentModifierMask = key.isEmpty ? [] : modifiers
        item.tag = tag
        item.identifier = name.map(actionIdentifier)
        return item
    }

    // MARK: Menus

    private static func appMenu() -> NSMenu {
        let menu = NSMenu(title: "Thurm")
        menu.addItem(item("About Thurm", #selector(NSApplication.orderFrontStandardAboutPanel(_:)), name: "about"))
        for updates in Updater.shared.menuItems() { menu.addItem(updates) }
        menu.addItem(.separator())
        menu.addItem(item("Settings…", #selector(AppDelegate.showSettings(_:)), ",", name: "open_config"))
        menu.addItem(item("Reload Configuration", #selector(AppDelegate.reloadConfig(_:)), ",", [.command, .shift],
                          name: "reload_config"))
        menu.addItem(item("Remotes…", #selector(AppDelegate.showRemotes(_:)), name: "remotes"))
        let integrations = NSMenu(title: "Integrations")
        integrations.delegate = Integrations.shared
        let integrationsItem = item("Integrations", nil)
        integrationsItem.submenu = integrations
        menu.addItem(integrationsItem)
        menu.addItem(.separator())
        menu.addItem(item("Secure Keyboard Entry", #selector(AppDelegate.toggleSecureInput(_:)), "i",
                          [.command, .option], name: "secure_keyboard_entry"))
        menu.addItem(.separator())
        let services = NSMenu(title: "Services")
        let servicesItem = item("Services", nil)
        servicesItem.submenu = services
        NSApp.servicesMenu = services
        menu.addItem(servicesItem)
        menu.addItem(.separator())
        menu.addItem(item("Hide Thurm", #selector(NSApplication.hide(_:)), "h", name: "hide"))
        menu.addItem(item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option],
                          name: "hide_others"))
        menu.addItem(item("Show All", #selector(NSApplication.unhideAllApplications(_:)), name: "show_all"))
        menu.addItem(.separator())
        menu.addItem(item("Quit Thurm", #selector(NSApplication.terminate(_:)), "q", name: "quit"))
        return menu
    }

    private static func shellMenu() -> NSMenu {
        let menu = NSMenu(title: "Shell")
        menu.addItem(item("New Tab", #selector(AppDelegate.newTab(_:)), "t", name: "new_tab"))
        menu.addItem(item("New Workspace", #selector(AppDelegate.newWorkspace(_:)), "n", name: "new_workspace"))
        // ⇧⌘N, its shortcut before Thurm had one window.
        let alias = item("New Workspace", #selector(AppDelegate.newWorkspace(_:)), "n", [.command, .shift],
                         name: "new_workspace")
        alias.isHidden = true
        alias.allowsKeyEquivalentWhenHidden = true
        menu.addItem(alias)
        menu.addItem(item("Switch Workspace…", #selector(AppDelegate.switchWorkspace(_:)), "o", [.command, .shift],
                          name: "switch_workspace"))
        menu.addItem(item("Switch to Agent…", #selector(AppDelegate.switchAgent(_:)), "a", [.command, .shift],
                          name: "switch_agent"))
        menu.addItem(item("Agent Wall", #selector(TerminalWindowController.toggleAgentWall(_:)), "a",
                          [.command, .control], name: "agent_wall"))
        menu.addItem(item("Rename Workspace…", #selector(AppDelegate.renameWorkspace(_:)), name: "rename_workspace"))
        menu.addItem(item("Close Workspace…", #selector(AppDelegate.closeWorkspace(_:)), name: "close_workspace"))
        menu.addItem(.separator())
        menu.addItem(item("Split Right", #selector(TerminalWindowController.splitRight(_:)), "d", name: "split_right"))
        menu.addItem(item("Split Down", #selector(TerminalWindowController.splitDown(_:)), "d", [.command, .shift],
                          name: "split_down"))
        menu.addItem(.separator())
        menu.addItem(item("Command Palette…", #selector(AppDelegate.showCommandPalette(_:)), "p", [.command, .shift],
                          name: "command_palette"))
        menu.addItem(item("Processes & Ports…", #selector(AppDelegate.showProcesses(_:)), name: "processes"))
        let commands = item("Commands", nil)
        commands.submenu = commandsMenu
        commands.isHidden = commandsMenu.items.isEmpty
        commandsItem = commands
        menu.addItem(commands)
        menu.addItem(.separator())
        menu.addItem(item("Close Pane", #selector(TerminalWindowController.closePane(_:)), "w", name: "close_pane"))
        menu.addItem(item("Close Tab", #selector(NSWindow.performClose(_:)), "w", [.command, .shift],
                          name: "close_tab"))
        return menu
    }

    private static func editMenu() -> NSMenu {
        let menu = NSMenu(title: "Edit")
        menu.addItem(item("Copy", #selector(TerminalView.copy(_:)), "c", name: "copy"))
        menu.addItem(item("Paste", #selector(TerminalView.paste(_:)), "v", name: "paste"))
        menu.addItem(item("Select All", #selector(TerminalView.selectAll(_:)), "a", name: "select_all"))
        menu.addItem(.separator())
        menu.addItem(item("Clear Screen", #selector(TerminalView.clearScreen(_:)), "k", name: "clear_screen"))
        menu.addItem(item("Clear Scrollback", #selector(TerminalView.clearScrollback(_:)), "k", [.command, .option],
                          name: "clear_scrollback"))
        menu.addItem(item("Explain Last Command", #selector(TerminalView.explainLastCommand(_:)), name: "explain"))
        menu.addItem(.separator())
        menu.addItem(item("Find…", #selector(TerminalWindowController.showFind(_:)), "f", name: "find"))
        menu.addItem(item("Find Next", #selector(TerminalWindowController.findNext(_:)), "g", name: "find_next"))
        menu.addItem(item("Find Previous", #selector(TerminalWindowController.findPrevious(_:)), "g",
                          [.command, .shift], name: "find_previous"))
        return menu
    }

    private static func viewMenu() -> NSMenu {
        let menu = NSMenu(title: "View")
        menu.addItem(item("Increase Font Size", #selector(AppDelegate.increaseFontSize(_:)), "=",
                          name: "increase_font_size"))
        let plus = item("Increase Font Size", #selector(AppDelegate.increaseFontSize(_:)), "+",
                        name: "increase_font_size")
        plus.isHidden = true
        plus.allowsKeyEquivalentWhenHidden = true
        menu.addItem(plus)
        menu.addItem(item("Decrease Font Size", #selector(AppDelegate.decreaseFontSize(_:)), "-",
                          name: "decrease_font_size"))
        menu.addItem(item("Reset Font Size", #selector(AppDelegate.resetFontSize(_:)), "0", name: "reset_font_size"))
        menu.addItem(.separator())
        menu.addItem(item("Tabs in Sidebar", #selector(AppDelegate.toggleSidebarTabs(_:)), "s", [.command, .control],
                          name: "toggle_tab_style"))
        menu.addItem(item("Agents in Sidebar", #selector(AppDelegate.toggleSidebarAgents(_:)),
                          name: "toggle_sidebar_agents"))
        let usage = NSMenu(title: "Usage Limits in Sidebar")
        usage.addItem(item("Claude Code", #selector(AppDelegate.toggleSidebarUsageClaude(_:)),
                           name: "toggle_sidebar_usage_claude"))
        usage.addItem(item("Codex", #selector(AppDelegate.toggleSidebarUsageCodex(_:)),
                           name: "toggle_sidebar_usage_codex"))
        let usageItem = item("Usage Limits in Sidebar", nil)
        usageItem.submenu = usage
        menu.addItem(usageItem)
        menu.addItem(item("Hide Sidebar", #selector(AppDelegate.toggleTabSidebar(_:)), "b", name: "toggle_sidebar"))
        let theme = NSMenu(title: "Theme")
        theme.delegate = ThemeMenu.shared
        let themeItem = item("Theme", nil)
        themeItem.submenu = theme
        menu.addItem(themeItem)
        menu.addItem(.separator())
        menu.addItem(item("Show Active Pane Border", #selector(AppDelegate.toggleActivePaneBorder(_:)),
                          name: "toggle_active_pane_border"))
        menu.addItem(item("Zoom Split", #selector(TerminalWindowController.toggleZoom(_:)), "\r", [.command, .shift],
                          name: "zoom_split"))
        menu.addItem(item("Equalize Splits", #selector(TerminalWindowController.equalizeSplits(_:)), "=",
                          [.command, .control], name: "equalize_splits"))
        menu.addItem(.separator())
        let focusMods: NSEvent.ModifierFlags = [.command, .option]
        menu.addItem(item("Select Split Left", #selector(TerminalWindowController.focusLeft(_:)), leftArrow, focusMods,
                          name: "focus_left"))
        menu.addItem(item("Select Split Right", #selector(TerminalWindowController.focusRight(_:)), rightArrow,
                          focusMods, name: "focus_right"))
        menu.addItem(item("Select Split Above", #selector(TerminalWindowController.focusUp(_:)), upArrow, focusMods,
                          name: "focus_up"))
        menu.addItem(item("Select Split Below", #selector(TerminalWindowController.focusDown(_:)), downArrow, focusMods,
                          name: "focus_down"))
        menu.addItem(.separator())
        let resizeMods: NSEvent.ModifierFlags = [.command, .control]
        menu.addItem(item("Move Divider Left", #selector(TerminalWindowController.resizeLeft(_:)), leftArrow,
                          resizeMods, name: "resize_left"))
        menu.addItem(item("Move Divider Right", #selector(TerminalWindowController.resizeRight(_:)), rightArrow,
                          resizeMods, name: "resize_right"))
        menu.addItem(item("Move Divider Up", #selector(TerminalWindowController.resizeUp(_:)), upArrow, resizeMods,
                          name: "resize_up"))
        menu.addItem(item("Move Divider Down", #selector(TerminalWindowController.resizeDown(_:)), downArrow,
                          resizeMods, name: "resize_down"))
        menu.addItem(.separator())
        let moveMods: NSEvent.ModifierFlags = [.command, .control, .shift]
        menu.addItem(item("Move Pane Left", #selector(TerminalWindowController.movePaneLeft(_:)), leftArrow, moveMods,
                          name: "move_pane_left"))
        menu.addItem(item("Move Pane Right", #selector(TerminalWindowController.movePaneRight(_:)), rightArrow,
                          moveMods, name: "move_pane_right"))
        menu.addItem(item("Move Pane Up", #selector(TerminalWindowController.movePaneUp(_:)), upArrow, moveMods,
                          name: "move_pane_up"))
        menu.addItem(item("Move Pane Down", #selector(TerminalWindowController.movePaneDown(_:)), downArrow, moveMods,
                          name: "move_pane_down"))
        menu.addItem(.separator())
        menu.addItem(item("Toggle Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control],
                          name: "toggle_fullscreen"))
        return menu
    }

    private static func windowMenu() -> NSMenu {
        let menu = NSMenu(title: "Window")
        menu.addItem(item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m", name: "minimize"))
        menu.addItem(item("Zoom", #selector(NSWindow.performZoom(_:)), name: "maximize"))
        menu.addItem(.separator())
        menu.addItem(item("Show Previous Tab", #selector(NSWindow.selectPreviousTab(_:)), "[", [.command, .shift],
                          name: "previous_tab"))
        menu.addItem(item("Show Next Tab", #selector(NSWindow.selectNextTab(_:)), "]", [.command, .shift],
                          name: "next_tab"))
        for n in 1...9 {
            let title = n == 9 ? "Select Last Tab" : "Select Tab \(n)"
            menu.addItem(item(title, #selector(TerminalWindowController.selectTabByNumber(_:)), "\(n)", [.command],
                              tag: n, name: n == 9 ? "select_last_tab" : "select_tab_\(n)"))
        }
        menu.addItem(.separator())
        menu.addItem(item("Move Tab to New Workspace", #selector(NSWindow.moveTabToNewWindow(_:)),
                          name: "move_tab_to_new_workspace"))
        menu.addItem(.separator())
        menu.addItem(item("Quick Terminal", #selector(AppDelegate.toggleQuickTerminal(_:)), name: "quick_terminal"))
        menu.addItem(.separator())
        let workspaces = NSMenu(title: "Workspaces")
        workspaces.delegate = WorkspaceMenuDelegate.shared
        let workspacesItem = item("Workspaces", nil)
        workspacesItem.submenu = workspaces
        menu.addItem(workspacesItem)
        menu.addItem(.separator())
        menu.addItem(item("Bring All to Front", #selector(NSApplication.arrangeInFront(_:)),
                          name: "bring_all_to_front"))
        return menu
    }

    private static func helpMenu() -> NSMenu {
        let menu = NSMenu(title: "Help")
        menu.addItem(item("Open Configuration File", #selector(AppDelegate.openConfig(_:)), name: "open_config_file"))
        return menu
    }
}

/// View > Theme, rebuilt every time it opens from the current config.
final class ThemeMenu: NSObject, NSMenuDelegate {
    static let shared = ThemeMenu()

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let cfg = SessionManager.shared.config
        let browse = NSMenuItem(title: "Browse Themes…", action: #selector(browse(_:)), keyEquivalent: "")
        browse.target = self
        menu.addItem(browse)
        menu.addItem(.separator())
        let follow = NSMenuItem(title: "Match System Appearance", action: #selector(toggleFollow(_:)),
                                keyEquivalent: "")
        follow.target = self
        follow.state = cfg.followsAppearance ? .on : .off
        menu.addItem(follow)
        menu.addItem(.separator())
        if cfg.followsAppearance {
            menu.addItem(.sectionHeader(title: "Light Appearance"))
            addThemes(cfg.themes.filter { !$0.dark }, selected: cfg.themeLight, to: menu)
            menu.addItem(.separator())
            menu.addItem(.sectionHeader(title: "Dark Appearance"))
            addThemes(cfg.themes.filter { $0.dark }, selected: cfg.themeDark, to: menu)
        } else {
            addThemes(cfg.themes, selected: cfg.theme.name, to: menu)
        }
    }

    /// Thurm's own themes, then the collection in a "More Themes" submenu with one submenu per
    /// first letter (hundreds of them). The current theme is checked wherever it is, and so is
    /// the path leading to it.
    private func addThemes(_ themes: [ThemeChoice], selected: String, to menu: NSMenu) {
        for t in themes where t.own {
            menu.addItem(themeItem(t.name, selected: selected))
        }
        let more = themes.filter { !$0.own }
        guard !more.isEmpty else { return }
        let moreMenu = NSMenu()
        var groups: [(String, [ThemeChoice])] = []
        for t in more {
            let first = t.name.first.map { $0.isLetter ? String($0).uppercased() : "#" } ?? "#"
            if groups.last?.0 == first {
                groups[groups.count - 1].1.append(t)
            } else {
                groups.append((first, [t]))
            }
        }
        for (letter, list) in groups {
            let sub = NSMenu()
            for t in list { sub.addItem(themeItem(t.name, selected: selected)) }
            let item = NSMenuItem(title: letter == "#" ? "0–9" : letter, action: nil, keyEquivalent: "")
            item.submenu = sub
            item.state = list.contains { $0.name == selected } ? .mixed : .off
            moreMenu.addItem(item)
        }
        let item = NSMenuItem(title: "More Themes", action: nil, keyEquivalent: "")
        item.submenu = moreMenu
        item.state = more.contains { $0.name == selected } ? .mixed : .off
        menu.addItem(item)
    }

    private func themeItem(_ name: String, selected: String) -> NSMenuItem {
        let item = NSMenuItem(title: name, action: #selector(choose(_:)), keyEquivalent: "")
        item.target = self
        item.representedObject = name
        item.state = name == selected ? .on : .off
        return item
    }

    @objc private func choose(_ sender: NSMenuItem) {
        guard let name = sender.representedObject as? String else { return }
        SessionManager.shared.chooseTheme(name)
    }

    @objc private func browse(_ sender: NSMenuItem) {
        SessionManager.shared.showThemePicker()
    }

    @objc private func toggleFollow(_ sender: NSMenuItem) {
        SessionManager.shared.toggleFollowAppearance()
    }
}

/// Window > Workspaces: the list, rebuilt when opened.
final class WorkspaceMenuDelegate: NSObject, NSMenuDelegate {
    static let shared = WorkspaceMenuDelegate()

    func menuNeedsUpdate(_ menu: NSMenu) {
        SessionManager.shared.fillWorkspaceMenu(menu)
        // File has these actions (with their shortcuts); keep only the list here.
        while let last = menu.items.last, !(last.isSeparatorItem) { menu.removeItem(last) }
        if let last = menu.items.last, last.isSeparatorItem { menu.removeItem(last) }
    }
}
