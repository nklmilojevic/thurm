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
    /// port from `ssh -G`, its host keys from this Mac's known hosts, and its daemon's own
    /// Tailscale address when it serves over Tailscale.
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
        /// Why the phone can't use ssh to it.
        var error: String?
        /// Its daemon over Tailscale (`wss://…/v1/connect`), from its `TailscaleStatus`.
        var tailnet: String?
        /// Its plain tailnet endpoints (`ws://…/v1/connect`), for a tailnet without HTTPS.
        var plain: [String] = []
        /// Checked in the sheet.
        var included: Bool
        var id: String { name }

        init(name: String, address: String, port: Int = 22, user: String, hostKeys: [String],
             socket: String? = nil, proxy: String? = nil, error: String? = nil, tailnet: String? = nil) {
            self.name = name
            self.address = address
            self.port = port
            self.user = user
            self.hostKeys = hostKeys
            self.socket = socket
            self.proxy = proxy
            self.error = error
            self.tailnet = tailnet
            included = (error == nil && !hostKeys.isEmpty) || !(tailnet ?? "").isEmpty
        }

        /// The phone can trust it over ssh: the Mac knows its host keys.
        var hasSSH: Bool { error == nil && !hostKeys.isEmpty }

        /// Over ssh or Tailscale, which needs no host keys (as `PairingPayload` checks in the
        /// iOS app).
        var usable: Bool { hasSSH || !(tailnet ?? "").isEmpty || !plain.isEmpty }

        /// Its daemon's Tailscale addresses arrived: a host the phone could not trust over ssh
        /// becomes one it can reach, so it goes in the code.
        mutating func setTailnet(_ url: String?, plain routes: [String] = []) {
            let was = usable
            tailnet = url
            plain = routes
            if !was && usable { included = true }
        }

        init?(json: Any?) {
            guard let d = json as? [String: Any], let name = jsonString(d["name"]) else { return nil }
            self.init(name: name, address: jsonString(d["host"]) ?? "", port: jsonInt(d["port"]) ?? 22,
                      user: jsonString(d["user"]) ?? "", hostKeys: d["host_keys"] as? [String] ?? [],
                      socket: jsonString(d["socket"]), proxy: jsonString(d["proxy"]),
                      error: jsonString(d["error"]))
        }

        /// The phone can use ssh to it: host keys, and an address to connect to.
        var phoneSSH: Bool { hasSSH && !address.trimmingCharacters(in: .whitespaces).isEmpty }

        /// Checked, and something the phone can connect to and trust: over Tailscale, the ssh
        /// address may be empty.
        var inCode: Bool {
            included && (phoneSSH || !(tailnet ?? "").isEmpty || !plain.isEmpty)
        }

        /// Its entry in the code (`PairedHost` in the iOS app). An app that knows `tailnet`
        /// connects there first, and over ssh when it has host keys.
        var payload: [String: Any] {
            var d: [String: Any] = ["name": name, "host": address.trimmingCharacters(in: .whitespaces),
                                    "port": port, "user": user, "host_keys": phoneSSH ? hostKeys : []]
            if let socket { d["socket"] = socket }
            if let tailnet, !tailnet.isEmpty { d["tailnet"] = ["url": tailnet] }
            if !plain.isEmpty { d["tailnet_plain"] = plain }
            return d
        }
    }

    /// The remote hosts the code carries.
    static func inCode(_ remotes: [RemoteHost]) -> [RemoteHost] {
        remotes.filter(\.inCode)
    }

    /// The ones Add Device Key authorizes the phone on: in the code, with ssh for the phone to use.
    /// A host left out of the code, or reached only over Tailscale, never gets its key.
    static func keyHosts(_ remotes: [RemoteHost]) -> [RemoteHost] {
        inCode(remotes).filter(\.phoneSSH)
    }

    /// The Tailscale addresses in a daemon's answer to `TailscaleStatus`: nil for an error (a
    /// daemon from before Tailscale) or no answer; otherwise the HTTPS address once served (nil
    /// before that) and the plain endpoints.
    static func tailnetRoutes(statusResponse resp: Any?) -> (url: String?, plain: [String])? {
        guard let v = JSON.variant(resp), v.name == "TailscaleStatus",
              let status = TailscaleStatus(json: v.payload)
        else { return nil }
        return (status.pairingURL, status.pairingPlain)
    }

    /// The plain endpoints the phone can use: `ws://` addresses, each once, IPv4 literals first,
    /// then IPv6, then names (a name may not resolve on the phone's tailnet, an address always does).
    static func plainRoutes(_ urls: [String]) -> [String] {
        var seen = Set<String>()
        let unique = urls.filter { $0.hasPrefix("ws://") && seen.insert($0).inserted }
        func rank(_ url: String) -> Int {
            guard let host = URL(string: url)?.host else { return 2 }
            if host.contains(":") { return 1 }
            return host.allSatisfy { $0.isNumber || $0 == "." } ? 0 : 2
        }
        // A stable order: the sort alone does not keep equal ranks in place.
        return unique.enumerated()
            .sorted { (rank($0.element), $0.offset) < (rank($1.element), $1.offset) }
            .map(\.element)
    }

    /// Whether there is a code to show: the phone needs this Mac's ssh address (with a valid port)
    /// and host keys, or its Tailscale address, which needs neither (as `PairingPayload` checks in
    /// the iOS app).
    static func canPair(host: String, port: Int = 22, hostKeys: [String], tailnet: String?,
                        plain: [String] = []) -> Bool {
        let sshReady = !host.trimmingCharacters(in: .whitespaces).isEmpty
            && (1...65535).contains(port)
            && !hostKeys.isEmpty
        return sshReady || !(tailnet ?? "").isEmpty || !plain.isEmpty
    }

    /// What the QR code carries (see `PairingPayload` in the iOS app). Remote hosts not checked
    /// (or not usable) are left out. `tailnet` is this Mac's daemon over Tailscale
    /// (`wss://…/v1/connect`), and `plain` its plain endpoints (`ws://`, for a tailnet without
    /// HTTPS): an app that knows them connects there first, and over ssh when it can't; older
    /// apps read only the ssh fields.
    static func payload(name: String, host: String, port: Int, user: String, hostKeys: [String],
                        relay: String?, code: String?, remotes: [RemoteHost] = [], tailnet: String? = nil,
                        plain: [String] = []) -> String {
        let trimmedHost = host.trimmingCharacters(in: .whitespaces)
        let validPort = (1...65535).contains(port)
        let validSSH = !trimmedHost.isEmpty && validPort
        var d: [String: Any] = ["v": 1, "name": name, "host": trimmedHost,
                                "port": validPort ? port : 22, "user": user,
                                "host_keys": validSSH ? hostKeys : []]
        if let tailnet, !tailnet.isEmpty { d["tailnet"] = ["url": tailnet] }
        if !plain.isEmpty { d["tailnet_plain"] = plain }
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

    /// Valid device name characters matching `thurm-remote`'s `validate_device_name`:
    /// 1–128 ASCII letters, digits, `.`, `_`, or `-`.
    static func isValidDeviceName(_ device: String) -> Bool {
        guard !device.isEmpty, device.utf8.count <= 128 else { return false }
        return device.unicodeScalars.allSatisfy { s in
            (s.value >= 0x30 && s.value <= 0x39)
                || (s.value >= 0x41 && s.value <= 0x5A)
                || (s.value >= 0x61 && s.value <= 0x7A)
                || s == "." || s == "_" || s == "-"
        }
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
        let device = String(parts[2].dropFirst(commentPrefix.count))
        guard isValidDeviceName(device) else { return nil }
        return ("\(parts[0]) \(parts[1])", device)
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

    /// Identifies one push registration instance `(device, generation)` on a daemon so delayed
    /// SSH key-replacement cleanup never revokes a Tailscale-only or newer registration.
    struct PushRegistrationRef: Codable, Equatable, Hashable {
        var device: String
        var generation: UInt64

        var json: [String: Any] { ["device": device, "generation": generation] }

        init(device: String, generation: UInt64) {
            self.device = device
            self.generation = generation
        }

        init?(json: Any?) {
            guard let d = json as? [String: Any],
                  let device = jsonString(d["device"]),
                  let genNum = d["generation"] as? NSNumber
            else { return nil }
            self.device = device
            self.generation = genNum.uint64Value
        }
    }

    struct PendingPushRevocation: Codable, Equatable {
        var registrations: [PushRegistrationRef]
        var dropUnregisteredSSH: Bool
    }

    /// Durable store for pre-captured local push revocations whose `~/.ssh/authorized_keys`
    /// replacement succeeded while the daemon cleanup RPC failed, so retrying completes cleanup.
    final class PendingPushRevokesStore {
        static let shared = PendingPushRevokesStore(defaults: .standard, key: "thurm.pendingLocalPushRevokes")

        private let defaults: UserDefaults
        private let key: String
        private let lock = NSLock()

        init(defaults: UserDefaults, key: String = "thurm.pendingLocalPushRevokes") {
            self.defaults = defaults
            self.key = key
        }

        private func loadUnlocked() -> [String: PendingPushRevocation] {
            guard let data = defaults.data(forKey: key),
                  let map = try? JSONDecoder().decode([String: PendingPushRevocation].self, from: data)
            else { return [:] }
            return map
        }

        private func saveUnlocked(_ map: [String: PendingPushRevocation]) {
            if map.isEmpty {
                defaults.removeObject(forKey: key)
            } else if let data = try? JSONEncoder().encode(map) {
                defaults.set(data, forKey: key)
            }
        }

        func get(_ device: String) -> PendingPushRevocation? {
            lock.lock()
            defer { lock.unlock() }
            return loadUnlocked()[device]
        }

        func set(_ device: String, _ entry: PendingPushRevocation) {
            lock.lock()
            defer { lock.unlock() }
            var map = loadUnlocked()
            map[device] = entry
            saveUnlocked(map)
        }

        func remove(_ device: String) {
            lock.lock()
            defer { lock.unlock() }
            var map = loadUnlocked()
            if map.removeValue(forKey: device) != nil {
                saveUnlocked(map)
            }
        }
    }

    /// The outcome of a daemon cleanup step: a value, or the reason it failed (shown to the user).
    enum CleanupResult<Value> {
        case success(Value)
        case failure(String)
    }

    enum LocalKeyAddResult: Equatable {
        case added(replaced: Bool)
        case failed(String)
    }

    /// Whether `file` currently holds a different Thurm device key under `device`.
    static func willReplaceDifferentKey(key: String, device: String, in file: URL = authorizedKeys) throws -> Bool {
        let existingLines = try lines(of: file)
        return existingLines.contains { line in
            guard let existing = parseDeviceLine(line[...]) else { return false }
            return existing.device == device && existing.key != key
        }
    }

    /// Adds `key` for `device` to `file` with pre-captured, generation-scoped push cleanup:
    /// - Adding a brand-new name (or re-adding/renaming the same key) with no pending cleanup
    ///   writes `authorized_keys` directly without needing a daemon RPC.
    /// - Replacing a different key captures existing non-Tailscale `(device, generation)` push
    ///   registrations BEFORE editing `authorized_keys` and fails closed if the daemon cannot be
    ///   reached, so Tailscale-paired or later-registered phones are never revoked.
    /// - If revocation fails after `authorized_keys` already changed, pending targets stay in
    ///   `pendingStore` so retrying the same key completes cleanup without capturing newer phones.
    static func addWithPushCleanup(
        key: String,
        device: String,
        to file: URL = authorizedKeys,
        pendingStore: PendingPushRevokesStore = .shared,
        listSSHRegistrations: (_ name: String, _ done: @escaping (CleanupResult<[PushRegistrationRef]>) -> Void) -> Void,
        revokeRegistrations: @escaping (_ regs: [PushRegistrationRef], _ dropUnregisteredSSH: Bool, _ done: @escaping (CleanupResult<Void>) -> Void) -> Void,
        completion: @escaping (LocalKeyAddResult) -> Void
    ) {
        let priorPending = pendingStore.get(device)
        let willReplace: Bool
        do {
            willReplace = try willReplaceDifferentKey(key: key, device: device, in: file)
        } catch {
            completion(.failed("Could not read ~/.ssh/authorized_keys: \(error.localizedDescription). The key was not added on any host."))
            return
        }

        if !willReplace {
            do {
                _ = try add(key: key, device: device, to: file)
            } catch {
                completion(.failed("Could not write ~/.ssh/authorized_keys: \(error.localizedDescription). The key was not added on any host."))
                return
            }
            guard let pending = priorPending else {
                completion(.added(replaced: false))
                return
            }
            revokeRegistrations(pending.registrations, pending.dropUnregisteredSSH) { res in
                switch res {
                case .success:
                    pendingStore.remove(device)
                    completion(.added(replaced: true))
                case .failure(let why):
                    completion(.failed("Replaced \(device)'s key on this Mac, but could not finish revoking the previous phone's push access (\(why)). Click Add Device Key again to finish cleanup."))
                }
            }
            return
        }

        // Replacing an existing different key: capture non-Tailscale registrations BEFORE writing
        // `~/.ssh/authorized_keys` so a replacement phone cannot be revoked by delayed cleanup.
        listSSHRegistrations(device) { listRes in
            let captured: [PushRegistrationRef]
            switch listRes {
            case .success(let list):
                captured = list
            case .failure(let why):
                completion(.failed("Could not replace \(device)'s key on this Mac without revoking the previous phone's push access: \(why). The key was not changed."))
                return
            }

            var merged = priorPending?.registrations ?? []
            for item in captured where !merged.contains(item) {
                merged.append(item)
            }
            let pending = PendingPushRevocation(registrations: merged, dropUnregisteredSSH: true)

            let replaced: Bool
            do {
                replaced = try add(key: key, device: device, to: file)
            } catch {
                completion(.failed("Could not write ~/.ssh/authorized_keys: \(error.localizedDescription). The key was not added on any host."))
                return
            }
            guard replaced || priorPending != nil else {
                completion(.added(replaced: false))
                return
            }
            pendingStore.set(device, pending)
            revokeRegistrations(pending.registrations, pending.dropUnregisteredSSH) { revokeRes in
                switch revokeRes {
                case .success:
                    pendingStore.remove(device)
                    completion(.added(replaced: true))
                case .failure(let why):
                    completion(.failed("Replaced \(device)'s key on this Mac, but could not revoke the previous phone's push access (\(why)). Click Add Device Key again to finish cleanup."))
                }
            }
        }
    }

    static func devices(in file: URL = authorizedKeys) -> [Device] {
        guard let text = try? String(contentsOf: file, encoding: .utf8) else { return [] }
        return text.split(separator: "\n").compactMap { line in
            deviceName(ofLine: line).map { Device(name: $0, line: String(line)) }
        }
    }

    /// Parses a line Thurm wrote (exactly `restrict,port-forwarding ecdsa-sha2-nistp256
    /// <key> thurm-ios:<device>`); nil for any other line, even one ending the same way.
    static func parseDeviceLine(_ line: Substring) -> (key: String, device: String)? {
        let parts = line.split(separator: " ", omittingEmptySubsequences: false)
        guard parts.count == 4, parts[0] == keyOptions, parts[1] == "ecdsa-sha2-nistp256", !parts[2].isEmpty,
              parts[3].hasPrefix(commentPrefix), parts[3].count > commentPrefix.count
        else { return nil }
        return ("\(parts[1]) \(parts[2])", String(parts[3].dropFirst(commentPrefix.count)))
    }

    /// The device of a line Thurm wrote (exactly `restrict,port-forwarding ecdsa-sha2-nistp256
    /// <key> thurm-ios:<device>`); nil for any other line, even one ending the same way.
    static func deviceName(ofLine line: Substring) -> String? {
        parseDeviceLine(line)?.device
    }

    /// Adds a device key, deduplicating by public key blob (so renaming a phone never leaves an
    /// unrevoked duplicate line) and by device name (so key rotation replaces old keys and each
    /// device name maps 1:1 to `RevokePhone`). Returns `true` when an existing *different* public
    /// key under the same device name was replaced. Creates ~/.ssh (0700) and the file (0600).
    @discardableResult
    static func add(key: String, device: String, to file: URL = authorizedKeys) throws -> Bool {
        let fm = FileManager.default
        try fm.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true,
                               attributes: [.posixPermissions: 0o700])
        var lines = try lines(of: file)
        var replacedDifferentKey = false
        lines.removeAll { line in
            guard let existing = parseDeviceLine(line[...]) else { return false }
            if existing.device == device && existing.key != key {
                replacedDifferentKey = true
                return true
            }
            return existing.key == key || existing.device == device
        }
        lines.append("\(keyOptions) \(key) \(commentPrefix)\(device)")
        try write(lines, to: file)
        return replacedDifferentKey
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

    /// Each remote host adds about 150 bytes (60 more with a Tailscale address): "M" keeps the code readable up to about six,
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
    @State private var loadingHost = true
    @State private var port = 22
    @State private var deviceKey = ""
    @State private var addingLocalKey = false
    @State private var showSSHKeyStep = false
    @State private var devices = PhonePairing.devices()
    @State private var message: String?
    @State private var remoteLogin: Bool?
    @State private var remoteLoginProbe: NWConnection?
    @State private var remoteLoginGeneration = 0
    /// Whether this Mac's `thurm` is found, and speaks our protocol, where sshd logins look.
    @State private var cliReady: Bool?
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
                             remotes: remoteHosts, tailnet: tailscale.status?.pairingURL,
                             plain: tailscale.status?.pairingPlain ?? [])
    }

    private var validPort: Bool {
        (1...65535).contains(port)
    }

    private var canPair: Bool {
        PhonePairing.canPair(host: host, port: port, hostKeys: hostKeys,
                             tailnet: tailscale.status?.pairingURL,
                             plain: tailscale.status?.pairingPlain ?? [])
    }

    /// Whether the pairing code includes at least one SSH target (this Mac or a checked remote),
    /// so Step 2 (pasting the phone's device key) is required rather than optional.
    private var needsSSHKey: Bool {
        let localSSH = !host.trimmingCharacters(in: .whitespaces).isEmpty && validPort && !hostKeys.isEmpty
        return localSSH || !PhonePairing.keyHosts(remoteHosts).isEmpty
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            Text("Pair iPhone or iPad").font(.title2.bold())
            if let url = tailscale.status?.pairingURL {
                Label("The app connects over Tailscale (\(URL(string: url)?.host ?? url)), with ssh as the fallback.",
                      systemImage: "network")
                    .foregroundStyle(.secondary)
            } else if let plain = tailscale.status?.pairingPlain.first {
                Label("The app connects over the tailnet (\(URL(string: plain)?.host ?? plain)), with ssh as the fallback.",
                      systemImage: "network")
                    .foregroundStyle(.secondary)
            }
            if !validPort {
                Label("SSH port must be between 1 and 65535.", systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
            if remoteLogin == false && validPort {
                Label(tailscale.status?.hasTailnetRoute != true
                      ? "Remote Login is off. Turn it on in System Settings › General › Sharing so the app can connect."
                      : "Remote Login is off: the app has no fallback when Tailscale is off on the phone.",
                      systemImage: "exclamationmark.triangle")
                    .foregroundStyle(.orange)
            }
            if loadingHost && host.isEmpty && !canPair {
                HStack(spacing: 6) {
                    ProgressView().controlSize(.small)
                    Text("Finding this Mac's address…").font(.caption).foregroundStyle(.secondary)
                }
            } else if !canPair {
                Label("Pairing over Tailscale needs HTTPS or a plain endpoint: set tailscale.host_listen, or "
                      + "tailscale.plain_listen for a tailnet without HTTPS, in the config.", systemImage: "info.circle")
                    .foregroundStyle(.secondary)
            }
            if cliReady == false {
                HStack {
                    Label("The command-line tool is not on this Mac's login PATH, so a phone's ssh session "
                          + "cannot find thurm.", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                    Button("Install Command-Line Tool") {
                        Integrations.shared.installCLI { checkCLI() }
                    }
                }
            }
            if hostKeys.isEmpty {
                if tailscale.status?.hasTailnetRoute != true {
                    Label("This Mac's ssh host keys could not be read (/etc/ssh).", systemImage: "xmark.octagon")
                        .foregroundStyle(.red)
                } else {
                    Label("This Mac's ssh host keys could not be read (/etc/ssh): the app connects only over "
                          + "Tailscale, with no ssh fallback.", systemImage: "exclamationmark.triangle")
                        .foregroundStyle(.orange)
                }
            }
            HStack(alignment: .top, spacing: 20) {
                if canPair, let image = PhonePairing.qrImage(link, remotes: remoteHosts.filter(\.inCode).count) {
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
                    // Without the host keys the phone can never trust this Mac over ssh.
                    .disabled(!canPair)
                }
            }
            remoteHostRows
            Divider()
            if needsSSHKey {
                Text("2. Paste the device key the app shows (Universal Clipboard or AirDrop).")
                deviceKeyInputRow
            } else {
                DisclosureGroup("2. Add device key (optional — not needed over Tailscale)",
                                isExpanded: $showSSHKeyStep) {
                    VStack(alignment: .leading, spacing: 8) {
                        Text("Only needed if you later add an SSH fallback address.")
                            .font(.caption).foregroundStyle(.secondary)
                        deviceKeyInputRow
                    }
                    .padding(.top, 4)
                }
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
        .onChange(of: port) { _, _ in
            checkRemoteLogin()
        }
        .onAppear {
            DispatchQueue.global(qos: .userInitiated).async {
                let name = PhonePairing.defaultHostName()
                DispatchQueue.main.async {
                    if host.isEmpty { host = name }
                    loadingHost = false
                }
            }
            checkRemoteLogin()
            checkCLI()
            loadRemoteHosts()
            tailscale.refresh()
        }
        .onDisappear {
            remoteLoginGeneration &+= 1
            remoteLoginProbe?.cancel()
            remoteLoginProbe = nil
        }
    }

    private var deviceKeyInputRow: some View {
        HStack {
            TextField("ecdsa-sha2-nistp256 AAAA… thurm-ios:iPhone", text: $deviceKey)
                .font(.system(.caption, design: .monospaced))
            Button("Add Device Key") { addKey() }
                .disabled(addingLocalKey || PhonePairing.validDeviceKey(deviceKey) == nil)
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
                            .disabled(!h.usable)
                            TextField("Address", text: $h.address)
                                .frame(width: 210)
                                .disabled(!h.included || !h.usable)
                            Text(h.port == 22 ? h.user : "\(h.user) · port \(h.port)")
                                .foregroundStyle(.secondary).lineLimit(1)
                            Spacer()
                            Text(state(of: h)).font(.caption)
                                .foregroundStyle(h.usable ? Color.secondary : Color.orange)
                                .lineLimit(2)
                        }
                        if h.included, h.hasSSH, let proxy = h.proxy {
                            Text("This Mac reaches it through \(proxy), which the phone can't use: enter an address the phone reaches directly.")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                        if h.included, h.usable, let hint = daemonHint(h.name) {
                            Text(hint).font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
            }
        }
    }

    private func state(of h: PhonePairing.RemoteHost) -> String {
        if h.tailnet != nil {
            return h.phoneSSH ? "Over Tailscale, ssh as the fallback" : "Over Tailscale"
        }
        if !h.plain.isEmpty {
            return h.phoneSSH ? "Over tailnet, ssh as the fallback" : "Over tailnet"
        }
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
                for h in list where Remotes.shared.statuses[h.name]?.phase == "connected" {
                    loadTailnet(on: h.name)
                }
            }
        }
    }

    /// Asks the host's daemon, through the Mac's tunnel to it, whether it serves over Tailscale.
    private func loadTailnet(on name: String) {
        Core.shared.requestAsync(object: "TailscaleStatus", host: name, timeout: 10) { resp in
            guard let routes = PhonePairing.tailnetRoutes(statusResponse: resp),
                  let i = remoteHosts.firstIndex(where: { $0.name == name })
            else { return }
            remoteHosts[i].setTailnet(routes.url, plain: routes.plain)
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
        guard !addingLocalKey, let (key, device) = PhonePairing.validDeviceKey(deviceKey) else { return }
        let line = deviceKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let remotesToAuthorize = PhonePairing.keyHosts(remoteHosts)
        addingLocalKey = true
        PhonePairing.addWithPushCleanup(
            key: key,
            device: device,
            listSSHRegistrations: { name, done in
                guard Core.shared.isConnected(localHost) else {
                    done(.failure("this Mac's Thurm daemon is not connected"))
                    return
                }
                guard Core.shared.hasCapability(localHost, "push-replace") else {
                    done(.failure("this Mac's Thurm daemon needs to be restarted to support scoped push cleanup"))
                    return
                }
                Core.shared.requestAsync(object: ["ListPushRegistrations": ["name": name]],
                                         host: localHost, timeout: 15) { resp in
                    guard let v = JSON.variant(resp), v.name == "PushRegistrations",
                          let raw = v.payload as? [Any]
                    else {
                        let why = ((resp as? [String: Any])?["error"] as? String)
                            ?? "the daemon did not answer"
                        done(.failure(why))
                        return
                    }
                    done(.success(raw.compactMap(PhonePairing.PushRegistrationRef.init(json:))))
                }
            },
            revokeRegistrations: { regs, dropUnregisteredSSH, done in
                guard Core.shared.isConnected(localHost) else {
                    done(.failure("this Mac's Thurm daemon is not connected"))
                    return
                }
                Core.shared.requestAsync(
                    object: ["RevokePushRegistrations": [
                        "registrations": regs.map(\.json),
                        "drop_unregistered_ssh": dropUnregisteredSSH,
                    ]],
                    host: localHost,
                    timeout: 20
                ) { resp in
                    if (resp as? String) == "Ok" {
                        done(.success(()))
                    } else {
                        let why = ((resp as? [String: Any])?["error"] as? String)
                            ?? "the daemon did not answer"
                        done(.failure(why))
                    }
                }
            },
            completion: { result in
                addingLocalKey = false
                devices = PhonePairing.devices()
                switch result {
                case .added(let replaced):
                    message = replaced
                        ? "Replaced \(device) on this Mac. The app can connect now."
                        : "Added \(device) on this Mac. The app can connect now."
                    deviceKey = ""
                    for h in remotesToAuthorize {
                        authorize(line, on: h.name)
                    }
                case .failed(let why):
                    // A failed local pairing or incomplete push cleanup must not leave the key
                    // working on remote hosts; keep `deviceKey` populated so retrying works.
                    message = why
                }
            }
        )
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

    /// Asks this Mac's `thurm` the way sshd's login environment would find it.
    private func checkCLI() {
        DispatchQueue.global(qos: .userInitiated).async {
            let probe = Core.shared.remoteCall(["op": "local_probe"]) as? [String: Any]
            let ready = probe?["found"] as? Bool == true && probe?["protocol_ok"] as? Bool == true
            DispatchQueue.main.async { cliReady = ready }
        }
    }

    /// Whether sshd answers on this Mac (Remote Login). Cancels any earlier probe so superseded
    /// port checks cannot overwrite `remoteLogin` out of order.
    private func checkRemoteLogin() {
        remoteLoginProbe?.cancel()
        remoteLoginProbe = nil
        remoteLoginGeneration &+= 1
        let gen = remoteLoginGeneration
        guard validPort, let nwPort = NWEndpoint.Port(rawValue: UInt16(port)) else {
            remoteLogin = nil
            return
        }
        let conn = NWConnection(host: "127.0.0.1", port: nwPort, using: .tcp)
        remoteLoginProbe = conn
        conn.stateUpdateHandler = { state in
            switch state {
            case .ready:
                conn.cancel()
                DispatchQueue.main.async {
                    guard remoteLoginGeneration == gen else { return }
                    remoteLogin = true
                    remoteLoginProbe = nil
                }
            case .failed, .waiting:
                conn.cancel()
                DispatchQueue.main.async {
                    guard remoteLoginGeneration == gen else { return }
                    remoteLogin = false
                    remoteLoginProbe = nil
                }
            default:
                break
            }
        }
        conn.start(queue: .global())
    }
}
