import AppKit
import CoreImage
import CryptoKit
import Network
import SwiftUI

/// Settings › Remotes › Pair iPhone: a QR code the iOS app scans (this Mac: host, user, host
/// keys, push relay), and the device keys it hands back, kept in `~/.ssh/authorized_keys`.
enum PhonePairing {
    /// Comment marking a device key in authorized_keys.
    static let commentPrefix = "thurm-ios:"
    /// A device may run remote commands (probe, start the daemon) and reach the daemon socket:
    /// no terminal, agent or X11 forwarding.
    static let keyOptions = "restrict,port-forwarding"

    /// `SHA256:<base64>` fingerprints of this Mac's ssh host keys (`ssh-keygen -lf`).
    static func hostKeyFingerprints(dir: URL = URL(fileURLWithPath: "/etc/ssh")) -> [String] {
        ["ssh_host_ed25519_key.pub", "ssh_host_ecdsa_key.pub", "ssh_host_rsa_key.pub"].compactMap { name in
            guard let line = try? String(contentsOf: dir.appendingPathComponent(name), encoding: .utf8) else {
                return nil
            }
            return fingerprint(publicKeyLine: line)
        }
    }

    static func fingerprint(publicKeyLine line: String) -> String? {
        let parts = line.split(separator: " ")
        guard parts.count >= 2, let blob = Data(base64Encoded: String(parts[1])) else { return nil }
        let digest = Data(SHA256.hash(data: blob)).base64EncodedString()
        return "SHA256:" + digest.replacingOccurrences(of: "=", with: "")
    }

    /// A `[[remote]]` host for the code, as `pairing_hosts` describes it: its address, user and
    /// port from `ssh -G`, its host keys from this Mac's known hosts.
    struct RemoteHost: Identifiable, Equatable {
        var name: String
        /// Where the phone connects: `ssh -G`'s hostname until edited.
        var address: String
        var port: Int
        var user: String
        var hostKeys: [String]
        var socket: String?
        /// The ProxyJump or ProxyCommand the Mac uses for it (the phone can't).
        var proxy: String?
        /// Why it can't be in the code.
        var error: String?
        /// Checked in the sheet.
        var included: Bool
        var id: String { name }

        init(name: String, address: String, port: Int = 22, user: String, hostKeys: [String],
             socket: String? = nil, proxy: String? = nil, error: String? = nil) {
            self.name = name
            self.address = address
            self.port = port
            self.user = user
            self.hostKeys = hostKeys
            self.socket = socket
            self.proxy = proxy
            self.error = error
            included = error == nil && !hostKeys.isEmpty
        }

        init?(json: Any?) {
            guard let d = json as? [String: Any], let name = jsonString(d["name"]) else { return nil }
            self.init(name: name, address: jsonString(d["host"]) ?? "", port: jsonInt(d["port"]) ?? 22,
                      user: jsonString(d["user"]) ?? "", hostKeys: d["host_keys"] as? [String] ?? [],
                      socket: jsonString(d["socket"]), proxy: jsonString(d["proxy"]),
                      error: jsonString(d["error"]))
        }

        /// Checked, and something the phone can connect to and trust.
        var inCode: Bool {
            included && error == nil && !hostKeys.isEmpty
                && !address.trimmingCharacters(in: .whitespaces).isEmpty
        }

        /// Its entry in the code (`PairedHost` in the iOS app).
        var payload: [String: Any] {
            var d: [String: Any] = ["name": name, "host": address.trimmingCharacters(in: .whitespaces),
                                    "port": port, "user": user, "host_keys": hostKeys]
            if let socket { d["socket"] = socket }
            return d
        }
    }

    /// The remote hosts the code carries, and so the ones Add Device Key authorizes the phone on:
    /// a host left out of the code never gets its key.
    static func inCode(_ remotes: [RemoteHost]) -> [RemoteHost] {
        remotes.filter(\.inCode)
    }

