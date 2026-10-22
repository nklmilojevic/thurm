import AppKit

/// Thurm > Integrations: what Thurm can set up outside the app, each done only when chosen
/// (the command palette lists them too). Shell integration needs nothing: the daemon injects it.
final class Integrations: NSObject, NSMenuDelegate {
    static let shared = Integrations()

    private let fm = FileManager.default
    private var home: URL { fm.homeDirectoryForCurrentUser }
    private var cli: URL { Bundle.main.bundleURL.appendingPathComponent("Contents/Helpers/thurm") }
    private var launchAgent: URL { home.appendingPathComponent("Library/LaunchAgents/com.thurm.daemon.plist") }
    /// `~/.local/bin` when it exists (no password needed), else `/usr/local/bin`.
    private var cliLinkCandidates: [URL] {
        [home.appendingPathComponent(".local/bin/thurm"), URL(fileURLWithPath: "/usr/local/bin/thurm")]
    }

    // MARK: Menu

    func menuNeedsUpdate(_ menu: NSMenu) {
        menu.removeAllItems()
        let cliLink = installedCLILink()
        add(menu, cliLink == nil ? "Install Command-Line Tool" : "Uninstall Command-Line Tool",
            on: cliLink != nil, tip: cliLink.map { "Linked at \($0.path)" }) { [self] in
            cliLink == nil ? installCLI() : uninstallCLI(cliLink!)
        }
        for agent in hookStatus() where agent.present {
            let complete = agent.installed == agent.total
            let title = complete ? "Uninstall \(agent.name) Hooks" : "Install \(agent.name) Hooks"
            add(menu, title, on: complete, tip: "Agent status in tabs and the sidebar (\(agent.path))") { [self] in
                toggleHooks(agent, install: !complete)
            }
        }
        // A skill of the user's own by that name is theirs to keep.
        let skills = skillStatus().filter { $0.present && $0.state != "other" }
        if !skills.isEmpty {
            let on = skills.allSatisfy { $0.state == "current" }
            let names = skills.map(\.name).joined(separator: ", ")
            add(menu, on ? "Uninstall thurm Skill for Agents" : "Install thurm Skill for Agents",
                on: on, tip: "Lets \(names) open panes and read other panes' output") { [self] in
                run(["skill", on ? "remove" : "setup"], title: "thurm Skill")
            }
        }
        let agentOn = fm.fileExists(atPath: launchAgent.path)
        add(menu, agentOn ? "Don't Start Daemon at Login" : "Start Daemon at Login", on: agentOn,
            tip: "Sessions are running before Thurm is opened") { [self] in
            run(["daemon", agentOn ? "uninstall-launchd" : "install-launchd"], title: "Daemon at Login")
        }
    }

    private func add(_ menu: NSMenu, _ title: String, on: Bool, tip: String?, _ block: @escaping () -> Void) {
        let item = BlockMenuItem(title, block: block)
        item.state = on ? .on : .off
        item.toolTip = tip
        menu.addItem(item)
    }

    // MARK: Command-line tool

    func installedCLILink() -> URL? {
        let target = cli.resolvingSymlinksInPath().path
        return cliLinkCandidates.first { link in
            (try? fm.destinationOfSymbolicLink(atPath: link.path)).map {
                URL(fileURLWithPath: $0, relativeTo: link.deletingLastPathComponent()).resolvingSymlinksInPath().path
            } == target
        }
    }

    /// `done` runs once the attempt has been reported, whether or not it linked the tool.
    func installCLI(done: @escaping () -> Void = {}) {
        defer { done() }
        let local = cliLinkCandidates[0]
        if fm.fileExists(atPath: local.deletingLastPathComponent().path) {
            if let existing = try? fm.attributesOfItem(atPath: local.path), existing[.type] as? FileAttributeType != .typeSymbolicLink {
                report("Command-Line Tool", "\(local.path) exists and is not a link; not replacing it.", ok: false)
                return
            }
            try? fm.removeItem(at: local)
            do {
                try fm.createSymbolicLink(at: local, withDestinationURL: cli)
                report("Command-Line Tool", "Linked \(local.path) → \(cli.path).", ok: true)
            } catch {
                report("Command-Line Tool", error.localizedDescription, ok: false)
            }
            return
        }
        let global = cliLinkCandidates[1]
        admin("mkdir -p /usr/local/bin && ln -sf \(quoted(cli.path)) \(quoted(global.path))",
              done: "Linked \(global.path) → \(cli.path).")
    }

