import Foundation
import QuartzCore
import CThurm

/// A daemon the app talks to: this Mac's (`localHost`) or a `[[remote]]` host's, by name.
typealias HostId = String

/// This Mac's own daemon.
let localHost: HostId = "local"

/// A pane is identified by its daemon and that daemon's id: two daemons number their panes
/// independently, so ids alone collide.
struct PaneKey: Hashable, CustomStringConvertible {
    let host: HostId
    let id: UInt64

    init(_ host: HostId, _ id: UInt64) {
        self.host = host
        self.id = id
    }

    static func local(_ id: UInt64) -> PaneKey { PaneKey(localHost, id) }

    var isRemote: Bool { host != localHost }
    /// The id as request payloads carry it.
    var number: NSNumber { NSNumber(value: id) }
    var description: String { isRemote ? "\(host):\(id)" : "\(id)" }
}

/// Receives daemon events, always on the main thread.
protocol CoreDelegate: AnyObject {
    func coreDidReceiveEvent(_ name: String, payload: Any?, host: HostId)
}

/// Identifies one connection. Passed as the C callback context so that events from an old,
/// already replaced connection (e.g. its late "Disconnected") can be ignored, and so events
/// and frames reach the right host's model.
/// Tokens are intentionally never freed: the Rust side may still hold the pointer.
final class ConnectionToken {
    let host: HostId
    let epoch: Int
    init(host: HostId, epoch: Int) {
        self.host = host
        self.epoch = epoch
    }
}

// C callbacks. They run on a Rust background thread; they must not capture context.

private let coreEventCallback: thurm_event_cb = { ctx, json in
    guard let ctx = ctx, let json = json else { return }
    let token = Unmanaged<ConnectionToken>.fromOpaque(ctx).takeUnretainedValue()
    Core.shared.receiveEvent(String(cString: json), host: token.host, epoch: token.epoch)
}

private let coreFrameCallback: thurm_frame_cb = { ctx, pane in
    guard let ctx = ctx else { return }
    let token = Unmanaged<ConnectionToken>.fromOpaque(ctx).takeUnretainedValue()
    Core.shared.markDirty(PaneKey(token.host, pane))
}

/// Thin Swift wrapper around the `thurm_*` C API exported by the Rust core: one connection
/// per daemon (this Mac's, and each connected remote host's through its tunnel).
final class Core {
    static let shared = Core()

    weak var delegate: CoreDelegate?

    private var clients: [HostId: OpaquePointer] = [:]
    /// The last connection error of this Mac's daemon.
    var lastError: String?

    private var epochs: [HostId: Int] = [:]
    private var tokens: [ConnectionToken] = []

    private let dirtyLock = NSLock()
    private var dirtyPanes = Set<PaneKey>()

    private init() {}

    /// The live connection to this Mac's daemon, nil while disconnected.
    var client: OpaquePointer? { clients[localHost] }

    func client(for host: HostId) -> OpaquePointer? { clients[host] }

    var isConnected: Bool { clients[localHost] != nil }

    func isConnected(_ host: HostId) -> Bool { clients[host] != nil }

    /// Remote hosts with a live connection.
    var connectedRemotes: [HostId] { clients.keys.filter { $0 != localHost }.sorted() }

    /// `thurmd` in the bundle's `Contents/Helpers` (not `Contents/MacOS`: the CLI `thurm`
    /// would collide with `Thurm` on a case-insensitive volume), or `$THURM_DAEMON`.
    static var daemonPath: String? {
        let fm = FileManager.default
        if let env = ProcessInfo.processInfo.environment["THURM_DAEMON"], fm.isExecutableFile(atPath: env) {
            return env
        }
        let path = Bundle.main.bundleURL
            .appendingPathComponent("Contents/Helpers/thurmd").path
        return fm.isExecutableFile(atPath: path) ? path : nil
    }

    /// Where `thurm` and `thurmd` are, to copy onto another Mac.
    static var helpersPath: String? {
        daemonPath.map { ($0 as NSString).deletingLastPathComponent }
    }

    /// This build's identifier; a daemon reporting another one is replaced in place.
    static let buildId = String(cString: thurm_build_id())