    /// What the QR code carries (see `PairingPayload` in the iOS app). Remote hosts not checked
    /// (or not usable) are left out. `tailnet` is this Mac's daemon over Tailscale
    /// (`wss://…/v1/connect`): an app that knows it connects there first, and over ssh when it
    /// can't; older apps read only the ssh fields.
    static func payload(name: String, host: String, port: Int, user: String, hostKeys: [String],
                        relay: String?, code: String?, remotes: [RemoteHost] = [], tailnet: String? = nil) -> String {
        var d: [String: Any] = ["v": 1, "name": name, "host": host, "port": port, "user": user,
                                "host_keys": hostKeys]
        if let tailnet, !tailnet.isEmpty { d["tailnet"] = ["url": tailnet] }
        if let relay, !relay.isEmpty { d["relay"] = relay }
        if let code, !code.isEmpty { d["code"] = code }
        let remotes = inCode(remotes)
        if !remotes.isEmpty { d["remotes"] = remotes.map(\.payload) }
        let json = (try? JSONSerialization.data(withJSONObject: d, options: [.sortedKeys])) ?? Data()
        let b64 = json.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return "thurm://pair?d=" + b64
    }

    /// A device key line from the iOS app, checked: only its P-256 keys with Thurm's comment, on
    /// one line (a line break would add a second, unrestricted key to authorized_keys).
    static func validDeviceKey(_ text: String) -> (key: String, device: String)? {
        let line = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !line.unicodeScalars.contains(where: { CharacterSet.controlCharacters.contains($0) }) else {
            return nil
        }
        let parts = line.split(separator: " ")
        guard parts.count == 3, parts[0] == "ecdsa-sha2-nistp256",
              let blob = Data(base64Encoded: String(parts[1])), isP256PublicKey(blob),
              parts[2].hasPrefix(commentPrefix), parts[2].count > commentPrefix.count
        else { return nil }
        return ("\(parts[0]) \(parts[1])", String(parts[2].dropFirst(commentPrefix.count)))
    }

    /// An ssh `ecdsa-sha2-nistp256` public key blob: the algorithm, the curve and a point on it.
    static func isP256PublicKey(_ blob: Data) -> Bool {
        var rest = blob[...]
        func string() -> Data? {
            guard rest.count >= 4 else { return nil }
            let length = rest.prefix(4).reduce(0) { $0 << 8 | Int($1) }
            rest = rest.dropFirst(4)
            guard rest.count >= length else { return nil }
            defer { rest = rest.dropFirst(length) }
            return Data(rest.prefix(length))
        }
        guard string() == Data("ecdsa-sha2-nistp256".utf8), string() == Data("nistp256".utf8),
              let point = string(), rest.isEmpty
        else { return false }
        return (try? P256.Signing.PublicKey(x963Representation: point)) != nil
    }

