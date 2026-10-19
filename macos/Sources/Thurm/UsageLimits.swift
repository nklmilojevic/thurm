import AppKit

/// A subscription whose limits the sidebar shows (`window.sidebar_usage`).
enum UsageProvider: String, CaseIterable {
    case claude
    case codex

    var name: String {
        switch self {
        case .claude: "Claude"
        case .codex: "Codex"
        }
    }

    var icon: PaneIcon {
        switch self {
        case .claude: .claude
        case .codex: .codex
        }
    }
}

/// One rate-limit window: Claude's and Codex's 5-hour and weekly limits.
struct UsageWindow: Equatable {
    var minutes: Int
    /// 0…100.
    var usedPercent: Double
    var resetsAt: Date? = nil

    /// "5h", "wk", "3d".
    var label: String {
        if minutes == 7 * 24 * 60 { return "wk" }
        if minutes % (24 * 60) == 0 { return "\(minutes / (24 * 60))d" }
        if minutes >= 60 { return "\(Int((Double(minutes) / 60).rounded()))h" }
        return "\(minutes)m"
    }

    /// "5-hour", "Weekly".
    var title: String {
        if minutes == 7 * 24 * 60 { return "Weekly" }
        if minutes % (24 * 60) == 0 { return "\(minutes / (24 * 60))-day" }
        if minutes >= 60 { return "\(Int((Double(minutes) / 60).rounded()))-hour" }
        return "\(minutes)-minute"
    }

    /// What is left, 0…100: all of it once the window has reset.
    func leftPercent(at now: Date) -> Double {
        if let resetsAt, resetsAt <= now { return 100 }
        return min(100, max(0, 100 - usedPercent))
    }
}

/// What is known of one provider's limits.
struct UsageReport: Equatable {
    var windows: [UsageWindow]
    /// When `windows` were read (for Codex: when its session log recorded them).
    var updated: Date?
    /// Why the last refresh failed; `windows` are then the last ones known.
    var problem: String?
}

/// Reads what is left of the Claude Code and Codex subscription limits for the sidebar.
///
/// Claude's come from Anthropic's usage endpoint (what Claude Code's `/usage` shows), with the
/// sign-in Claude Code keeps in the keychain. It is only read: Claude Code renews it when it runs.
/// Codex's come from the latest rate-limit snapshot Codex wrote to its session logs, so they
/// cover what this Mac's Codex last saw.
final class UsageLimits {
    static let shared = UsageLimits()

    private(set) var reports: [UsageProvider: UsageReport] = [:]
    private var timer: Timer?
    private let queue = DispatchQueue(label: "com.thurm.usage-limits", qos: .utility)
    private var lastTick = Date()
    private var claudeNext = Date.distantPast
    private var claudeLast = Date.distantPast
    private var claudeInFlight = false
    private var codexInFlight = false
    private var codexSeen: (path: String, modified: Date)?

    private static let tick: TimeInterval = 30
    private static let claudeInterval: TimeInterval = 5 * 60
    private static let claudeBackoff: TimeInterval = 15 * 60
    private static let claudeMinimum: TimeInterval = 60
    /// After finding no sign-in: reading the keychain again may ask for it again.
    private static let claudeSignedOutInterval: TimeInterval = 30 * 60

    private var enabled: Bool {
        let config = SessionManager.shared.config
        return config.sidebarUsage && config.sidebarTabs
    }

    /// Starts or stops polling after the config changed.
    func configChanged() {
        if enabled {
            guard timer == nil else { return }
            let t = Timer(timeInterval: Self.tick, repeats: true) { [weak self] _ in self?.poll() }
            t.tolerance = 5
            RunLoop.main.add(t, forMode: .common)
            timer = t
            poll()
        } else {
            timer?.invalidate()
            timer = nil
            claudeNext = .distantPast
            codexSeen = nil
            if !reports.isEmpty {
                reports = [:]
                SessionManager.shared.refreshSidebars()
            }
        }
    }

    /// Reads both again now (Claude at most once a minute).
    func refreshNow() {
        guard enabled else { return }
        claudeNext = min(claudeNext, claudeLast.addingTimeInterval(Self.claudeMinimum))
        codexSeen = nil
        poll()
    }

