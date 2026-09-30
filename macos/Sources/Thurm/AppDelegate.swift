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
        if offerMoveToApplications() { return }
        startUp()
    }

    /// Everything after the launch-location check.
    func startUp() {
        if MetalContext.shared == nil {
            // Every pane would stay blank.
            let alert = NSAlert()
            alert.alertStyle = .critical
            alert.messageText = "Thurm Can't Draw on This Mac"
            alert.informativeText = "Thurm needs Metal to draw terminals, and Metal is not available "
                + "on this Mac."
            alert.runModal()
            NSApp.terminate(nil)
            return
        }
        FontShaper.registerBundledFonts()
        Notifications.shared.setup()
        SecureInput.shared.restoreUserPreference()
        SessionManager.shared.start()
        Updater.shared.start()
        NSApp.activate()
    }

    /// Opened from the disk image (or translocated by Gatekeeper to a random read-only path): the
    /// daemon, the command-line tool and the login item would point at a path that disappears,
    /// and updates couldn't replace the app. Offers to copy it to Applications and relaunch
    /// from there; true when that is happening.
    private func offerMoveToApplications() -> Bool {
        let fm = FileManager.default
        let url = Bundle.main.bundleURL
        let translocated = url.path.contains("/AppTranslocation/")
        let readOnly = (try? url.resourceValues(forKeys: [.volumeIsReadOnlyKey]))?.volumeIsReadOnly == true
        guard url.pathExtension == "app", translocated || readOnly else { return false }
        let alert = NSAlert()
        alert.messageText = "Move Thurm to Applications?"
        alert.informativeText = "Thurm is running from a disk image. The session daemon, the command-line "
            + "tool and updates need it installed in Applications."
        alert.addButton(withTitle: "Move to Applications")
        alert.addButton(withTitle: "Not Now")
        guard alert.runModal() == .alertFirstButtonReturn else { return false }
        let targets = [URL(fileURLWithPath: "/Applications"),
                       fm.homeDirectoryForCurrentUser.appendingPathComponent("Applications")]
        for dir in targets {
            let dest = dir.appendingPathComponent(url.lastPathComponent)
            // Copied next to it first: an installed Thurm is only replaced by a complete copy.
            let staging = dir.appendingPathComponent(".\(url.lastPathComponent).\(UUID().uuidString)")
            do {
                try fm.createDirectory(at: dir, withIntermediateDirectories: true)
                try fm.copyItem(at: url, to: staging)
                if fm.fileExists(atPath: dest.path) {
                    _ = try fm.replaceItemAt(dest, withItemAt: staging)
                } else {
                    try fm.moveItem(at: staging, to: dest)
                }
            } catch {
                try? fm.removeItem(at: staging)
                continue
            }
            // Gatekeeper already checked this copy of the app; without the quarantine flag the
            // copy isn't translocated again.
            let xattr = Process()
            xattr.executableURL = URL(fileURLWithPath: "/usr/bin/xattr")
            xattr.arguments = ["-dr", "com.apple.quarantine", dest.path]
            try? xattr.run()
            xattr.waitUntilExit()
            // This copy quits only once the moved one runs; if it can't be opened, this one
            // carries on.
            NSWorkspace.shared.openApplication(at: dest, configuration: NSWorkspace.OpenConfiguration()) { app, error in
                DispatchQueue.main.async {
                    if app != nil && error == nil {
                        NSApp.terminate(nil)
                        return
                    }
                    let failed = NSAlert()
                    failed.messageText = "Thurm Could Not Be Opened from Applications"
                    failed.informativeText = "It was copied to \(dest.path). \(error?.localizedDescription ?? "")"
                    failed.runModal()
                    (NSApp.delegate as? AppDelegate)?.startUp()
                }
            }
            return true
        }
        let failed = NSAlert()
        failed.messageText = "Thurm Could Not Be Moved"
        failed.informativeText = "Drag Thurm from the disk image to Applications, then open it from there."
        failed.runModal()
        return false
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
        menu.addItem(NSMenuItem(title: "New Tab", action: #selector(newTab(_:)), keyEquivalent: ""))
        menu.addItem(NSMenuItem(title: "New Workspace", action: #selector(newWorkspace(_:)), keyEquivalent: ""))
        for item in menu.items { item.target = self }
        return menu
    }

    // MARK: Actions

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
