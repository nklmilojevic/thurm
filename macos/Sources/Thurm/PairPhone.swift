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

    /// What the QR code carries (see `PairingPayload` in the iOS app).
    static func payload(name: String, host: String, port: Int, user: String, hostKeys: [String],
                        relay: String?, code: String?) -> String {
        var d: [String: Any] = ["v": 1, "name": name, "host": host, "port": port, "user": user,
                                "host_keys": hostKeys]
        if let relay, !relay.isEmpty { d["relay"] = relay }
        if let code, !code.isEmpty { d["code"] = code }
        let json = (try? JSONSerialization.data(withJSONObject: d, options: [.sortedKeys])) ?? Data()
        let b64 = json.base64EncodedString()
            .replacingOccurrences(of: "+", with: "-").replacingOccurrences(of: "/", with: "_")
            .replacingOccurrences(of: "=", with: "")
        return "thurm://pair?d=" + b64
    }

    /// A device key line from the iOS app, checked: only its P-256 keys with Thurm's comment.
    static func validDeviceKey(_ text: String) -> (key: String, device: String)? {
        let parts = text.trimmingCharacters(in: .whitespacesAndNewlines).split(separator: " ")
        guard parts.count == 3, parts[0] == "ecdsa-sha2-nistp256",
              let blob = Data(base64Encoded: String(parts[1])), blob.count == 104,
              parts[2].hasPrefix(commentPrefix), parts[2].count > commentPrefix.count
        else { return nil }
        return ("\(parts[0]) \(parts[1])", String(parts[2].dropFirst(commentPrefix.count)))
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
            guard let comment = line.split(separator: " ").last, comment.hasPrefix(commentPrefix) else {
                return nil
            }
            return Device(name: String(comment.dropFirst(commentPrefix.count)), line: String(line))
        }
    }

    /// Adds (or replaces, by device name) a device key. Creates ~/.ssh (0700) and the file (0600).
    static func add(key: String, device: String, to file: URL = authorizedKeys) throws {
        let fm = FileManager.default
        try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true,
                               attributes: [.posixPermissions: 0o700])
        var lines = ((try? String(contentsOf: file, encoding: .utf8)) ?? "")
            .split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
        if lines.last == "" { lines.removeLast() }
        lines.removeAll { $0.hasSuffix(" " + commentPrefix + device) }
        lines.append("\(keyOptions) \(key) \(commentPrefix)\(device)")
        try write(lines, to: file)
    }

    static func revoke(_ device: Device, in file: URL = authorizedKeys) throws {
        guard let text = try? String(contentsOf: file, encoding: .utf8) else { return }
        let lines = text.split(separator: "\n", omittingEmptySubsequences: false).map(String.init)
            .filter { $0 != device.line }
        try write(lines.last == "" ? Array(lines.dropLast()) : lines, to: file)
    }

    private static func write(_ lines: [String], to file: URL) throws {
        try (lines.joined(separator: "\n") + "\n").write(to: file, atomically: true, encoding: .utf8)
        try FileManager.default.setAttributes([.posixPermissions: 0o600], ofItemAtPath: file.path)
    }

    /// This Mac's name on the tailnet (MagicDNS), else its Bonjour name.
    static func defaultHostName() -> String {
        for cli in ["/Applications/Tailscale.app/Contents/MacOS/Tailscale", "/usr/local/bin/tailscale",
                    "/opt/homebrew/bin/tailscale"] where FileManager.default.isExecutableFile(atPath: cli) {
            let p = Process()
            p.executableURL = URL(fileURLWithPath: cli)
            p.arguments = ["status", "--json"]
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

    static func qrImage(_ text: String) -> NSImage? {
        guard let filter = CIFilter(name: "CIQRCodeGenerator") else { return nil }
        filter.setValue(Data(text.utf8), forKey: "inputMessage")
        filter.setValue("M", forKey: "inputCorrectionLevel")
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
    private let hostKeys = PhonePairing.hostKeyFingerprints()
    private let user = NSUserName()
    private let push = SessionManager.shared.config.push

    private var link: String {
        PhonePairing.payload(name: Host.current().localizedName ?? host, host: host, port: port,
                             user: user, hostKeys: hostKeys, relay: push.relay, code: push.code)
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Pair iPhone or iPad").font(.title2.bold())
            if remoteLogin == false {
                Label("Remote Login is off. Turn it on in System Settings › General › Sharing so the app can connect.",
                      systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
            if hostKeys.isEmpty {
                Label("This Mac's ssh host keys could not be read (/etc/ssh).", systemImage: "xmark.octagon")
                    .foregroundStyle(.red)
            }
            HStack(alignment: .top, spacing: 20) {
                if let image = PhonePairing.qrImage(link), !host.isEmpty, !hostKeys.isEmpty {
                    Image(nsImage: image)
                        .interpolation(.none)
                        .resizable()
                        .frame(width: 220, height: 220)
                        .accessibilityLabel("Pairing code")
                }
                VStack(alignment: .leading, spacing: 8) {
                    Text("1. In the Thurm app on your iPhone, tap + and scan this code.")
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
                }
            }
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
            if !devices.isEmpty {
                Text("Paired devices").font(.headline)
                ForEach(devices) { d in
                    HStack {
                        Image(systemName: "iphone")
                        Text(d.name)
                        Spacer()
                        Button("Revoke", role: .destructive) {
                            try? PhonePairing.revoke(d)
                            devices = PhonePairing.devices()
                        }
                    }
                }
            }
            HStack {
                Spacer()
                Button("Done") { remotes.sheet = nil }
                    .keyboardShortcut(.defaultAction)
            }
        }
        .padding(20)
        .frame(width: 560)
        .onAppear {
            DispatchQueue.global(qos: .userInitiated).async {
                let name = PhonePairing.defaultHostName()
                DispatchQueue.main.async { if host.isEmpty { host = name } }
            }
            checkRemoteLogin()
        }
    }

    private func addKey() {
        guard let (key, device) = PhonePairing.validDeviceKey(deviceKey) else { return }
        do {
            try PhonePairing.add(key: key, device: device)
            message = "Added \(device). The app can connect now."
            deviceKey = ""
        } catch {
            message = "Could not write ~/.ssh/authorized_keys: \(error.localizedDescription)"
        }
        devices = PhonePairing.devices()
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