    private func poll() {
        let now = Date()
        // A window that reset since the last tick shows full again.
        let crossed = reports.values.contains { r in
            r.windows.contains { w in w.resetsAt.map { $0 > lastTick && $0 <= now } ?? false }
        }
        lastTick = now
        if crossed { SessionManager.shared.refreshSidebars() }
        if !codexInFlight {
            codexInFlight = true
            let seen = codexSeen
            queue.async {
                let found = Self.latestCodexSnapshot(unchangedSince: seen)
                DispatchQueue.main.async {
                    self.codexInFlight = false
                    guard self.enabled else { return }
                    switch found {
                    case .unchanged: break
                    case .none: self.set(.codex, nil)
                    case .snapshot(let path, let modified, let report):
                        self.codexSeen = (path, modified)
                        self.set(.codex, report)
                    }
                }
            }
        }
        if !claudeInFlight, now >= claudeNext {
            claudeInFlight = true
            claudeLast = now
            claudeNext = now.addingTimeInterval(Self.claudeInterval)
            queue.async {
                Self.fetchClaude { result in
                    DispatchQueue.main.async {
                        self.claudeInFlight = false
                        guard self.enabled else { return }
                        self.applyClaude(result)
                    }
                }
            }
        }
    }

    private func applyClaude(_ result: ClaudeResult) {
        switch result {
        case .signedOut:
            claudeNext = Date().addingTimeInterval(Self.claudeSignedOutInterval)
            set(.claude, nil)
        case .windows(let windows):
            set(.claude, UsageReport(windows: windows, updated: Date(), problem: nil))
        case .failed(let problem, let backOff):
            if backOff { claudeNext = Date().addingTimeInterval(Self.claudeBackoff) }
            var report = reports[.claude] ?? UsageReport(windows: [], updated: nil)
            report.problem = problem
            set(.claude, report)
        }
    }

    private func set(_ provider: UsageProvider, _ report: UsageReport?) {
        guard reports[provider] != report else { return }
        reports[provider] = report
        SessionManager.shared.refreshSidebars()
    }

    // MARK: Claude

    enum ClaudeResult: Equatable {
        /// No Claude Code sign-in on this Mac.
        case signedOut
        case windows([UsageWindow])
        case failed(String, backOff: Bool)
    }

    private static func fetchClaude(_ done: @escaping (ClaudeResult) -> Void) {
        guard let raw = claudeCredentials(), let credentials = parseClaudeCredentials(raw) else {
            return done(.signedOut)
        }
        if let expires = credentials.expiresAt, expires <= Date() {
            return done(.failed("Claude Code's sign-in has expired; it renews the next time Claude Code runs",
                                backOff: false))
        }
        var request = URLRequest(url: URL(string: "https://api.anthropic.com/api/oauth/usage")!)
        request.timeoutInterval = 20
        request.setValue("Bearer \(credentials.token)", forHTTPHeaderField: "Authorization")
        request.setValue("oauth-2025-04-20", forHTTPHeaderField: "anthropic-beta")
        request.setValue("application/json", forHTTPHeaderField: "Accept")
        let version = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
        request.setValue("Thurm/\(version)", forHTTPHeaderField: "User-Agent")
        URLSession.shared.dataTask(with: request) { data, response, error in
            let status = (response as? HTTPURLResponse)?.statusCode ?? 0
            switch status {
            case 200:
                guard let data, let windows = parseClaudeUsage(data) else {
                    return done(.failed("Anthropic's answer could not be read", backOff: true))
                }
                done(.windows(windows))
            case 401, 403:
                done(.failed("Claude Code's sign-in was refused; it renews the next time Claude Code runs",
                             backOff: false))
            case 429:
                done(.failed("Anthropic is rate limiting usage checks; trying again later", backOff: true))
            case 0:
                done(.failed("Couldn't reach Anthropic: \(error?.localizedDescription ?? "no answer")", backOff: false))
            default:
                done(.failed("Anthropic answered \(status)", backOff: true))
            }
        }.resume()
    }

    /// Claude Code's stored sign-in: the keychain item it writes on macOS (read with `security`,
    /// which Claude Code itself uses, so no keychain prompt), else `~/.claude/.credentials.json`.
    private static func claudeCredentials() -> Data? {
        let process = Process()
        process.executableURL = URL(fileURLWithPath: "/usr/bin/security")
        process.arguments = ["find-generic-password", "-s", "Claude Code-credentials", "-w"]
        let out = Pipe()
        process.standardOutput = out
        process.standardError = FileHandle.nullDevice
        if (try? process.run()) != nil {
            let data = out.fileHandleForReading.readDataToEndOfFile()
            process.waitUntilExit()
            if process.terminationStatus == 0, !data.isEmpty { return data }
        }
        let dir = ProcessInfo.processInfo.environment["CLAUDE_CONFIG_DIR"]
            .map { URL(fileURLWithPath: $0) }
            ?? FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent(".claude")
        return try? Data(contentsOf: dir.appendingPathComponent(".credentials.json"))
    }

