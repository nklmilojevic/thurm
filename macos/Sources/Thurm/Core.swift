import Foundation
import QuartzCore
import CThurm

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

/// How far a transfer got.
struct TransferProgress: Equatable {
    var done: UInt64
    var total: UInt64

    var fraction: Double { total == 0 ? 1 : min(1, Double(done) / Double(total)) }
}

/// How a transfer ended (`thurm_upload`, `thurm_download`).
struct TransferOutcome {
    var error: String?
    var cancelled = false
    /// The host's daemon cannot take or send files.
    var unsupported = false
    /// A folder was among the files; nothing was sent.
    var folder = false
    /// Nothing is at the download's path (as opposed to any other failure).
    var notFound = false
    var notModified = false
    /// A download's file: its name, path on the host, size and version.
    var name: String?
    var path: String?
    var size: UInt64 = 0
    var version: String?
    /// Where a download into a folder was saved.
    var savedTo: String?

    init(error: String? = nil) {
        self.error = error
    }

    init(json: [String: Any]) {
        error = json["error"] as? String
        cancelled = json["cancelled"] as? Bool ?? false
        unsupported = json["unsupported"] as? Bool ?? false
        folder = json["folder"] as? Bool ?? false
        notFound = json["not_found"] as? Bool ?? false
        notModified = json["not_modified"] as? Bool ?? false
        savedTo = json["saved_to"] as? String
        if let info = json["info"] as? [String: Any] {
            name = info["name"] as? String
            path = info["path"] as? String
            size = (info["size"] as? NSNumber)?.uint64Value ?? 0
            version = info["version"] as? String
        }
    }
}

private final class TransferBox {
    let onProgress: (TransferProgress) -> Void
    let onFinish: (TransferOutcome) -> Void

    init(_ onProgress: @escaping (TransferProgress) -> Void, _ onFinish: @escaping (TransferOutcome) -> Void) {
        self.onProgress = onProgress
        self.onFinish = onFinish
    }
}