    // MARK: Connection

    private func newToken(_ host: HostId) -> UnsafeMutableRawPointer {
        let epoch = (epochs[host] ?? 0) + 1
        epochs[host] = epoch
        let token = ConnectionToken(host: host, epoch: epoch)
        tokens.append(token)
        return Unmanaged.passUnretained(token).toOpaque()
    }

    /// Connects to this Mac's daemon (spawning it when needed). Must be called on the main
    /// thread.
    @discardableResult
    func connect() -> Bool {
        if client != nil { return true }
        let ctx = newToken(localHost)
        var err: UnsafeMutablePointer<CChar>? = nil
        var result: OpaquePointer? = nil
        if let path = Core.daemonPath {
            result = path.withCString { cPath in
                thurm_connect(cPath, "Thurm.app", coreEventCallback, coreFrameCallback, ctx, &err)
            }
        } else {
            tlog("thurmd not found next to the executable; connecting to a running daemon only")
            result = thurm_connect(nil, "Thurm.app", coreEventCallback, coreFrameCallback, ctx, &err)
        }
        if let e = err {
            lastError = String(cString: e)
            thurm_string_free(e)
        }
        guard let c = result else {
            tlog("connect failed: \(lastError ?? "unknown error")")
            return false
        }
        clients[localHost] = c
        lastError = nil
        return true
    }

    /// Connects to `host`'s daemon through its tunnel's local socket. Returns the error.
    @discardableResult
    func connectRemote(_ host: HostId, socket: String) -> String? {
        if clients[host] != nil { return nil }
        let ctx = newToken(host)
        var err: UnsafeMutablePointer<CChar>? = nil
        let result = socket.withCString { sock in
            thurm_connect_socket(sock, nil, "Thurm.app", coreEventCallback, coreFrameCallback, ctx, &err)
        }
        var message: String?
        if let e = err {
            message = String(cString: e)
            thurm_string_free(e)
        }
        guard let c = result else { return message ?? "cannot connect" }
        clients[host] = c
        return nil
    }

    /// Closes the connection on purpose. Panes keep running inside the daemon.
    func disconnect(_ host: HostId = localHost) {
        guard let c = clients.removeValue(forKey: host) else { return }
        epochs[host] = (epochs[host] ?? 0) + 1 // late events from the old connection are ignored
        thurm_disconnect(c)
    }

    func disconnectAll() {
        for host in Array(clients.keys) { disconnect(host) }
    }

    // MARK: Events

    fileprivate func receiveEvent(_ text: String, host: HostId, epoch eventEpoch: Int) {
        // Parse on the callback thread, dispatch on main.
        guard let v = JSON.variant(JSON.decode(text)) else {
            tlog("unparseable event: \(text.prefix(200))")
            return
        }
        let name = v.name
        let payload = v.payload
        DispatchQueue.main.async {
            let core = Core.shared
            guard eventEpoch == core.epochs[host] else { return }
            core.delegate?.coreDidReceiveEvent(name, payload: payload, host: host)
        }
    }

    fileprivate func markDirty(_ pane: PaneKey) {
        let now = Perf.enabled ? CACurrentMediaTime() : 0
        dirtyLock.lock()
        let first = dirtyPanes.insert(pane).inserted
        if Perf.enabled {
            var a = arrivals[pane] ?? (0, now, now)
            a = (a.count + 1, a.count == 0 ? now : a.first, now)
            arrivals[pane] = a
        }
        dirtyLock.unlock()
        // Draw from the event rather than waiting for the view's next display tick. One hop
        // per batch: while the pane stays dirty, the pending hop (or the tick) picks it up.
        if first {
            DispatchQueue.main.async { SessionManager.shared.view(for: pane)?.frameArrived() }
        }
    }

    /// Perf: daemon frames received for `pane` since the last call, with the first and last
    /// arrival times.
    private var arrivals: [PaneKey: (count: Int, first: CFTimeInterval, last: CFTimeInterval)] = [:]

    func takeArrivals(_ pane: PaneKey) -> (count: Int, first: CFTimeInterval, last: CFTimeInterval)? {
        dirtyLock.lock()
        defer { dirtyLock.unlock() }
        return arrivals.removeValue(forKey: pane)
    }