    static func parseClaudeCredentials(_ data: Data) -> (token: String, expiresAt: Date?)? {
        var data = data
        // `security -w` prints a value it takes for binary as hex.
        if let text = String(data: data, encoding: .utf8)?.trimmingCharacters(in: .whitespacesAndNewlines),
           !text.hasPrefix("{"), let decoded = Data(hex: text) {
            data = decoded
        }
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let oauth = root["claudeAiOauth"] as? [String: Any],
              let token = oauth["accessToken"] as? String, !token.isEmpty
        else { return nil }
        let expires = (oauth["expiresAt"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue / 1000) }
        return (token, expires)
    }

    /// `/api/oauth/usage`: `five_hour` and `seven_day`, each `{utilization: 0…100, resets_at}`.
    static func parseClaudeUsage(_ data: Data) -> [UsageWindow]? {
        guard let root = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        return [("five_hour", 5 * 60), ("seven_day", 7 * 24 * 60)].compactMap { key, minutes in
            guard let w = root[key] as? [String: Any],
                  let used = (w["utilization"] as? NSNumber)?.doubleValue
            else { return nil }
            return UsageWindow(minutes: minutes, usedPercent: used,
                               resetsAt: (w["resets_at"] as? String).flatMap(parseDate))
        }
    }

    // MARK: Codex

    enum CodexFound {
        case unchanged
        case snapshot(path: String, modified: Date, UsageReport)
    }

    /// Session logs written in the last days, newest first, for their latest `rate_limits`.
    private static func latestCodexSnapshot(unchangedSince seen: (path: String, modified: Date)?) -> CodexFound? {
        let fm = FileManager.default
        let home = ProcessInfo.processInfo.environment["CODEX_HOME"].map { URL(fileURLWithPath: $0) }
            ?? fm.homeDirectoryForCurrentUser.appendingPathComponent(".codex")
        let sessions = home.appendingPathComponent("sessions")
        // sessions/YYYY/MM/DD/rollout-*.jsonl: the newest days, then the newest files in them.
        var days: [URL] = []
        func children(_ url: URL) -> [URL] {
            ((try? fm.contentsOfDirectory(at: url, includingPropertiesForKeys: nil)) ?? [])
                .sorted { $0.lastPathComponent > $1.lastPathComponent }
        }
        outer: for year in children(sessions) {
            for month in children(year) {
                for day in children(month) {
                    days.append(day)
                    if days.count == 7 { break outer }
                }
            }
        }
        let files = days.flatMap { day in
            children(day).filter { $0.lastPathComponent.hasPrefix("rollout-") && $0.pathExtension == "jsonl" }
        }.compactMap { url -> (URL, Date)? in
            guard let modified = (try? url.resourceValues(forKeys: [.contentModificationDateKey]))?
                .contentModificationDate else { return nil }
            return (url, modified)
        }.sorted { $0.1 > $1.1 }
        if let newest = files.first, let seen, newest.0.path == seen.path, newest.1 == seen.modified {
            return .unchanged
        }
        for (url, modified) in files.prefix(5) {
            if let report = codexSnapshot(tail(of: url)) {
                return .snapshot(path: url.path, modified: modified, report)
            }
        }
        return nil
    }

    private static func tail(of url: URL, bytes: UInt64 = 512 * 1024) -> Data {
        guard let handle = try? FileHandle(forReadingFrom: url) else { return Data() }
        defer { try? handle.close() }
        let size = (try? handle.seekToEnd()) ?? 0
        try? handle.seek(toOffset: size > bytes ? size - bytes : 0)
        return (try? handle.readToEnd()) ?? Data()
    }