    private func uninstallCLI(_ link: URL) {
        if fm.isWritableFile(atPath: link.deletingLastPathComponent().path) {
            try? fm.removeItem(at: link)
            report("Command-Line Tool", "Removed \(link.path).", ok: true)
        } else {
            admin("rm -f \(quoted(link.path))", done: "Removed \(link.path).")
        }
    }

    // MARK: Agent hooks

    private struct HookAgent {
        let kind: String
        let name: String
        let installed: Int
        let total: Int
        let present: Bool
        let path: String
    }

    private func hookStatus() -> [HookAgent] {
        guard let out = capture(["--json", "hooks", "status"]).output.data(using: .utf8),
              let list = try? JSONSerialization.jsonObject(with: out) as? [[String: Any]]
        else { return [] }
        return list.compactMap { d in
            guard let kind = d["agent"] as? String, let name = d["name"] as? String else { return nil }
            return HookAgent(kind: kind, name: name, installed: d["installed"] as? Int ?? 0,
                             total: d["total"] as? Int ?? 0, present: d["present"] as? Bool ?? false,
                             path: d["path"] as? String ?? "")
        }
    }

    private func toggleHooks(_ agent: HookAgent, install: Bool) {
        let alert = NSAlert()
        alert.messageText = install ? "Install \(agent.name) hooks?" : "Remove \(agent.name) hooks?"
        alert.informativeText = install
            ? "Adds Thurm's hooks to \(agent.path) (a backup of the current file is kept), so Thurm shows when "
                + "\(agent.name) is working, waiting for you or done. Running sessions pick them up after a restart."
            : "Removes Thurm's hooks from \(agent.path); your other hooks stay."
        alert.addButton(withTitle: install ? "Install" : "Remove")
        alert.addButton(withTitle: "Cancel")
        guard alert.runModal() == .alertFirstButtonReturn else { return }
        run(["hooks", install ? "install" : "uninstall", "--agent", agent.kind], title: "\(agent.name) Hooks")
    }

    // MARK: Agent skill

    private struct SkillTarget {
        let name: String
        let present: Bool
        let state: String
    }

    private func skillStatus() -> [SkillTarget] {
        guard let out = capture(["--json", "skill", "status"]).output.data(using: .utf8),
              let list = try? JSONSerialization.jsonObject(with: out) as? [[String: Any]]
        else { return [] }
        return list.compactMap { d in
            guard let name = d["name"] as? String else { return nil }
            return SkillTarget(name: name, present: d["present"] as? Bool ?? false,
                               state: d["state"] as? String ?? "")
        }
    }

    // MARK: Running things

    private func capture(_ args: [String]) -> (ok: Bool, output: String) {
        let p = Process()
        p.executableURL = cli
        p.arguments = args
        let pipe = Pipe()
        p.standardOutput = pipe
        p.standardError = pipe
        do { try p.run() } catch { return (false, error.localizedDescription) }
        let data = pipe.fileHandleForReading.readDataToEndOfFile()
        p.waitUntilExit()
        return (p.terminationStatus == 0, String(decoding: data, as: UTF8.self))
    }

    private func run(_ args: [String], title: String) {
        let (ok, out) = capture(args)
        report(title, out.trimmingCharacters(in: .whitespacesAndNewlines), ok: ok)
    }

    private func admin(_ script: String, done: String) {
        let source = "do shell script \"\(script.replacingOccurrences(of: "\\", with: "\\\\").replacingOccurrences(of: "\"", with: "\\\""))\" with administrator privileges"
        var error: NSDictionary?
        NSAppleScript(source: source)?.executeAndReturnError(&error)
        if let error {
            report("Command-Line Tool", error[NSAppleScript.errorMessage] as? String ?? "Failed.", ok: false)
        } else {
            report("Command-Line Tool", done, ok: true)
        }
    }

    private func quoted(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    private func report(_ title: String, _ message: String, ok: Bool) {
        if ok, let view = SessionManager.shared.currentController?.content.focusedView {
            view.showToast(message.isEmpty ? title : message, duration: 4)
            return
        }
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = message
        alert.alertStyle = ok ? .informational : .warning
        alert.runModal()
    }
}