    /// True (once) when a new frame arrived for `pane` since the last call.
    func takeDirty(_ pane: PaneKey) -> Bool {
        dirtyLock.lock()
        let was = dirtyPanes.remove(pane) != nil
        dirtyLock.unlock()
        return was
    }

    // MARK: Requests

    /// On the main thread a request never waits longer than this: a daemon that stopped
    /// answering (a hung tunnel after sleep) must not freeze the app.
    static let mainThreadTimeout: TimeInterval = 10

    /// Blocking request to `host`'s daemon. Returns the decoded JSON response ({"error": …}
    /// after `timeout`), or nil when disconnected. `timeout` defaults to `mainThreadTimeout`
    /// on the main thread and to none elsewhere.
    @discardableResult
    func request(_ json: String, host: HostId = localHost, timeout: TimeInterval? = nil) -> Any? {
        assert(Thread.isMainThread, "Core requests resolve connections on the main thread")
        guard let c = clients[host] else { return nil }
        let limit = timeout ?? Core.mainThreadTimeout
        guard let raw = thurm_request_timeout(c, json, UInt64(max(0, limit) * 1000)) else { return nil }
        let text = String(cString: raw)
        thurm_string_free(raw)
        let value = JSON.decode(text)
        if let d = value as? [String: Any], let e = d["error"] as? String {
            tlog("request to \(host) failed: \(e) (\(json.prefix(120)))")
        }
        return value
    }

    @discardableResult
    func request(object: Any, host: HostId = localHost, timeout: TimeInterval? = nil) -> Any? {
        request(JSON.encode(object), host: host, timeout: timeout)
    }

    /// Sends a request without blocking; `completion` runs on the main thread with the decoded
    /// response (nil when not connected). Resolves the connection here, on the main thread:
    /// the library keeps it open for the request even if it is disconnected meanwhile.
    func requestAsync(object: Any, host: HostId = localHost, timeout: TimeInterval,
                      completion: @escaping (Any?) -> Void) {
        assert(Thread.isMainThread, "Core requests resolve connections on the main thread")
        guard let c = clients[host] else {
            completion(nil)
            return
        }
        let box = Unmanaged.passRetained(ResponseBox(completion)).toOpaque()
        JSON.encode(object).withCString { json in
            thurm_request_async(c, json, UInt64(max(0, timeout) * 1000), { ctx, raw in
                guard let ctx else { return }
                let box = Unmanaged<ResponseBox>.fromOpaque(ctx).takeRetainedValue()
                var value: Any?
                if let raw {
                    value = JSON.decode(String(cString: raw))
                    thurm_string_free(raw)
                }
                DispatchQueue.main.async { box.completion(value) }
            }, box)
        }
    }

    /// Fire-and-forget request.
    func send(_ json: String, host: HostId = localHost) {
        guard let c = clients[host] else { return }
        thurm_send(c, json)
    }

    func send(object: Any, host: HostId = localHost) {
        send(JSON.encode(object), host: host)
    }

    // MARK: Hot paths

    func subscribe(_ pane: PaneKey) {
        guard let c = clients[pane.host] else { return }
        thurm_subscribe(c, pane.id)
    }

    func unsubscribe(_ pane: PaneKey) {
        guard let c = clients[pane.host] else { return }
        thurm_unsubscribe(c, pane.id)
    }

