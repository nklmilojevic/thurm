import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate, NSMenuItemValidation {
    // MARK: Lifecycle

    func applicationWillFinishLaunching(_ notification: Notification) {
        // We restore windows ourselves from the daemon's layout.
        UserDefaults.standard.register(defaults: [
            "NSQuitAlwaysKeepsWindows": false,
            "ApplePersistenceIgnoreState": true,
        ])
        NSWindow.allowsAutomaticWindowTabbing = true
        NSApp.mainMenu = MainMenu.build()
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        FontShaper.registerBundledFonts()
        Notifications.shared.setup()
        SecureInput.shared.restoreUserPreference()
        SessionManager.shared.start()
        Updater.shared.start()
        NSApp.activate()
    }

    func applicationShouldTerminate(_ sender: NSApplication) -> NSApplication.TerminateReply {
        SessionManager.shared.prepareForTermination()
        return .terminateNow
    }

    func applicationWillTerminate(_ notification: Notification) {
        SecureInput.shared.releaseAll()
    }

    func applicationShouldTerminateAfterLastWindowClosed(_ sender: NSApplication) -> Bool {
        SessionManager.shared.config.quitAfterLastWindow
    }

    func applicationShouldHandleReopen(_ sender: NSApplication, hasVisibleWindows flag: Bool) -> Bool {
        if !flag && SessionManager.shared.regularControllers.isEmpty {
            SessionManager.shared.newWindow()
        }
        return true
    }

    func applicationDidBecomeActive(_ notification: Notification) {
        SessionManager.shared.appDidBecomeActive()
    }

    func applicationDidResignActive(_ notification: Notification) {
        SessionManager.shared.appDidResignActive()
    }

    func applicationSupportsSecureRestorableState(_ app: NSApplication) -> Bool {
        true
    }

    func applicationDockMenu(_ sender: NSApplication) -> NSMenu? {
        let menu = NSMenu()
        menu.addItem(NSMenuItem(title: "New Window", action: #selector(newWindow(_:)), keyEquivalent: ""))
        menu.addItem(NSMenuItem(title: "New Tab", action: #selector(newTab(_:)), keyEquivalent: ""))
        for item in menu.items { item.target = self }
        return menu
    }

    // MARK: Actions

    @objc func newWindow(_ sender: Any?) {
        SessionManager.shared.newWindow()
    }

    @objc func newTab(_ sender: Any?) {
        SessionManager.shared.newTab(from: SessionManager.shared.currentController)
    }

    @objc func openConfig(_ sender: Any?) {
        SessionManager.shared.openConfigFile()
    }

    @objc func reloadConfig(_ sender: Any?) {
        SessionManager.shared.reloadConfig(notifyDaemon: true)
    }

    @objc func showRemotes(_ sender: Any?) {
        RemotesWindow.shared.show()
    }

    @objc func showProcesses(_ sender: Any?) {
        ProcessPanel.shared.show()
    }

    @objc func toggleSidebarTabs(_ sender: Any?) {
        SessionManager.shared.toggleSidebarTabs()
    }

    @objc func newWorkspace(_ sender: Any?) {
        SessionManager.shared.newWorkspace()
    }

    @objc func switchWorkspace(_ sender: Any?) {
        SessionManager.shared.showWorkspaceSwitcher()
    }

    @objc func switchAgent(_ sender: Any?) {
        SessionManager.shared.showAgentPicker()
    }

    @objc func renameWorkspace(_ sender: Any?) {
        if let ws = SessionManager.shared.currentWorkspace { SessionManager.shared.renameWorkspace(ws) }
    }

    @objc func closeWorkspace(_ sender: Any?) {
        if let ws = SessionManager.shared.currentWorkspace { SessionManager.shared.closeWorkspace(ws) }
    }

    /// Cmd+B: collapse or expand the vertical tab sidebar of the front window.
    @objc func toggleTabSidebar(_ sender: Any?) {
        SessionManager.shared.currentController?.tabSplit?.toggleSidebarAnimated()
    }

    @objc func toggleQuickTerminal(_ sender: Any?) {
        QuickTerminal.shared.toggle()
    }

    @objc func toggleSecureInput(_ sender: Any?) {
        SecureInput.shared.toggleGlobal()
    }

    @objc func showCommandPalette(_ sender: Any?) {
        SessionManager.shared.showCommandPalette()
    }

    @objc func increaseFontSize(_ sender: Any?) {
        SessionManager.shared.changeFontSize(by: 1)
    }

    @objc func decreaseFontSize(_ sender: Any?) {
        SessionManager.shared.changeFontSize(by: -1)
    }

    @objc func resetFontSize(_ sender: Any?) {
        SessionManager.shared.resetFontSize()
    }

    // MARK: NSMenuItemValidation

    func validateMenuItem(_ menuItem: NSMenuItem) -> Bool {
        if menuItem.action == #selector(toggleSecureInput(_:)) {
            menuItem.state = SecureInput.shared.globalEnabled ? .on : .off
        }
        if menuItem.action == #selector(renameWorkspace(_:)) || menuItem.action == #selector(closeWorkspace(_:)) {
            return SessionManager.shared.currentWorkspace != nil
        }
        if menuItem.action == #selector(toggleTabSidebar(_:)) {
            guard let split = SessionManager.shared.currentController?.tabSplit else {
                menuItem.title = "Hide Sidebar"
                return false
            }
            menuItem.title = split.sidebarCollapsed ? "Show Sidebar" : "Hide Sidebar"
        }
        if menuItem.action == #selector(toggleSidebarTabs(_:)) {
            menuItem.state = SessionManager.shared.config.sidebarTabs ? .on : .off
        }
        return true
    }
}