    /// The last `token_count` event with Codex's `rate_limits` in a session log's lines:
    /// `primary` and `secondary`, each `{used_percent, window_minutes, resets_at}` (older Codex:
    /// `resets_in_seconds`, from the event's `timestamp`).
    static func codexSnapshot(_ log: Data) -> UsageReport? {
        let lines = log.split(separator: UInt8(ascii: "\n"))
        for line in lines.reversed() {
            guard line.count < 64 * 1024,
                  let text = String(data: line, encoding: .utf8), text.contains("\"rate_limits\""),
                  let event = try? JSONSerialization.jsonObject(with: line) as? [String: Any],
                  let payload = event["payload"] as? [String: Any],
                  payload["type"] as? String == "token_count",
                  let limits = payload["rate_limits"] as? [String: Any]
            else { continue }
            // Other limits (one model's) can come in their own snapshot.
            if let id = limits["limit_id"] as? String, id != "codex" { continue }
            let at = (event["timestamp"] as? String).flatMap(parseDate)
            let windows = ["primary", "secondary"].compactMap { key -> UsageWindow? in
                guard let w = limits[key] as? [String: Any],
                      let used = (w["used_percent"] as? NSNumber)?.doubleValue,
                      let minutes = (w["window_minutes"] as? NSNumber)?.intValue, minutes > 0
                else { return nil }
                var resets = (w["resets_at"] as? NSNumber).map { Date(timeIntervalSince1970: $0.doubleValue) }
                if resets == nil, let at, let secs = (w["resets_in_seconds"] as? NSNumber)?.doubleValue {
                    resets = at.addingTimeInterval(secs)
                }
                return UsageWindow(minutes: minutes, usedPercent: used, resetsAt: resets)
            }.sorted { $0.minutes < $1.minutes }
            guard !windows.isEmpty else { continue }
            return UsageReport(windows: windows, updated: at, problem: nil)
        }
        return nil
    }

    private static func parseDate(_ s: String) -> Date? {
        let f = ISO8601DateFormatter()
        f.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        if let d = f.date(from: s) { return d }
        f.formatOptions = [.withInternetDateTime]
        return f.date(from: s)
    }
}

private extension Data {
    init?(hex: String) {
        guard hex.count % 2 == 0, !hex.isEmpty else { return nil }
        var bytes = [UInt8]()
        bytes.reserveCapacity(hex.count / 2)
        var i = hex.startIndex
        while i < hex.endIndex {
            let j = hex.index(i, offsetBy: 2)
            guard let b = UInt8(hex[i..<j], radix: 16) else { return nil }
            bytes.append(b)
            i = j
        }
        self.init(bytes)
    }
}

// MARK: - Sidebar

/// The bottom of the sidebar: a row per provider, a ring and what is left for each window.
final class UsagePanel: NSView {
    private let separator = NSBox()
    private let stack = NSStackView()
    private var shown: [UsageRowModel] = []
    private var themed: Theme?
    /// Holds the panel at no height while it is hidden.
    private var collapsed: NSLayoutConstraint!

    init() {
        super.init(frame: .zero)
        separator.boxType = .separator
        stack.orientation = .vertical
        stack.alignment = .leading
        stack.spacing = 4
        for v in [separator, stack] as [NSView] {
            v.translatesAutoresizingMaskIntoConstraints = false
            addSubview(v)
        }
        // Below the collapsed height, which wins while the panel is hidden.
        let bottom = stack.bottomAnchor.constraint(equalTo: bottomAnchor)
        bottom.priority = .defaultHigh
        collapsed = heightAnchor.constraint(equalToConstant: 0)
        NSLayoutConstraint.activate([
            bottom,
            collapsed,
            separator.topAnchor.constraint(equalTo: topAnchor),
            separator.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 12),
            separator.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -12),
            stack.topAnchor.constraint(equalTo: separator.bottomAnchor, constant: 8),
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 16),
            stack.trailingAnchor.constraint(lessThanOrEqualTo: trailingAnchor, constant: -12),
        ])
        let click = NSClickGestureRecognizer(target: self, action: #selector(clicked))
        addGestureRecognizer(click)
        isHidden = true
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    @objc private func clicked() {
        UsageLimits.shared.refreshNow()
    }

    func reload() {
        let config = SessionManager.shared.config
        let now = Date()
        let models = config.sidebarUsage
            ? UsageProvider.allCases.compactMap { p in
                UsageLimits.shared.reports[p].map { UsageRowModel(provider: p, report: $0, now: now) }
            }
            : []
        guard models != shown || config.theme != themed else { return }
        shown = models
        themed = config.theme
        stack.arrangedSubviews.forEach { $0.removeFromSuperview() }
        for m in models {
            stack.addArrangedSubview(UsageRow(m, chrome: config.chrome))
        }
        isHidden = models.isEmpty
        collapsed.isActive = models.isEmpty
    }
}