    /// Raw UTF-8 text straight to the PTY.
    func input(_ pane: PaneKey, text: String) {
        guard let c = clients[pane.host], !text.isEmpty else { return }
        let bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            thurm_input(c, pane.id, buf.baseAddress, buf.count)
        }
    }

    /// Shows every pane in theme `name` without saving it (nil: the configured theme again).
    /// Returns the theme now shown, as JSON; nil for an unknown theme.
    func previewTheme(_ name: String?) -> String? {
        // Remote panes are drawn with this Mac's theme too.
        for (host, c) in clients where host != localHost {
            if let raw = thurm_preview_theme(c, name) { thurm_string_free(raw) }
        }
        guard let c = client, let raw = thurm_preview_theme(c, name) else { return nil }
        defer { thurm_string_free(raw) }
        return String(cString: raw)
    }

    /// This Mac's config changed: remote connections re-read it (the local daemon's
    /// ConfigReloaded does that for its own).
    func reloadRemoteEngines() {
        for (host, c) in clients where host != localHost {
            thurm_reload_engine(c)
        }
    }

    func paste(_ pane: PaneKey, text: String) {
        guard let c = clients[pane.host] else { return }
        thurm_paste(c, pane.id, text)
    }

    func resize(_ pane: PaneKey, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        guard let c = clients[pane.host] else { return }
        thurm_resize(c, pane.id,
                     UInt16(clamping: cols), UInt16(clamping: rows),
                     UInt16(clamping: cellWidth), UInt16(clamping: cellHeight))
    }

    func focus(_ pane: PaneKey, focused: Bool) {
        guard let c = clients[pane.host] else { return }
        thurm_focus(c, pane.id, focused)
    }

    func key(_ pane: PaneKey, kind: UInt32, code: UInt32, mods: UInt8, action: UInt8,
             text: String?, shifted: UInt32, baseLayout: UInt32) {
        guard let c = clients[pane.host] else { return }
        var ev = thurm_key_event()
        ev.kind = kind
        ev.code = code
        ev.mods = mods
        ev.action = action
        ev.shifted = shifted
        ev.base_layout = baseLayout
        if let text = text, !text.isEmpty {
            text.withCString { ptr in
                ev.text = ptr
                thurm_key(c, pane.id, &ev)
            }
        } else {
            ev.text = nil
            thurm_key(c, pane.id, &ev)
        }
    }

    func mouse(_ pane: PaneKey, kind: UInt8, button: UInt8, mods: UInt8, clicks: Int,
               col: Int, row: Int, rightHalf: Bool, x: Int, y: Int) {
        guard let c = clients[pane.host] else { return }
        var ev = thurm_mouse_event()
        ev.kind = kind
        ev.button = button
        ev.mods = mods
        ev.clicks = UInt8(clamping: max(1, min(3, clicks)))
        ev.col = UInt16(clamping: max(0, col))
        ev.row = UInt16(clamping: max(0, row))
        ev.right_half = rightHalf
        ev.x = UInt32(clamping: max(0, x))
        ev.y = UInt32(clamping: max(0, y))
        thurm_mouse(c, pane.id, &ev)
    }

    func wheel(_ pane: PaneKey, lines: Int, col: Int, row: Int, mods: UInt8) {
        guard let c = clients[pane.host], lines != 0 else { return }
        thurm_wheel(c, pane.id, Int32(clamping: lines),
                    UInt16(clamping: max(0, col)), UInt16(clamping: max(0, row)), mods)
    }

    /// Writes `data` to a new private file on `host` (in its daemon's runtime directory).
    /// Returns the path there, or the error.
    func writeTempFile(host: HostId, name: String, data: Data) -> (path: String?, error: String?) {
        guard let c = clients[host] else { return (nil, "\(host) is not connected") }
        var err: UnsafeMutablePointer<CChar>? = nil
        let raw: UnsafeMutablePointer<CChar>? = data.withUnsafeBytes { buf in
            name.withCString { n in
                thurm_write_temp_file(c, n, buf.bindMemory(to: UInt8.self).baseAddress, buf.count, &err)
            }
        }
        if let raw {
            defer { thurm_string_free(raw) }
            return (String(cString: raw), nil)
        }
        var message = "cannot write the file"
        if let e = err {
            message = String(cString: e)
            thurm_string_free(e)
        }
        return (nil, message)
    }

    // MARK: Remote operations

    /// `thurm_remote_call`: blocking; ssh-backed operations belong off the main thread.
    func remoteCall(_ object: [String: Any]) -> Any? {
        let json = JSON.encode(object)
        guard let raw = thurm_remote_call(json) else { return nil }
        defer { thurm_string_free(raw) }
        return JSON.decode(String(cString: raw))
    }
}

/// Carries a `requestAsync` completion through the C callback.
private final class ResponseBox {
    let completion: (Any?) -> Void
    init(_ completion: @escaping (Any?) -> Void) { self.completion = completion }
}
