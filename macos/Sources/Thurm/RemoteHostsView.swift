import AppKit
import SwiftUI

/// Settings › Remotes: a row per `[[remote]]` host with its state and Details…, and Add Host….
/// Hosts are changed through `RemoteHosts`: in config.toml, or in settings.toml when config.toml
/// can't be written or settings.toml already sets them.
struct RemoteHostRows: View {
    let item: SettingItem
    @ObservedObject var store: SettingsStore
    @ObservedObject private var remotes = RemoteHosts.shared

    private var hosts: [RemoteHostEntry] { RemoteHostEntry.entries(item.value) }
    private var readOnly: Bool { store.doc?.configReadOnly == true }

    var body: some View {
        if item.isOverridden {
            OverlayHostsNote(item: item, store: store)
        }
        if hosts.isEmpty {
            Text("No hosts yet. Add Host\u{2026} connects to one over ssh.")
                .foregroundStyle(.secondary)
        }
        ForEach(hosts) { host in
            HostRow(host: host, remotes: remotes)
        }
        HStack {
            if readOnly && !item.isOverridden {
                Text("config.toml is managed by another tool: hosts added here go in settings.toml.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button("Pair iPhone\u{2026}") { remotes.sheet = .pairPhone }
            Button("Add Host\u{2026}") { remotes.sheet = .add }
        }
    }
}

extension View {
    /// The Add Host and host details sheets of Settings › Remotes.
    func remoteHostSheets(store: SettingsStore) -> some View {
        modifier(RemoteHostSheets(store: store))
    }
}

private struct RemoteHostSheets: ViewModifier {
    let store: SettingsStore
    @ObservedObject private var remotes = RemoteHosts.shared

    func body(content: Content) -> some View {
        content.sheet(item: $remotes.sheet) { sheet in
            switch sheet {
            case .add: AddHostSheet(remotes: remotes)
            case .details(let host): HostDetails(name: host, store: store, remotes: remotes)
            case .pairPhone: PairPhoneSheet(remotes: remotes)
            }
        }
    }
}

/// settings.toml sets `remote`, which replaces config.toml's hosts; edits here go there too.
private struct OverlayHostsNote: View {
    let item: SettingItem
    let store: SettingsStore

    var body: some View {
        HStack(spacing: 8) {
            Image(systemName: "info.circle")
                .foregroundStyle(.secondary)
            Text("These hosts are in settings.toml, in place of config.toml's: Reset to go back to those.")
                .font(.callout)
            Spacer(minLength: 8)
            Button("Edit in settings.toml\u{2026}") { store.editTable(item) }
                .controlSize(.small)
            ResetSlot(item: item, store: store)
        }
    }
}

private struct HostRow: View {
    let host: RemoteHostEntry
    @ObservedObject var remotes: RemoteHosts

    var body: some View {
        LabeledContent {
            HStack(spacing: 10) {
                Text(RemoteHostState.label(host.name, busy: remotes.busy[host.name]))
                    .foregroundStyle(.secondary)
                Button("Details\u{2026}") { remotes.showDetails(host.name) }
            }
        } label: {
            HStack(spacing: 10) {
                StatusDot(host: host.name)
                VStack(alignment: .leading, spacing: 2) {
                    Text(host.name)
                    Text(host.target)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
            }
        }
    }
}

/// How a host's tunnel state reads, and its color.
private enum RemoteHostState {
    static func label(_ host: HostId, busy: String?) -> String {
        if let busy { return busy }
        let label = Remotes.shared.phaseLabel(host)
        return label.prefix(1).uppercased() + label.dropFirst()
    }

    static func color(_ host: HostId) -> Color {
        if Core.shared.isConnected(host) { return .green }
        switch Remotes.shared.statuses[host]?.phase {
        case "needs_attention", "upgrade_needed", "not_installed": return .orange
        case "disabled", "stopped": return .gray
        default: return .yellow
        }
    }
}

private struct StatusDot: View {
    let host: HostId

    var body: some View {
        Circle()
            .fill(RemoteHostState.color(host))
            .frame(width: 8, height: 8)
    }
}

/// One host: its state, settings, the checks of `thurm remote doctor` with their fixes, and
/// Remove Host….
private struct HostDetails: View {
    let name: HostId
    @ObservedObject var store: SettingsStore
    @ObservedObject var remotes: RemoteHosts
    @State private var target = ""
    @State private var failure: String?
    @State private var confirmingRemove = false
    @FocusState private var targetFocused: Bool

    private var host: RemoteHostEntry? {
        RemoteHostEntry.entries(store.setting("remote")?.value).first { $0.name == name }
    }
    private var status: RemoteStatus? { Remotes.shared.statuses[name] }

    var body: some View {
        VStack(spacing: 0) {
            HStack(spacing: 10) {
                StatusDot(host: name)
                VStack(alignment: .leading, spacing: 2) {
                    Text(name)
                        .font(.title2.weight(.semibold))
                    Text(RemoteHostState.label(name, busy: remotes.busy[name]))
                        .foregroundStyle(.secondary)
                }
                Spacer()
            }
            .padding(.horizontal, 20)
            .padding(.top, 16)
            Form {
                if let message = status?.message, !message.isEmpty {
                    Section {
                        Text(message)
                            .font(.callout)
                            .textSelection(.enabled)
                    }
                }
                if let host {
                    Section("Connection") {
                        Toggle("Connect", isOn: Binding(
                            get: { host.enabled },
                            set: { save("enabled", $0) }
                        ))
                        .help("Off: Thurm keeps the host but doesn't connect to it.")
                        LabeledContent("SSH target") {
                            TextField("", text: $target, prompt: Text("me@devbox"))
                                .textFieldStyle(.roundedBorder)
                                .frame(width: 260)
                                .focused($targetFocused)
                                .onSubmit(saveTarget)
                                .onChange(of: targetFocused) { _, focused in
                                    if !focused { saveTarget() }
                                }
                        }
                        Picker("Clipboard reads", selection: Binding(
                            get: { host.clipboardRead },
                            set: { save("clipboard_read", $0) }
                        )) {
                            Text("Ask every time").tag("ask")
                            Text("Always allow").tag("always")
                            Text("Never").tag("never")
                        }
                        .help("Whether programs on this host may read this Mac's clipboard (OSC 52).")
                    }
                }
                if let status, status.remoteBuild != nil || status.os != nil {
                    Section("Host") {
                        if let build = status.remoteBuild {
                            LabeledContent("Build") {
                                Text(build)
                                    .font(.system(.body, design: .monospaced))
                                    .textSelection(.enabled)
                            }
                        }
                        if let os = status.os {
                            LabeledContent("Platform", value: [os, status.arch].compactMap { $0 }.joined(separator: " "))
                        }
                    }
                }
                Section {
                    checks
                } header: {
                    HStack {
                        Text("Checks")
                        Spacer()
                        if remotes.checking.contains(name) {
                            ProgressView()
                                .controlSize(.small)
                        }
                    }
                }
            }
            .formStyle(.grouped)
            Divider()
            HStack {
                Button("Remove Host\u{2026}", role: .destructive) { confirmingRemove = true }
                    .disabled(host == nil)
                Spacer()
                Button("Reconnect") { remotes.retry(name) }
                    .disabled(remotes.busy[name] != nil)
                Button("Check Again") { remotes.recheck(name) }
                    .disabled(remotes.checking.contains(name) || remotes.fixing[name] != nil)
                Button("Done") { remotes.sheet = nil }
                    .keyboardShortcut(.defaultAction)
            }
            .padding(16)
        }
        .frame(width: 620, height: 640)
        .onAppear { target = host?.target ?? "" }
        .onChange(of: host?.target) { _, new in target = new ?? "" }
        .alert("Remove \(name)?", isPresented: $confirmingRemove) {
            Button("Remove", role: .destructive) {
                remotes.remove(name) { failure = $0 }
            }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("Thurm stops connecting to it. Its daemon and panes keep running there.")
        }
        .alert("The host was not changed", isPresented: Binding(
            get: { failure != nil },
            set: { if !$0 { failure = nil } }
        )) {
            Button("OK") {}
        } message: {
            Text(failure ?? "")
        }
    }

    @ViewBuilder private var checks: some View {
        let report = remotes.reports[name]
        if let error = report?["error"] as? String {
            CheckRow(host: name, check: ["id": "ssh", "title": "ssh", "state": "fail", "detail": error],
                     remotes: remotes)
        } else if let list = report?["checks"] as? [[String: Any]] {
            ForEach(Array(list.enumerated()), id: \.offset) { _, check in
                CheckRow(host: name, check: check, remotes: remotes)
            }
        } else {
            Text(remotes.checking.contains(name) ? "Checking \(name)\u{2026}" : "Not checked yet.")
                .foregroundStyle(.secondary)
        }
    }

    private func save(_ key: String, _ value: Any) {
        remotes.set(name, key, value) { failure = $0 }
    }

    private func saveTarget() {
        let trimmed = target.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let host, trimmed != host.target else { return }
        guard !trimmed.isEmpty else {
            NSSound.beep()
            target = host.target
            return
        }
        save("host", trimmed)
    }
}

/// One check of `doctor`: its state, what it found, and its fix.
private struct CheckRow: View {
    let host: HostId
    let check: [String: Any]
    @ObservedObject var remotes: RemoteHosts

    private var id: String { jsonString(check["id"]) ?? "" }
    private var title: String { jsonString(check["title"]) ?? id }
    private var state: String { jsonString(check["state"]) ?? "skip" }
    private var fix: [String: Any]? { check["fix"] as? [String: Any] }
    private var terminal: String? { jsonString(check["terminal"]) }

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 10) {
            Image(systemName: symbol)
                .foregroundStyle(color)
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .foregroundStyle(state == "skip" ? .secondary : .primary)
                if let detail = jsonString(check["detail"]), !detail.isEmpty {
                    Text(detail)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            Spacer(minLength: 8)
            buttons
        }
    }

    @ViewBuilder private var buttons: some View {
        if remotes.fixing[host] == id {
            Button("Fixing\u{2026}") {}
                .disabled(true)
        } else if state == "fail" || state == "warn" {
            if let fix, let label = jsonString(fix["label"]) {
                Button(label) {
                    remotes.runFix(host, id: id, title: title, confirm: jsonString(fix["confirm"]),
                                   terminal: terminal)
                }
                .disabled(remotes.fixing[host] != nil)
            }
            if let terminal {
                Button(fix == nil ? "Run in a Tab\u{2026}" : "In a Tab\u{2026}") {
                    remotes.runInTab(host, script: terminal, title: title)
                }
            }
        }
    }

    private var symbol: String {
        switch state {
        case "ok": return "checkmark.circle.fill"
        case "warn": return "exclamationmark.triangle.fill"
        case "fail": return "xmark.circle.fill"
        default: return "minus.circle"
        }
    }

    private var color: Color {
        switch state {
        case "ok": return .green
        case "warn": return .orange
        case "fail": return .red
        default: return .secondary
        }
    }
}

/// Add Host…: a name and an ssh target, checked over ssh before the host is added.
private struct AddHostSheet: View {
    @ObservedObject var remotes: RemoteHosts
    @State private var name = ""
    @State private var target = ""
    @State private var failure: String?
    @State private var connecting = false

    private var trimmedName: String { name.trimmingCharacters(in: .whitespacesAndNewlines) }
    private var trimmedTarget: String { target.trimmingCharacters(in: .whitespacesAndNewlines) }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Add a Host")
                .font(.headline)
            Text("Thurm connects with the system ssh: the host needs a key that works without a prompt "
                + "(an agent is fine). Connect once with ssh in a terminal to accept its host key.")
                .font(.callout)
                .foregroundStyle(.secondary)
                .fixedSize(horizontal: false, vertical: true)
            Form {
                TextField("Name", text: $name, prompt: Text("devbox"))
                TextField("SSH target", text: $target, prompt: Text("me@devbox, a Host alias, or ssh://me@devbox:2222"))
            }
            .disabled(connecting)
            if let failure {
                Label {
                    Text(failure)
                        .textSelection(.enabled)
                        .fixedSize(horizontal: false, vertical: true)
                } icon: {
                    Image(systemName: "exclamationmark.triangle.fill")
                        .foregroundStyle(.orange)
                }
                .font(.callout)
            }
            HStack(spacing: 8) {
                if connecting {
                    ProgressView()
                        .controlSize(.small)
                    Text("Connecting to \(trimmedTarget)\u{2026}")
                        .foregroundStyle(.secondary)
                }
                Spacer()
                Button("Cancel") { remotes.sheet = nil }
                    .keyboardShortcut(.cancelAction)
                Button("Add", action: add)
                    .keyboardShortcut(.defaultAction)
                    .disabled(connecting || trimmedName.isEmpty || trimmedTarget.isEmpty)
            }
        }
        .padding(20)
        .frame(width: 480)
    }

    private func add() {
        let name = trimmedName
        connecting = true
        failure = nil
        remotes.add(name: name, target: trimmedTarget) { error in
            connecting = false
            if let error {
                failure = error
            } else {
                remotes.showDetails(name)
            }
        }
    }
}
