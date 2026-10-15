import AppKit
import SwiftUI

/// What the daemon reports of serving over Tailscale (`TailscaleStatus` in thurm-proto).
struct TailscaleStatus: Equatable {
    /// off, starting, needs_login, needs_machine_auth, running, stopped or error.
    var state: String
    var error: String?
    var hostname: String
    var dnsName: String?
    var authURL: URL?
    var account: String?
    var tailnet: String?
    /// `wss://<dns name>/v1/connect`, once the node serves HTTPS.
    var url: String?
    var allowTailnetMembers: Bool
    var tags: [String]
    /// unavailable, magic_dns_off, certs_off, provisioning or ready.
    var https: String
    var httpsError: String?
    var hostURLs: [String]
    var serveSocket: String?

    init?(json: Any?) {
        guard let d = json as? [String: Any], let state = jsonString(d["state"]) else { return nil }
        self.state = state
        error = jsonString(d["error"])
        hostname = jsonString(d["hostname"]) ?? ""
        dnsName = jsonString(d["dns_name"])
        authURL = jsonString(d["auth_url"]).flatMap(URL.init(string:))
        let account = d["account"] as? [String: Any]
        self.account = jsonString(account?["login_name"])
        tailnet = jsonString(d["tailnet_name"])
        url = jsonString(d["url"])
        allowTailnetMembers = jsonBool(d["allow_tailnet_members"]) ?? false
        tags = d["tags"] as? [String] ?? []
        let https = d["https"] as? [String: Any]
        self.https = jsonString(https?["state"]) ?? "unavailable"
        httpsError = jsonString(https?["error"])
        hostURLs = d["host_urls"] as? [String] ?? []
        serveSocket = jsonString(d["serve_socket"])
    }

    var label: String {
        switch state {
        case "off": return "Off"
        case "starting": return "Starting\u{2026}"
        case "needs_login": return "Needs you to sign in"
        case "needs_machine_auth": return "Waiting for a tailnet admin to approve it"
        case "running": return https == "provisioning" ? "Getting a certificate\u{2026}" : "Serving"
        case "stopped": return "Stopped"
        default: return "Error"
        }
    }

    var color: Color {
        switch state {
        case "running": return https == "ready" ? .green : .yellow
        case "starting": return .yellow
        case "needs_login", "needs_machine_auth", "error": return .orange
        default: return .gray
        }
    }

    /// The phone connects here: the node's own address, once it serves HTTPS.
    var pairingURL: String? { state == "running" && https == "ready" ? url : nil }
}

/// This Mac's daemon serving over Tailscale: its status (from `TailscaleStatusChanged`), the
/// sign-in and sign-out requests, and the notifications when it needs something.
final class TailscaleService: ObservableObject {
    static let shared = TailscaleService()

    @Published private(set) var status: TailscaleStatus?
    /// Why the daemon could not be asked: an older one, or none connected.
    @Published private(set) var unavailable: String?
    /// The request under way ("Signing out…").
    @Published private(set) var busy: String?
    @Published var failure: String?

    /// What was last notified, so a status repeated by the daemon notifies once.
    private var notified: String?

    func refresh() {
        Core.shared.requestAsync(object: "TailscaleStatus", timeout: 10) { [weak self] resp in
            self?.answered(resp)
        }
    }

    /// `TailscaleStatusChanged` from this Mac's daemon.
    func update(_ payload: Any?) {
        guard let status = TailscaleStatus(json: payload) else { return }
        unavailable = nil
        set(status)
    }

    func signIn() {
        if let url = status?.authURL {
            NSWorkspace.shared.open(url)
            return
        }
        run("TailscaleLogin", busy: "Asking Tailscale for a sign-in link\u{2026}") { status in
            if let url = status.authURL { NSWorkspace.shared.open(url) }
        }
    }

    func signOut() {
        run("TailscaleLogout", busy: "Signing out\u{2026}")
    }

    private func run(_ request: Any, busy label: String, then: ((TailscaleStatus) -> Void)? = nil) {
        busy = label
        failure = nil
        // Reaching Tailscale's coordination server can take a while.
        Core.shared.requestAsync(object: request, timeout: 40) { [weak self] resp in
            guard let self else { return }
            busy = nil
            if let status = answered(resp) { then?(status) }
        }
    }

    @discardableResult
    private func answered(_ resp: Any?) -> TailscaleStatus? {
        guard let resp else {
            unavailable = "Thurm's daemon is not connected."
            return nil
        }
        if let error = (resp as? [String: Any])?["error"] as? String {
            // A daemon from before Tailscale doesn't know the request.
            if error.contains("does not know the request") {
                unavailable = "This Mac's session daemon is older than the app and can't serve over Tailscale "
                    + "until it is upgraded."
            } else {
                failure = error
            }
            return nil
        }
        guard let v = JSON.variant(resp), v.name == "TailscaleStatus",
              let status = TailscaleStatus(json: v.payload)
        else { return nil }
        unavailable = nil
        set(status)
        return status
    }

    private func set(_ new: TailscaleStatus) {
        guard new != status else { return }
        status = new
        notify(new)
    }