    static var authorizedKeys: URL {
        FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".ssh/authorized_keys")
    }

    struct Device: Identifiable, Equatable {
        var name: String
        var line: String
        var id: String { line }
    }

    static func devices(in file: URL = authorizedKeys) -> [Device] {
        guard let text = try? String(contentsOf: file, encoding: .utf8) else { return [] }
        return text.split(separator: "\n").compactMap { line in
            deviceName(ofLine: line).map { Device(name: $0, line: String(line)) }
        }
    }

    /// The device of a line Thurm wrote (exactly `restrict,port-forwarding ecdsa-sha2-nistp256
    /// <key> thurm-ios:<device>`); nil for any other line, even one ending the same way.
    static func deviceName(ofLine line: Substring) -> String? {
        let parts = line.split(separator: " ", omittingEmptySubsequences: false)
        guard parts.count == 4, parts[0] == keyOptions, parts[1] == "ecdsa-sha2-nistp256", !parts[2].isEmpty,
              parts[3].hasPrefix(commentPrefix), parts[3].count > commentPrefix.count
        else { return nil }
        return String(parts[3].dropFirst(commentPrefix.count))
    }

    /// Adds (or replaces, by device name) a device key. Creates ~/.ssh (0700) and the file (0600).
    static func add(key: String, device: String, to file: URL = authorizedKeys) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true,
                               attributes: [.posixPermissions: 0o700])
        var lines = try lines(of: file)
        lines.removeAll { deviceName(ofLine: $0[...]) == device }
        lines.append("\(keyOptions) \(key) \(commentPrefix)\(device)")
        try write(lines, to: file)
    }

    static func revoke(_ device: Device, in file: URL = authorizedKeys) throws {
        let lines = try lines(of: file)
        guard lines.contains(device.line) else { return }
        try write(lines.filter { $0 != device.line }, to: file)
    }

    /// The file's lines, none when it does not exist. Any other read error is thrown: writing
    /// back what could not be read would drop every other key.
    private static func lines(of file: URL) throws -> [String] {
        let text: String
        do {
            text = try String(contentsOf: file, encoding: .utf8)
        } catch CocoaError.fileReadNoSuchFile {
            return []
        }
        var lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        if lines.last == "" { lines.removeLast() }
        return lines
    }

    /// Replaces the file in one step: the new contents go to a temporary file beside it, which gets
    /// its permissions before `rename(2)` puts it in place. Whatever fails before that leaves the
    /// file as it was, so an error means no key was added or removed.
    private static func write(_ lines: [String], to file: URL) throws {
        let fm = FileManager.default
        let tmp = file.deletingLastPathComponent()
            .appendingPathComponent(".\(file.lastPathComponent).thurm-\(UUID().uuidString)")
        do {
            try Data((lines.joined(separator: "\n") + "\n").utf8).write(to: tmp, options: .withoutOverwriting)
            try fm.setAttributes([.posixPermissions: 0o600], ofItemAtPath: tmp.path)
            guard rename(tmp.path, file.path) == 0 else {
                throw POSIXError(POSIXErrorCode(rawValue: errno) ?? .EIO)
            }
        } catch {
            try? fm.removeItem(at: tmp)
            throw error
        }
    }

    /// This Mac's name on the tailnet (MagicDNS), else its Bonjour name.
    static func defaultHostName() -> String {
        for cli in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale",
                    "/opt/homebrew/bin/tailscale"] where FileManager.default.isExecutableFile(atPath: cli) {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: cli)
            p.arguments = ["status", "--json"]
            // Without a terminal, the Tailscale app's binary starts its GUI instead of the CLI.
            var env = ProcessInfo.processInfo.environment
            env["TAILSCALE_BE_CLI"] = "1"
            p.environment = env
            let out = Pipe()
            p.standardOutput = out
            p.standardError = FileHandle.nullDevice
            guard (try? p.run()) != nil else { continue }
            let data = out.fileHandleForReading.readDataToEndOfFile()
            p.waitUntilExit()
            if let d = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
               let me = d["Self"] as? [String: Any], let dns = me["DNSName"] as? String, !dns.isEmpty {
                return dns.hasSuffix(".") ? String(dns.dropLast()) : dns
            }
        }
        let local = Host.current().name ?? ProcessInfo.processInfo.hostName
        return local.hasSuffix(".local") ? local : local + ".local"
    }

    /// Each remote host adds about 150 bytes: "M" keeps the code readable up to about six,
    /// "L" beyond keeps it from getting too dense to scan.
    static func correctionLevel(remotes: Int) -> String {
        remotes > 6 ? "L" : "M"
    }

    static func qrImage(_ text: String, remotes: Int = 0) -> NSImage? {
        guard let filter = CIFilter(name: "CIQRCodeGenerator") else { return nil }
        filter.setValue(Data(text.utf8), forKey: "inputMessage")
        filter.setValue(correctionLevel(remotes: remotes), forKey: "inputCorrectionLevel")
        guard let image = filter.outputImage?.transformed(by: CGAffineTransform(scaleX: 8, y: 8)) else {
            return nil
        }
        let rep = NSCIImageRep(ciImage: image)
        let out = NSImage(size: rep.size)
        out.addRepresentation(rep)
        return out
    }
}

struct PairPhoneSheet: View {
    @ObservedObject var remotes: RemoteHosts
    @State private var host = ""
    @State private var port = 22
    @State private var deviceKey = ""
    @State private var devices = PhonePairing.devices()
    @State private var message: String?
    @State private var remoteLogin: Bool?
    /// The `[[remote]]` hosts (`pairing_hosts`), with what the sheet changed.
    @State private var remoteHosts: [PhonePairing.RemoteHost] = []
    @State private var loadingHosts = true
    /// Devices with a key on each remote host.
    @State private var remoteDevices: [String: [String]] = [:]
    /// What adding the device key did on each remote host.
    @State private var keyResults: [String: KeyResult] = [:]
    private let hostKeys = PhonePairing.hostKeyFingerprints()
    private let user = NSUserName()
    private let push = SessionManager.shared.config.push
    @ObservedObject private var tailscale = TailscaleService.shared

