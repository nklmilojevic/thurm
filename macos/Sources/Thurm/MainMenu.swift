import AppKit

/// Programmatic main menu (no nib). Actions are sent to the first responder, so they reach the
/// focused TerminalView, its TerminalWindowController or the AppDelegate.
enum MainMenu {
    // Function-key characters used as key equivalents.
    private static let upArrow = "\u{F700}"
    private static let downArrow = "\u{F701}"
    private static let leftArrow = "\u{F702}"
    private static let rightArrow = "\u{F703}"

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

    private static func item(_ title: String, _ action: Selector?, _ key: String = "",
                             _ modifiers: NSEvent.ModifierFlags = [.command], tag: Int = 0) -> NSMenuItem {
        let item = NSMenuItem(title: title, action: action, keyEquivalent: key)
        item.keyEquivalentModifierMask = key.isEmpty ? [] : modifiers
        item.tag = tag
        return item
    }

    // MARK: Menus

    private static func appMenu() -> NSMenu {
        let menu = NSMenu(title: "Thurm")
        menu.addItem(item("About Thurm", #selector(NSApplication.orderFrontStandardAboutPanel(_:))))
        for updates in Updater.shared.menuItems() { menu.addItem(updates) }
        menu.addItem(.separator())
        menu.addItem(item("Settings…", #selector(AppDelegate.openConfig(_:)), ","))
        menu.addItem(item("Reload Configuration", #selector(AppDelegate.reloadConfig(_:)), ",", [.command, .shift]))
        menu.addItem(item("Remotes…", #selector(AppDelegate.showRemotes(_:))))
        let integrations = NSMenu(title: "Integrations")
        integrations.delegate = Integrations.shared
        let integrationsItem = item("Integrations", nil)
        integrationsItem.submenu = integrations
        menu.addItem(integrationsItem)
        menu.addItem(.separator())
        menu.addItem(item("Secure Keyboard Entry", #selector(AppDelegate.toggleSecureInput(_:)), "i",
                          [.command, .option]))
        menu.addItem(.separator())
        let services = NSMenu(title: "Services")
        let servicesItem = item("Services", nil)
        servicesItem.submenu = services
        NSApp.servicesMenu = services
        menu.addItem(servicesItem)
        menu.addItem(.separator())
        menu.addItem(item("Hide Thurm", #selector(NSApplication.hide(_:)), "h"))
        menu.addItem(item("Hide Others", #selector(NSApplication.hideOtherApplications(_:)), "h", [.command, .option]))
        menu.addItem(item("Show All", #selector(NSApplication.unhideAllApplications(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Quit Thurm", #selector(NSApplication.terminate(_:)), "q"))
        return menu
    }

    private static func shellMenu() -> NSMenu {
        let menu = NSMenu(title: "Shell")
        menu.addItem(item("New Window", #selector(AppDelegate.newWindow(_:)), "n"))
        menu.addItem(item("New Tab", #selector(AppDelegate.newTab(_:)), "t"))
        menu.addItem(item("New Workspace", #selector(AppDelegate.newWorkspace(_:)), "n", [.command, .shift]))
        menu.addItem(item("Switch Workspace…", #selector(AppDelegate.switchWorkspace(_:)), "o", [.command, .shift]))
        menu.addItem(item("Switch to Agent…", #selector(AppDelegate.switchAgent(_:)), "a", [.command, .shift]))
        menu.addItem(item("Rename Workspace…", #selector(AppDelegate.renameWorkspace(_:))))
        menu.addItem(item("Close Workspace…", #selector(AppDelegate.closeWorkspace(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Split Right", #selector(TerminalWindowController.splitRight(_:)), "d"))
        menu.addItem(item("Split Down", #selector(TerminalWindowController.splitDown(_:)), "d", [.command, .shift]))
        menu.addItem(.separator())
        menu.addItem(item("Command Palette…", #selector(AppDelegate.showCommandPalette(_:)), "p", [.command, .shift]))
        menu.addItem(item("Processes & Ports…", #selector(AppDelegate.showProcesses(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Close Pane", #selector(TerminalWindowController.closePane(_:)), "w"))
        menu.addItem(item("Close Tab", #selector(NSWindow.performClose(_:)), "w", [.command, .shift]))
        return menu
    }

    private static func editMenu() -> NSMenu {
        let menu = NSMenu(title: "Edit")
        menu.addItem(item("Copy", #selector(TerminalView.copy(_:)), "c"))
        menu.addItem(item("Paste", #selector(TerminalView.paste(_:)), "v"))
        menu.addItem(item("Select All", #selector(TerminalView.selectAll(_:)), "a"))
        menu.addItem(.separator())
        menu.addItem(item("Clear Screen", #selector(TerminalView.clearScreen(_:)), "k"))
        menu.addItem(item("Clear Scrollback", #selector(TerminalView.clearScrollback(_:)), "k", [.command, .option]))
        menu.addItem(item("Explain Last Command", #selector(TerminalView.explainLastCommand(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Find…", #selector(TerminalWindowController.showFind(_:)), "f"))
        menu.addItem(item("Find Next", #selector(TerminalWindowController.findNext(_:)), "g"))
        menu.addItem(item("Find Previous", #selector(TerminalWindowController.findPrevious(_:)), "g",
                          [.command, .shift]))
        return menu
    }

    private static func viewMenu() -> NSMenu {
        let menu = NSMenu(title: "View")
        menu.addItem(item("Increase Font Size", #selector(AppDelegate.increaseFontSize(_:)), "="))
        let plus = item("Increase Font Size", #selector(AppDelegate.increaseFontSize(_:)), "+")
        plus.isHidden = true
        plus.allowsKeyEquivalentWhenHidden = true
        menu.addItem(plus)
        menu.addItem(item("Decrease Font Size", #selector(AppDelegate.decreaseFontSize(_:)), "-"))
        menu.addItem(item("Reset Font Size", #selector(AppDelegate.resetFontSize(_:)), "0"))
        menu.addItem(.separator())
        menu.addItem(item("Tabs in Sidebar", #selector(AppDelegate.toggleSidebarTabs(_:)), "s", [.command, .control]))
        menu.addItem(item("Hide Sidebar", #selector(AppDelegate.toggleTabSidebar(_:)), "b"))
        let theme = NSMenu(title: "Theme")
        theme.delegate = ThemeMenu.shared
        let themeItem = item("Theme", nil)
        themeItem.submenu = theme
        menu.addItem(themeItem)
        menu.addItem(.separator())
        menu.addItem(item("Zoom Split", #selector(TerminalWindowController.toggleZoom(_:)), "\r", [.command, .shift]))
        menu.addItem(item("Equalize Splits", #selector(TerminalWindowController.equalizeSplits(_:)), "=",
                          [.command, .control]))
        menu.addItem(.separator())
        let focusMods: NSEvent.ModifierFlags = [.command, .option]
        menu.addItem(item("Select Split Left", #selector(TerminalWindowController.focusLeft(_:)), leftArrow, focusMods))
        menu.addItem(item("Select Split Right", #selector(TerminalWindowController.focusRight(_:)), rightArrow, focusMods))
        menu.addItem(item("Select Split Above", #selector(TerminalWindowController.focusUp(_:)), upArrow, focusMods))
        menu.addItem(item("Select Split Below", #selector(TerminalWindowController.focusDown(_:)), downArrow, focusMods))
        menu.addItem(.separator())
        let resizeMods: NSEvent.ModifierFlags = [.command, .control]
        menu.addItem(item("Move Divider Left", #selector(TerminalWindowController.resizeLeft(_:)), leftArrow, resizeMods))
        menu.addItem(item("Move Divider Right", #selector(TerminalWindowController.resizeRight(_:)), rightArrow,
                          resizeMods))
        menu.addItem(item("Move Divider Up", #selector(TerminalWindowController.resizeUp(_:)), upArrow, resizeMods))
        menu.addItem(item("Move Divider Down", #selector(TerminalWindowController.resizeDown(_:)), downArrow,
                          resizeMods))
        menu.addItem(.separator())
        menu.addItem(item("Toggle Full Screen", #selector(NSWindow.toggleFullScreen(_:)), "f", [.command, .control]))
        return menu
    }

    private static func windowMenu() -> NSMenu {
        let menu = NSMenu(title: "Window")
        menu.addItem(item("Minimize", #selector(NSWindow.performMiniaturize(_:)), "m"))
        menu.addItem(item("Zoom", #selector(NSWindow.performZoom(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Show Previous Tab", #selector(NSWindow.selectPreviousTab(_:)), "[", [.command, .shift]))
        menu.addItem(item("Show Next Tab", #selector(NSWindow.selectNextTab(_:)), "]", [.command, .shift]))
        for n in 1...9 {
            let title = n == 9 ? "Select Last Tab" : "Select Tab \(n)"
            menu.addItem(item(title, #selector(TerminalWindowController.selectTabByNumber(_:)), "\(n)", [.command],
                              tag: n))
        }
        menu.addItem(.separator())
        menu.addItem(item("Merge All Windows", #selector(NSWindow.mergeAllWindows(_:))))
        menu.addItem(item("Move Tab to New Window", #selector(NSWindow.moveTabToNewWindow(_:))))
        menu.addItem(.separator())
        menu.addItem(item("Quick Terminal", #selector(AppDelegate.toggleQuickTerminal(_:))))
        menu.addItem(.separator())
        let workspaces = NSMenu(title: "Workspaces")
        workspaces.delegate = WorkspaceMenuDelegate.shared
        let workspacesItem = item("Workspaces", nil)
        workspacesItem.submenu = workspaces
        menu.addItem(workspacesItem)
        menu.addItem(.separator())
        menu.addItem(item("Bring All to Front", #selector(NSApplication.arrangeInFront(_:))))
        return menu
    }

    private static func helpMenu() -> NSMenu {
        let menu = NSMenu(title: "Help")
        menu.addItem(item("Open Configuration File", #selector(AppDelegate.openConfig(_:))))
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