/// Runs on the transfer's thread: progress until the final message, which releases the box.
private let coreTransferCallback: thurm_transfer_cb = { ctx, json in
    guard let ctx, let json else { return }
    let value = JSON.decode(String(cString: json)) as? [String: Any] ?? [:]
    if value["finished"] as? Bool == true {
        let box = Unmanaged<TransferBox>.fromOpaque(ctx).takeRetainedValue()
        let outcome = TransferOutcome(json: value)
        DispatchQueue.main.async { box.onFinish(outcome) }
    } else {
        let box = Unmanaged<TransferBox>.fromOpaque(ctx).takeUnretainedValue()
        let progress = TransferProgress(done: (value["done"] as? NSNumber)?.uint64Value ?? 0,
                                        total: (value["total"] as? NSNumber)?.uint64Value ?? 0)
        DispatchQueue.main.async { box.onProgress(progress) }
    }
}

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
        if host != localHost { Extensions.shared.remoteHostDisconnected(host) }
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

    /// Uses theme `spec` instead of the config file's for this session (nil: the file's again),
    /// and redraws every connection's panes in it. Returns why `spec` is not a theme, if it isn't.
    func setSessionTheme(_ spec: String?) -> String? {
        if let raw = thurm_set_session_theme(spec) {
            defer { thurm_string_free(raw) }
            return String(cString: raw)
        }
        for c in clients.values { thurm_reload_engine(c) }
        return nil
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

    // MARK: File transfers

    /// Whether `host`'s daemon listed `cap` when it connected.
    func hasCapability(_ host: HostId, _ cap: String) -> Bool {
        guard let c = clients[host] else { return false }
        return cap.withCString { thurm_has_capability(c, $0) }
    }

    /// Whether `host`'s daemon takes and sends files on streams.
    func canTransfer(_ host: HostId) -> Bool { hasCapability(host, "transfer") }

    /// Sends `files` (local paths, each with the name to send it as, or nil for its own) to
    /// `pane`'s host as one batch; the daemon types their paths into the pane once all have
    /// landed. `onProgress` and `onFinish` run on the main thread. Returns the id for
    /// `cancelTransfer`, or nil when nothing started (`onFinish` has the reason).
    @discardableResult
    func upload(_ pane: PaneKey, files: [(path: String, name: String?)],
                onProgress: @escaping (TransferProgress) -> Void,
                onFinish: @escaping (TransferOutcome) -> Void) -> UInt64? {
        guard let c = clients[pane.host] else {
            onFinish(TransferOutcome(error: "\(pane.host) is not connected"))
            return nil
        }
        let list: [[String: Any]] = files.map { f in
            var entry: [String: Any] = ["path": f.path]
            if let name = f.name { entry["name"] = name }
            return entry
        }
        let box = Unmanaged.passRetained(TransferBox(onProgress, onFinish)).toOpaque()
        let id = JSON.encode(list).withCString { thurm_upload(c, pane.id, $0, coreTransferCallback, box) }
        return id == 0 ? nil : id
    }

    /// Fetches the file at `path` as `pane` sees it into `dest`: nothing is written while
    /// `ifVersion` is still its version (`notModified`), and a file over `maxBytes` (0: any size)
    /// is refused. Callbacks run on the main thread. Returns the id for `cancelTransfer`.
    @discardableResult
    func download(_ pane: PaneKey, path: String, to dest: URL, ifVersion: String? = nil,
                  maxBytes: UInt64 = 0,
                  onProgress: @escaping (TransferProgress) -> Void,
                  onFinish: @escaping (TransferOutcome) -> Void) -> UInt64? {
        guard let c = clients[pane.host] else {
            onFinish(TransferOutcome(error: "\(pane.host) is not connected"))
            return nil
        }
        let box = Unmanaged.passRetained(TransferBox(onProgress, onFinish)).toOpaque()
        let id = path.withCString { p in
            dest.path.withCString { d in
                if let ifVersion {
                    return ifVersion.withCString { v in
                        thurm_download(c, pane.id, p, d, v, maxBytes, coreTransferCallback, box)
                    }
                }
                return thurm_download(c, pane.id, p, d, nil, maxBytes, coreTransferCallback, box)
            }
        }
        return id == 0 ? nil : id
    }

    /// Fetches the file at `path` as `pane` sees it into the folder `dir` as `name` (or
    /// `name 2`… when taken), moved in by a rename that never replaces a file: nothing already
    /// in `dir` is touched. `savedTo` in the outcome is where it went. Callbacks on the main
    /// thread; returns the id for `cancelTransfer`.
    @discardableResult
    func download(_ pane: PaneKey, path: String, intoFolder dir: URL, name: String,
                  onProgress: @escaping (TransferProgress) -> Void,
                  onFinish: @escaping (TransferOutcome) -> Void) -> UInt64? {
        guard let c = clients[pane.host] else {
            onFinish(TransferOutcome(error: "\(pane.host) is not connected"))
            return nil
        }
        let box = Unmanaged.passRetained(TransferBox(onProgress, onFinish)).toOpaque()
        let id = path.withCString { p in
            dir.path.withCString { d in
                name.withCString { n in
                    thurm_download_to_folder(c, pane.id, p, d, n, coreTransferCallback, box)
                }
            }
        }
        return id == 0 ? nil : id
    }

    /// Stops an upload (nothing is typed or kept on the host) or a download.
    func cancelTransfer(_ id: UInt64) {
        thurm_upload_cancel(id)
        thurm_download_cancel(id)
    }

    /// Starts a download (`download`), returning what cancels it; tests stand in for it.
    typealias DownloadStarter = (_ pane: PaneKey, _ path: String, _ dest: URL, _ ifVersion: String?,
                                 _ maxBytes: UInt64,
                                 _ onProgress: @escaping (TransferProgress) -> Void,
                                 _ onFinish: @escaping (TransferOutcome) -> Void) -> (() -> Void)?

    /// What cancels transfer `id`, if it started.
    private static func cancelling(_ id: UInt64?) -> (() -> Void)? {
        id.map { id in { Core.shared.cancelTransfer(id) } }
    }

    static let startDownload: DownloadStarter = { pane, path, dest, ifVersion, maxBytes, onProgress, onFinish in
        cancelling(Core.shared.download(pane, path: path, to: dest, ifVersion: ifVersion, maxBytes: maxBytes,
                                        onProgress: onProgress, onFinish: onFinish))
    }

    /// Starts a download into a folder (`download(_:path:intoFolder:name:…)`), returning what
    /// cancels it; tests stand in for it.
    typealias FolderDownloadStarter = (_ pane: PaneKey, _ path: String, _ dir: URL, _ name: String,
                                       _ onProgress: @escaping (TransferProgress) -> Void,
                                       _ onFinish: @escaping (TransferOutcome) -> Void) -> (() -> Void)?

    static let startFolderDownload: FolderDownloadStarter = { pane, path, dir, name, onProgress, onFinish in
        cancelling(Core.shared.download(pane, path: path, intoFolder: dir, name: name,
                                        onProgress: onProgress, onFinish: onFinish))
    }

    /// Starts an upload of one batch (`upload`), returning what cancels it; tests stand in for it.
    typealias UploadStarter = (_ pane: PaneKey, _ files: [(path: String, name: String?)],
                               _ onProgress: @escaping (TransferProgress) -> Void,
                               _ onFinish: @escaping (TransferOutcome) -> Void) -> (() -> Void)?

    static let startUpload: UploadStarter = { pane, files, onProgress, onFinish in
        cancelling(Core.shared.upload(pane, files: files, onProgress: onProgress, onFinish: onFinish))
    }

    /// Answers `host`'s clipboard query `id` with `data`, or refuses it (nil).
    func clipboardAnswer(host: HostId, id: UInt64, data: Data?) {
        guard let c = clients[host] else { return }
        guard let data else {
            thurm_clipboard_answer(c, id, nil, 0)
            return
        }
        // An empty answer still needs a pointer: NULL is a refusal.
        var empty: UInt8 = 0
        if data.isEmpty {
            thurm_clipboard_answer(c, id, &empty, 0)
            return
        }
        data.withUnsafeBytes { buf in
            thurm_clipboard_answer(c, id, buf.bindMemory(to: UInt8.self).baseAddress, buf.count)
        }
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