    private enum KeyResult: Equatable {
        case adding
        case added
        case failed(String)
        /// The host's authorized_keys is managed elsewhere: what to add there by hand.
        case manual(String, text: String)
    }

    private struct RemoteDevice: Hashable {
        var host: String
        var name: String
    }

    private var link: String {
        PhonePairing.payload(name: Host.current().localizedName ?? host, host: host, port: port,
                             user: user, hostKeys: hostKeys, relay: push.relay, code: push.code,
                             remotes: remoteHosts, tailnet: tailscale.status?.pairingURL)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Pair iPhone or iPad").font(.title2.bold())
            if let url = tailscale.status?.pairingURL {
                Label("The app connects over Tailscale (\(URL(string: url)?.host ?? url)), with ssh as the fallback.",
                      systemImage: "network")
                    .foregroundStyle(.secondary)
            }
            if remoteLogin == false {
                Label(tailscale.status?.pairingURL == nil
                      ? "Remote Login is off. Turn it on in System Settings › General › Sharing so the app can connect."
                      : "Remote Login is off: the app has no fallback when Tailscale is off on the phone.",
                      systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
            if hostKeys.isEmpty {
                Label("This Mac's ssh host keys could not be read (/etc/ssh).", systemImage: "xmark.octagon")
                    .foregroundStyle(.red)
            }
            HStack(alignment: .top, spacing: 20) {
                if let image = PhonePairing.qrImage(link, remotes: remoteHosts.filter(\.inCode).count),
                   !host.isEmpty, !hostKeys.isEmpty {
                    Image(nsImage: image)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: 220, height: 220)
                        .accessibilityLabel("Pairing code")
                }
                VStack(alignment: .leading, spacing: 8) {
                    Text("1. In the Thurm app on your iPhone, tap + and scan this code.")
                        .fixedSize(horizontal: false, vertical: true)
                    Form {
                        TextField("Address", text: $host)
                        TextField("Port", value: $port, format: .number.grouping(.never))
                        LabeledContent("User", value: user)
                    }
                    .formStyle(.columns)
                    Text("Use an address the phone can reach (a Tailscale name works anywhere).")
                        .font(.caption).foregroundStyle(.secondary)
                    Button("Copy Pairing Link") {
                        NSPasteboard.general.clearContents()
                        NSPasteboard.general.setString(link, forType: .string)
                    }
                    // Without the host keys the phone can never trust this Mac.
                    .disabled(host.isEmpty || hostKeys.isEmpty)
                }
            }
            remoteHostRows
            Divider()
            Text("2. Paste the device key the app shows (Universal Clipboard or AirDrop).")
            HStack {
                TextField("ecdsa-sha2-nistp256 AAAA… thurm-ios:iPhone", text: $deviceKey)
                    .font(.system(.caption, design: .monospaced))
                Button("Add Device Key") { addKey() }
                    .disabled(PhonePairing.validDeviceKey(deviceKey) == nil)
            }
            if let message {
                Text(message).font(.caption).foregroundStyle(.secondary)
            }
            keyResultRows
            pairedDevices
            HStack {
                Spacer()
                Button("Done") { remotes.sheet = nil }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 640)
        .onAppear {
            DispatchQueue.global(qos: .userInitiated).async {
                let name = PhonePairing.defaultHostName()
                DispatchQueue.main.async { if host.isEmpty { host = name } }
            }
            checkRemoteLogin()
            loadRemoteHosts()
            tailscale.refresh()
        }
    }

    // MARK: Remote hosts

    @ViewBuilder private var remoteHostRows: some View {
        if loadingHosts {
            HStack(spacing: 6) {
                ProgressView().controlSize(.small)
                Text("Looking up remote hosts…").font(.caption).foregroundStyle(.secondary)
            }
        } else if !remoteHosts.isEmpty {
            VStack(alignment: .leading, spacing: 8) {
                Text("Remote hosts in the code").font(.headline)
                ForEach($remoteHosts) { $h in
                    VStack(alignment: .leading, spacing: 2) {
                        HStack {
                            Toggle(isOn: $h.included) {
                                Text(h.name).frame(width: 110, alignment: .leading).lineLimit(1)
                            }
                            .toggleStyle(.checkbox)
                            .disabled(h.error != nil)
                            TextField("Address", text: $h.address)
                                .frame(width: 210)
                                .disabled(!h.included || h.error != nil)
                            Text(h.port == 22 ? h.user : "\(h.user) · port \(h.port)")
                                .foregroundStyle(.secondary).lineLimit(1)
                            Spacer()
                            Text(state(of: h)).font(.caption)
                                .foregroundStyle(h.error == nil ? Color.secondary : Color.orange)
                                .lineLimit(2)
                        }
                        if h.included, h.error == nil, let proxy = h.proxy {
                            Text("This Mac reaches it through \(proxy), which the phone can't use: enter an address the phone reaches directly.")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        if h.included, h.error == nil, let hint = daemonHint(h.name) {
                            Text(hint).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
        }
    }

    private func state(of h: PhonePairing.RemoteHost) -> String {
        if let error = h.error {
            return error.hasPrefix("no known host key") ? "Connect to it from this Mac once" : error
        }
        if Remotes.shared.statuses[h.name]?.phase != "connected" { return "Not reachable now" }
        return h.hostKeys.count == 1 ? "Host key found" : "\(h.hostKeys.count) host keys found"
    }

    /// A host whose daemon is older than this app's: the terminal works from the phone, push
    /// and Live Activities may need the upgrade.
    private func daemonHint(_ name: String) -> String? {
        guard let s = Remotes.shared.statuses[name], s.phase == "upgrade_needed" || s.upgradeAvailable else {
            return nil
        }
        return "Thurm there is older than this Mac's: notifications and Live Activities need its upgrade (Settings › Remotes)."
    }

    private func loadRemoteHosts() {
        DispatchQueue.global(qos: .userInitiated).async {
            let list = (Core.shared.remoteCall(["op": "pairing_hosts"]) as? [Any] ?? [])
                .compactMap(PhonePairing.RemoteHost.init(json:))
            DispatchQueue.main.async {
                remoteHosts = list
                loadingHosts = false
                for h in list where h.error == nil { loadDevices(on: h.name) }
            }
        }
    }

    private func loadDevices(on name: String) {
        DispatchQueue.global(qos: .userInitiated).async {
            let list = Core.shared.remoteCall(["op": "paired_devices", "name": name]) as? [String]
            DispatchQueue.main.async {
                if let list { remoteDevices[name] = list }
            }
        }
    }

    @ViewBuilder private var keyResultRows: some View {
        ForEach(remoteHosts.filter { keyResults[$0.name] != nil }) { h in
            switch keyResults[h.name] {
            case .adding:
                Label("\(h.name): adding…", systemImage: "hourglass").font(.caption)
            case .added:
                Label("\(h.name): added", systemImage: "checkmark.circle").font(.caption)
                    .foregroundStyle(.green)
            case .failed(let why):
                Label("\(h.name): \(why)", systemImage: "xmark.octagon").font(.caption)
                    .foregroundStyle(.red)
            case .manual(let why, let text):
                VStack(alignment: .leading, spacing: 4) {
                    Label("\(h.name): \(why)", systemImage: "exclamationmark.triangle").font(.caption)
                        .foregroundStyle(.orange)
                    HStack(alignment: .top) {
                        Text(text).font(.system(.caption, design: .monospaced)).textSelection(.enabled)
                        Spacer()
                        Button("Copy") {
                            NSPasteboard.general.clearContents()
                            NSPasteboard.general.setString(text, forType: .string)
                        }
                    }
                }
            case nil:
                EmptyView()
            }
        }
    }

    @ViewBuilder private var pairedDevices: some View {
        let onRemotes = remoteHosts.flatMap { h in
            (remoteDevices[h.name] ?? []).map { RemoteDevice(host: h.name, name: $0) }
        }
        if !devices.isEmpty || !onRemotes.isEmpty {
            Text("Paired devices").font(.headline)
            ForEach(devices) { d in
                deviceRow(d.name, on: "this Mac") { revoke(d) }
            }
            ForEach(onRemotes, id: \.self) { d in
                deviceRow(d.name, on: d.host) { revoke(d) }
            }
        }
    }

    private func deviceRow(_ name: String, on host: String, revoke: @escaping () -> Void) -> some View {
        HStack {
            Image(systemName: "iphone")
            Text(name)
            Text(host).foregroundStyle(.secondary)
            Spacer()
            Button("Revoke", role: .destructive, action: revoke)
        }
    }

    // MARK: Keys

    private func addKey() {
        guard let (key, device) = PhonePairing.validDeviceKey(deviceKey) else { return }
        let line = deviceKey.trimmingCharacters(in: .whitespacesAndNewlines)
        do {
            try PhonePairing.add(key: key, device: device)
            message = "Added \(device) on this Mac. The app can connect now."
            deviceKey = ""
        } catch {
            // A failed pairing must not leave the key working on the remote hosts.
            message = "Could not write ~/.ssh/authorized_keys: \(error.localizedDescription). "
                + "The key was not added on any host."
            devices = PhonePairing.devices()
            return
        }
        devices = PhonePairing.devices()
        for h in PhonePairing.inCode(remoteHosts) {
            authorize(line, on: h.name)
        }
    }

    private func authorize(_ line: String, on name: String) {
        keyResults[name] = .adding
        DispatchQueue.global(qos: .userInitiated).async {
            let r = Core.shared.remoteCall(["op": "authorize_device", "name": name, "key": line]) as? [String: Any]
            DispatchQueue.main.async {
                if let r, r["error"] == nil {
                    keyResults[name] = .added
                    loadDevices(on: name)
                } else if let r, jsonBool(r["not_writable"]) == true {
                    if let nix = jsonString(r["nixos"]) {
                        keyResults[name] = .manual("its keys are managed by NixOS; add the key to its configuration:",
                                                   text: nix)
                    } else {
                        keyResults[name] = .manual("authorized_keys there is not writable; add this line by hand:",
                                                   text: jsonString(r["line"]) ?? line)
                    }
                } else {
                    keyResults[name] = .failed(jsonString(r?["error"]) ?? "no answer")
                }
            }
        }
    }

    /// Removes the phone's key, then has the daemon forget its push registration (no more
    /// notices or Live Activity updates) and drop the phones' open connections (the others
    /// reconnect).
    private func revoke(_ device: PhonePairing.Device) {
        do {
            try PhonePairing.revoke(device)
        } catch {
            message = "Could not revoke \(device.name): its key still works. "
                + "~/.ssh/authorized_keys: \(error.localizedDescription)"
            devices = PhonePairing.devices()
            return
        }
        devices = PhonePairing.devices()
        message = "Revoked \(device.name)."
        forgetPhone(device.name, on: localHost, label: device.name)
    }

    /// The same on a remote host, through ssh, then its daemon.
    private func revoke(_ device: RemoteDevice) {
        message = "Revoking \(device.name) on \(device.host)…"
        DispatchQueue.global(qos: .userInitiated).async {
            let r = Core.shared.remoteCall(["op": "revoke_device", "name": device.host, "device": device.name])
            DispatchQueue.main.async {
                if let e = (r as? [String: Any])?["error"] as? String ?? (r == nil ? "no answer" : nil) {
                    message = "Could not revoke \(device.name) on \(device.host): its key still works. \(e)"
                } else {
                    message = "Revoked \(device.name) on \(device.host)."
                    forgetPhone(device.name, on: device.host, label: "\(device.name) on \(device.host)")
                }
                loadDevices(on: device.host)
            }
        }
    }

    private func forgetPhone(_ name: String, on host: HostId, label: String) {
        Core.shared.requestAsync(object: ["RevokePhone": ["name": name]], host: host, timeout: 15) { resp in
            if resp == nil || (resp as? [String: Any])?["error"] != nil {
                let why = ((resp as? [String: Any])?["error"] as? String) ?? "the daemon did not answer"
                message = "Revoked \(label)'s key, but it may still get notifications: \(why)"
            }
        }
    }

    /// Whether sshd answers on this Mac (Remote Login).
    private func checkRemoteLogin() {
        let conn = NWConnection(host: "127.0.0.1", port: NWEndpoint.Port(integerLiteral: UInt16(clamping: port)),
                                using: .tcp)
        conn.stateUpdateHandler = { state in
            switch state {
            case .ready:
                DispatchQueue.main.async { remoteLogin = true }
                conn.cancel()
            case .failed, .waiting:
                DispatchQueue.main.async { remoteLogin = false }
                conn.cancel()
            default:
                break
            }
        }
        conn.start(queue: .global())
    }
}
