import Foundation
import QuartzCore
import CThurm

/// Receives daemon events, always on the main thread.
protocol CoreDelegate: AnyObject {
    func coreDidReceiveEvent(_ name: String, payload: Any?)
}

/// Identifies one connection. Passed as the C callback context so that events from an old,
/// already replaced connection (e.g. its late "Disconnected") can be ignored.
/// Tokens are intentionally never freed: the Rust side may still hold the pointer.
final class ConnectionToken {
    let epoch: Int
    init(epoch: Int) { self.epoch = epoch }
}

// C callbacks. They run on a Rust background thread; they must not capture context.

private let coreEventCallback: thurm_event_cb = { ctx, json in
    guard let ctx = ctx, let json = json else { return }
    let token = Unmanaged<ConnectionToken>.fromOpaque(ctx).takeUnretainedValue()
    Core.shared.receiveEvent(String(cString: json), epoch: token.epoch)
}

private let coreFrameCallback: thurm_frame_cb = { _, pane in
    Core.shared.markDirty(pane)
}

/// Thin Swift wrapper around the `thurm_*` C API exported by the Rust core.
final class Core {
    static let shared = Core()

    weak var delegate: CoreDelegate?

    /// The live connection, nil while disconnected.
    private(set) var client: OpaquePointer?
    var lastError: String?

    private var epoch = 0
    private var tokens: [ConnectionToken] = []

    private let dirtyLock = NSLock()
    private var dirtyPanes = Set<UInt64>()

    private init() {}

    var isConnected: Bool { client != nil }

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

    /// This build's identifier; a daemon reporting another one is replaced in place.
    static let buildId = String(cString: thurm_build_id())

    // MARK: Connection

    /// Connects (spawning the daemon when needed). Must be called on the main thread.
    @discardableResult
    func connect() -> Bool {
        if client != nil { return true }
        epoch += 1
        let token = ConnectionToken(epoch: epoch)
        tokens.append(token)
        let ctx = Unmanaged.passUnretained(token).toOpaque()

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
        client = c
        lastError = nil
        return true
    }

    /// Closes the connection on purpose. Panes keep running inside the daemon.
    func disconnect() {
        guard let c = client else { return }
        client = nil
        epoch += 1 // late events from the old connection are ignored
        thurm_disconnect(c)
    }

    // MARK: Events

    fileprivate func receiveEvent(_ text: String, epoch eventEpoch: Int) {
        // Parse on the callback thread, dispatch on main.
        guard let v = JSON.variant(JSON.decode(text)) else {
            tlog("unparseable event: \(text.prefix(200))")
            return
        }
        let name = v.name
        let payload = v.payload
        DispatchQueue.main.async {
            let core = Core.shared
            guard eventEpoch == core.epoch else { return }
            core.delegate?.coreDidReceiveEvent(name, payload: payload)
        }
    }

    fileprivate func markDirty(_ pane: UInt64) {
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
    private var arrivals: [UInt64: (count: Int, first: CFTimeInterval, last: CFTimeInterval)] = [:]

    func takeArrivals(_ pane: UInt64) -> (count: Int, first: CFTimeInterval, last: CFTimeInterval)? {
        dirtyLock.lock()
        defer { dirtyLock.unlock() }
        return arrivals.removeValue(forKey: pane)
    }

    /// True (once) when a new frame arrived for `pane` since the last call.
    func takeDirty(_ pane: UInt64) -> Bool {
        dirtyLock.lock()
        let was = dirtyPanes.remove(pane) != nil
        dirtyLock.unlock()
        return was
    }

    // MARK: Requests

    /// Blocking request. Returns the decoded JSON response, or nil when disconnected.
    @discardableResult
    func request(_ json: String) -> Any? {
        guard let c = client else { return nil }
        guard let raw = thurm_request(c, json) else { return nil }
        let text = String(cString: raw)
        thurm_string_free(raw)
        let value = JSON.decode(text)
        if let d = value as? [String: Any], let e = d["error"] as? String {
            tlog("request failed: \(e) (\(json.prefix(120)))")
        }
        return value
    }

    @discardableResult
    func request(object: Any) -> Any? {
        request(JSON.encode(object))
    }

    /// Fire-and-forget request.
    func send(_ json: String) {
        guard let c = client else { return }
        thurm_send(c, json)
    }

    func send(object: Any) {
        send(JSON.encode(object))
    }

    // MARK: Hot paths

    func subscribe(_ pane: UInt64) {
        guard let c = client else { return }
        thurm_subscribe(c, pane)
    }

    func unsubscribe(_ pane: UInt64) {
        guard let c = client else { return }
        thurm_unsubscribe(c, pane)
    }

    /// Raw UTF-8 text straight to the PTY.
    func input(_ pane: UInt64, text: String) {
        guard let c = client, !text.isEmpty else { return }
        let bytes = Array(text.utf8)
        bytes.withUnsafeBufferPointer { buf in
            thurm_input(c, pane, buf.baseAddress, buf.count)
        }
    }

    /// Shows every pane in theme `name` without saving it (nil: the configured theme again).
    /// Returns the theme now shown, as JSON; nil for an unknown theme.
    func previewTheme(_ name: String?) -> String? {
        guard let c = client, let raw = thurm_preview_theme(c, name) else { return nil }
        defer { thurm_string_free(raw) }
        return String(cString: raw)
    }

    func paste(_ pane: UInt64, text: String) {
        guard let c = client else { return }
        thurm_paste(c, pane, text)
    }

    func resize(_ pane: UInt64, cols: Int, rows: Int, cellWidth: Int, cellHeight: Int) {
        guard let c = client else { return }
        thurm_resize(c, pane,
                     UInt16(clamping: cols), UInt16(clamping: rows),
                     UInt16(clamping: cellWidth), UInt16(clamping: cellHeight))
    }

    func focus(_ pane: UInt64, focused: Bool) {
        guard let c = client else { return }
        thurm_focus(c, pane, focused)
    }

    func key(_ pane: UInt64, kind: UInt32, code: UInt32, mods: UInt8, action: UInt8,
             text: String?, shifted: UInt32, baseLayout: UInt32) {
        guard let c = client else { return }
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
                thurm_key(c, pane, &ev)
            }
        } else {
            ev.text = nil
            thurm_key(c, pane, &ev)
        }
    }

    func mouse(_ pane: UInt64, kind: UInt8, button: UInt8, mods: UInt8, clicks: Int,
               col: Int, row: Int, rightHalf: Bool, x: Int, y: Int) {
        guard let c = client else { return }
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
        thurm_mouse(c, pane, &ev)
    }

    func wheel(_ pane: UInt64, lines: Int, col: Int, row: Int, mods: UInt8) {
        guard let c = client, lines != 0 else { return }
        thurm_wheel(c, pane, Int32(clamping: lines),
                    UInt16(clamping: max(0, col)), UInt16(clamping: max(0, row)), mods)
    }
}