    /// Posts what needs the user: a sign-in, an admin's approval, or a stop on an error. Each
    /// once until the state changes.
    private func notify(_ s: TailscaleStatus) {
        let key: String?
        switch s.state {
        case "needs_login" where s.authURL != nil: key = "needs_login"
        case "needs_machine_auth": key = "needs_machine_auth"
        case "error": key = "error:\(s.error ?? "")"
        default: key = nil
        }
        defer { notified = key }
        guard let key, key != notified, SessionManager.shared.config.notificationsEnabled else { return }
        switch s.state {
        case "needs_login":
            Notifications.shared.post(id: "tailscale", title: "Sign in to Tailscale",
                                      body: "Thurm needs you to sign in before your devices can reach \(s.hostname) over the tailnet.",
                                      url: s.authURL)
        case "needs_machine_auth":
            Notifications.shared.post(id: "tailscale", title: "Approve \(s.hostname) in Tailscale",
                                      body: "Your tailnet requires an admin to approve new devices.",
                                      url: URL(string: "https://login.tailscale.com/admin/machines"))
        default:
            Notifications.shared.post(id: "tailscale", title: "Tailscale stopped",
                                      body: s.error ?? "Thurm stopped serving over Tailscale.", url: nil)
        }
    }
}

/// Settings › Remotes › Tailscale: the Serve over Tailscale switch (`tailscale.enabled`), then
/// what the daemon reports: its state, address, account, and Sign In / Sign Out.
struct TailscaleRows: View {
    let item: SettingItem
    @ObservedObject var store: SettingsStore
    @ObservedObject private var service = TailscaleService.shared
    @State private var confirmingSignOut = false

    var body: some View {
        SettingRow(item: item, store: store)
        if let reason = service.unavailable {
            Text(reason)
                .font(.callout)
                .foregroundStyle(.secondary)
        } else if let s = service.status {
            statusRows(s)
        }
        if let failure = service.failure {
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
    }

    @ViewBuilder private func statusRows(_ s: TailscaleStatus) -> some View {
        LabeledContent {
            HStack(spacing: 8) {
                if let busy = service.busy {
                    ProgressView()
                        .controlSize(.small)
                    Text(busy)
                        .foregroundStyle(.secondary)
                } else {
                    buttons(s)
                }
            }
        } label: {
            HStack(spacing: 10) {
                Circle()
                    .fill(s.color)
                    .frame(width: 8, height: 8)
                VStack(alignment: .leading, spacing: 2) {
                    Text(s.label)
                    if let detail = detail(s) {
                        Text(detail)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                            .textSelection(.enabled)
                            .fixedSize(horizontal: false, vertical: true)
                    }
                }
            }
        }
        .alert("Sign out of Tailscale?", isPresented: $confirmingSignOut) {
            Button("Sign Out", role: .destructive) { service.signOut() }
            Button("Cancel", role: .cancel) {}
        } message: {
            Text("\(s.hostname) leaves your tailnet and your devices can no longer reach Thurm through it. "
                + "Turning it back on asks you to sign in again.")
        }
        if let address = s.dnsName, s.state == "running" {
            LabeledContent("Address") {
                HStack(spacing: 8) {
                    Text(address)
                        .textSelection(.enabled)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Button {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(address, forType: .string)
                    } label: {
                        Image(systemName: "doc.on.doc")
                    }
                    .buttonStyle(.borderless)
                    .help("Copy Tailscale Address")
                }
            }
        }
        if let account = s.account, s.state == "running" {
            LabeledContent("Account", value: account)
        }
        if s.https == "magic_dns_off" || s.https == "certs_off" {
            LabeledContent {
                Button("Open DNS Settings") {
                    NSWorkspace.shared.open(URL(string: "https://login.tailscale.com/admin/dns")!)
                }
            } label: {
                Text(s.https == "magic_dns_off"
                     ? "Turn on MagicDNS for your tailnet so the phone can reach this Mac by name."
                     : "Turn on HTTPS certificates for your tailnet so the phone can connect securely.")
                    .font(.callout)
                    .fixedSize(horizontal: false, vertical: true)
            }
        }
        ForEach(s.hostURLs, id: \.self) { url in
            LabeledContent("Through this Mac's Tailscale", value: url)
        }
        if let sock = s.serveSocket {
            LabeledContent("For tailscale serve") {
                Text("tailscale serve --bg unix:\(sock)")
                    .font(.system(.caption, design: .monospaced))
                    .textSelection(.enabled)
            }
        }
    }

    @ViewBuilder private func buttons(_ s: TailscaleStatus) -> some View {
        switch s.state {
        case "needs_login":
            Button(s.authURL == nil ? "Sign In\u{2026}" : "Open Sign-In Page") { service.signIn() }
        case "needs_machine_auth":
            Button("Open Machines") {
                NSWorkspace.shared.open(URL(string: "https://login.tailscale.com/admin/machines")!)
            }
        case "running":
            Button("Sign Out\u{2026}") { confirmingSignOut = true }
        default:
            EmptyView()
        }
    }

    private func detail(_ s: TailscaleStatus) -> String? {
        switch s.state {
        case "running":
            return s.allowTailnetMembers ? "Every member of the tailnet may connect." :
                "Only \(s.account ?? "the account that signed it in") may connect."
        case "needs_login":
            return "Sign in once; it stays signed in when turned off."
        case "error", "stopped":
            return s.error
        default:
            return s.httpsError
        }
    }
}