/// What one row shows, compared to skip rebuilding a row that would look the same.
struct UsageRowModel: Equatable {
    struct Gauge: Equatable {
        var label: String
        var left: Int
    }

    var provider: UsageProvider
    var gauges: [Gauge]
    var stale: Bool
    var tooltip: String

    init(provider: UsageProvider, report: UsageReport, now: Date) {
        self.provider = provider
        gauges = report.windows.map { Gauge(label: $0.label, left: Int($0.leftPercent(at: now).rounded(.down))) }
        stale = report.problem != nil
        var lines = [provider.name]
        let time = DateFormatter()
        time.doesRelativeDateFormatting = true
        time.dateStyle = .short
        time.timeStyle = .short
        for w in report.windows {
            var line = "\(w.title): \(Int(w.leftPercent(at: now).rounded(.down)))% left"
            if let r = w.resetsAt, r > now { line += ", resets \(time.string(from: r))" }
            lines.append(line)
        }
        if report.windows.isEmpty { lines.append("No usage known yet") }
        if let updated = report.updated { lines.append("Updated \(time.string(from: updated))") }
        if let problem = report.problem { lines.append(problem) }
        lines.append("Click to refresh")
        tooltip = lines.joined(separator: "\n")
    }
}

private final class UsageRow: NSStackView {
    init(_ model: UsageRowModel, chrome: ChromePalette) {
        super.init(frame: .zero)
        orientation = .horizontal
        spacing = 4
        toolTip = model.tooltip
        let logo = NSImageView(image: model.provider.icon.glyph(size: 12))
        logo.contentTintColor = model.stale ? chrome.contentDisabled : chrome.contentSecondary
        logo.setAccessibilityLabel(model.provider.name)
        addArrangedSubview(logo)
        setCustomSpacing(8, after: logo)
        if model.gauges.isEmpty {
            addArrangedSubview(Self.text("—", color: chrome.contentTertiary))
        }
        for (i, g) in model.gauges.enumerated() {
            let color = model.stale ? chrome.contentDisabled : Self.color(left: g.left, chrome: chrome)
            let ring = UsageRing(left: CGFloat(g.left) / 100, color: color, track: chrome.divider)
            addArrangedSubview(ring)
            let label = Self.text("\(g.label) \(g.left)%", color: model.stale ? chrome.contentDisabled
                                    : g.left < 25 ? color : chrome.contentSecondary)
            label.setAccessibilityLabel("\(model.provider.name) \(g.label): \(g.left)% left")
            addArrangedSubview(label)
            if i < model.gauges.count - 1 { setCustomSpacing(10, after: label) }
        }
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    static func color(left: Int, chrome: ChromePalette) -> NSColor {
        left < 10 ? chrome.danger : left < 25 ? chrome.warning : chrome.contentSecondary
    }

    private static func text(_ s: String, color: NSColor) -> NSTextField {
        let label = NSTextField(labelWithString: s)
        label.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        label.textColor = color
        return label
    }
}

/// A ring filled clockwise from the top by what is left.
final class UsageRing: NSView {
    let left: CGFloat
    private let color: NSColor
    private let track: NSColor

    init(left: CGFloat, color: NSColor, track: NSColor) {
        self.left = min(1, max(0, left))
        self.color = color
        self.track = track
        super.init(frame: NSRect(x: 0, y: 0, width: 11, height: 11))
        setAccessibilityElement(false)
    }

    required init?(coder: NSCoder) {
        fatalError("init(coder:) is not supported")
    }

    override var intrinsicContentSize: NSSize { NSSize(width: 11, height: 11) }

    override func draw(_ dirtyRect: NSRect) {
        let width: CGFloat = 2
        let rect = bounds.insetBy(dx: width / 2, dy: width / 2)
        let center = NSPoint(x: rect.midX, y: rect.midY)
        let radius = rect.width / 2
        track.setStroke()
        let full = NSBezierPath(ovalIn: rect)
        full.lineWidth = width
        full.stroke()
        guard left > 0 else { return }
        let arc = NSBezierPath()
        arc.appendArc(withCenter: center, radius: radius, startAngle: 90, endAngle: 90 - 360 * left, clockwise: true)
        arc.lineWidth = width
        arc.lineCapStyle = .round
        color.setStroke()
        arc.stroke()
    }
}
