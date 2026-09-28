import Foundation

// MARK: - Logging

/// NSLog wrapper that never interprets the message as a format string.
func tlog(_ message: String) {
    NSLog("%@", "Thurm: " + message)
}

// MARK: - Constants mirrored from thurm.h
//
// The C enums in thurm.h are anonymous, which Swift imports with platform dependent integer
// types. Mirroring the values here keeps the Swift code free of casts.

enum CellFlag {
    static let bold: UInt16 = 1 << 0
    static let italic: UInt16 = 1 << 1
    static let underline: UInt16 = 1 << 2
    static let doubleUnderline: UInt16 = 1 << 3
    static let undercurl: UInt16 = 1 << 4
    static let dottedUnderline: UInt16 = 1 << 5
    static let dashedUnderline: UInt16 = 1 << 6
    static let strikeout: UInt16 = 1 << 7
    static let wide: UInt16 = 1 << 8
    static let wideSpacer: UInt16 = 1 << 9
    static let hidden: UInt16 = 1 << 10
    static let selected: UInt16 = 1 << 11
    static let searchMatch: UInt16 = 1 << 12
    static let defaultBackground: UInt16 = 1 << 13
    static let dim: UInt16 = 1 << 14
    static let searchFocus: UInt16 = 1 << 15
}

/// `THURM_NO_COLOR`: underline color "use fg".
let kNoColor: UInt32 = 0xFF00_0000

enum TermMode {
    static let showCursor: UInt32 = 1 << 0
    static let appCursor: UInt32 = 1 << 1
    static let mouseReportClick: UInt32 = 1 << 3
    static let bracketedPaste: UInt32 = 1 << 4
    static let mouseMotion: UInt32 = 1 << 6
    static let focusInOut: UInt32 = 1 << 11
    static let altScreen: UInt32 = 1 << 12
    static let mouseDrag: UInt32 = 1 << 13
    static let mouseAny: UInt32 = (1 << 3) | (1 << 6) | (1 << 13)
}

enum CursorShapeCode {
    static let block: UInt8 = 0
    static let underline: UInt8 = 1
    static let beam: UInt8 = 2
    static let hollowBlock: UInt8 = 3
    static let hidden: UInt8 = 4
}

enum KeyMods {
    static let shift: UInt8 = 1
    static let alt: UInt8 = 2
    static let ctrl: UInt8 = 4
    static let superKey: UInt8 = 8
    static let capsLock: UInt8 = 64
}

enum KeyActionCode {
    static let press: UInt8 = 0
    static let repeating: UInt8 = 1
    static let release: UInt8 = 2
}

enum KeyKind {
    static let text: UInt32 = 0
    static let named: UInt32 = 1
}

enum MouseKindCode {
    static let press: UInt8 = 0
    static let release: UInt8 = 1
    static let move: UInt8 = 2
}

enum MouseButtonCode {
    static let left: UInt8 = 0
    static let middle: UInt8 = 1
    static let right: UInt8 = 2
    static let back: UInt8 = 3
    static let forward: UInt8 = 4
    static let none: UInt8 = 5
}

// MARK: - JSON helpers

/// An externally tagged serde enum value split into variant name and payload.
struct Variant {
    let name: String
    let payload: Any?
}

enum JSON {
    /// Serializes a JSONSerialization-compatible value (use NSNull for `null`).
    static func encode(_ object: Any) -> String {
        guard let data = try? JSONSerialization.data(withJSONObject: object, options: [.fragmentsAllowed]),
              let text = String(data: data, encoding: .utf8)
        else {
            tlog("could not encode JSON object \(object)")
            return "null"
        }
        return text
    }

    static func decode(_ text: String) -> Any? {
        guard let data = text.data(using: .utf8) else { return nil }
        return try? JSONSerialization.jsonObject(with: data, options: [.fragmentsAllowed])
    }

    /// `"Ok"` → ("Ok", nil); `{"PaneCreated":{"pane":1}}` → ("PaneCreated", {...}).
    static func variant(_ value: Any?) -> Variant? {
        if let name = value as? String {
            return Variant(name: name, payload: nil)
        }
        if let dict = value as? [String: Any], dict.count == 1, let first = dict.first {
            return Variant(name: first.key, payload: first.value)
        }
        return nil
    }
}

func jsonUInt64(_ value: Any?) -> UInt64? { (value as? NSNumber)?.uint64Value }
func jsonInt(_ value: Any?) -> Int? { (value as? NSNumber)?.intValue }
func jsonDouble(_ value: Any?) -> Double? { (value as? NSNumber)?.doubleValue }
func jsonBool(_ value: Any?) -> Bool? { (value as? NSNumber)?.boolValue }
func jsonString(_ value: Any?) -> String? { value as? String }

/// Returns `value` or NSNull, for building request dictionaries.
func orNull(_ value: Any?) -> Any { value ?? NSNull() }

// MARK: - Pane metadata

enum AgentStatus: String {
    case working = "Working"
    case idle = "Idle"
    case needsInput = "NeedsInput"
    /// Finished a turn the user hasn't looked at (hooks only).
    case done = "Done"

    /// Higher is more urgent (used to summarize a tab), like tty7: waiting > working > done.
    var urgency: Int {
        switch self {
        case .idle: return 0
        case .done: return 1
        case .working: return 2
        case .needsInput: return 3
        }
    }
}

struct AgentState: Equatable {
    var name: String
    var kind: String
    var status: AgentStatus
    /// From the agent's hooks.
    var sessionId: String?
    var message: String?
    var turnMs: UInt64?
    var turns: Int = 0
    var hooked = false
    /// What the session is about (the agent's own session title).
    var topic: String? = nil
}

/// Swift view of `thurm_proto::GitInfo`.
struct GitInfo: Equatable {
    var root: String
    var branch: String
    var added: UInt32
    var removed: UInt32
}

/// Swift view of `thurm_proto::Progress` (ConEmu OSC 9;4).
struct ProgressReport: Equatable {
    enum State: String {
        case normal, error, indeterminate, paused
    }
    var state: State
    /// 0-100, when the program gave one.
    var percent: Int?
}

/// Swift view of `thurm_proto::PaneInfo`.
struct PaneInfo {
    var id: UInt64
    var title: String
    var cwd: String?
    var foregroundName: String?
    var agent: AgentState?
    var alive: Bool
    var passwordInput: Bool
    var atPrompt: Bool
    var restored: Bool
    var progress: ProgressReport?
    var git: GitInfo?

    init?(json: Any?) {
        guard let d = json as? [String: Any], let id = jsonUInt64(d["id"]) else { return nil }
        self.id = id
        title = jsonString(d["title"]) ?? ""
        cwd = jsonString(d["cwd"])
        if let fg = d["foreground"] as? [String: Any] {
            foregroundName = jsonString(fg["name"])
        } else {
            foregroundName = nil
        }
        if let a = d["agent"] as? [String: Any],
           let statusName = jsonString(a["status"]),
           let status = AgentStatus(rawValue: statusName) {
            agent = AgentState(name: jsonString(a["name"]) ?? "Agent",
                               kind: jsonString(a["kind"]) ?? "",
                               status: status,
                               sessionId: jsonString(a["session_id"]),
                               message: jsonString(a["message"]),
                               turnMs: jsonUInt64(a["turn_ms"]),
                               turns: jsonInt(a["turns"]) ?? 0,
                               hooked: jsonBool(a["hooked"]) ?? false,
                               topic: jsonString(a["topic"]))
        } else {
            agent = nil
        }
        alive = jsonBool(d["alive"]) ?? true
        passwordInput = jsonBool(d["password_input"]) ?? false
        atPrompt = jsonBool(d["at_prompt"]) ?? false
        restored = jsonBool(d["restored"]) ?? false
        if let p = d["progress"] as? [String: Any],
           let state = jsonString(p["state"]).flatMap(ProgressReport.State.init(rawValue:)) {
            progress = ProgressReport(state: state, percent: jsonInt(p["percent"]))
        } else {
            progress = nil
        }
        if let g = d["git"] as? [String: Any], let root = jsonString(g["root"]) {
            git = GitInfo(root: root, branch: jsonString(g["branch"]) ?? "",
                          added: UInt32(jsonInt(g["added"]) ?? 0), removed: UInt32(jsonInt(g["removed"]) ?? 0))
        } else {
            git = nil
        }
    }

    /// Title for tabs: program title, else the cwd's last path component.
    var displayTitle: String {
        let trimmed = title.trimmingCharacters(in: .whitespacesAndNewlines)
        if !trimmed.isEmpty { return trimmed }
        if let cwd = cwd, !cwd.isEmpty {
            let base = (cwd as NSString).lastPathComponent
            return base.isEmpty ? cwd : base
        }
        return "Thurm"
    }

    private static let shells: Set<String> = [
        "zsh", "bash", "fish", "sh", "dash", "tcsh", "csh", "ksh", "nu", "pwsh", "elvish", "xonsh", "login",
    ]

    /// True when the foreground process is something other than an idle shell.
    var hasRunningProcess: Bool {
        guard alive, var name = foregroundName, !name.isEmpty else { return false }
        if name.hasPrefix("-") { name.removeFirst() }
        name = (name as NSString).lastPathComponent
        return !PaneInfo.shells.contains(name)
    }
}

// MARK: - Layout (mirrors crates/thurm-proto/src/layout.rs)

enum SplitDirName: String, Codable {
    case right, down, left, up
}

indirect enum LayoutNode: Codable, Equatable {
    case pane(id: UInt64)
    case split(dir: SplitDirName, ratio: Double, first: LayoutNode, second: LayoutNode)

    private enum CodingKeys: String, CodingKey {
        case type, id, dir, ratio, first, second
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        let type = try c.decode(String.self, forKey: .type)
        switch type {
        case "pane":
            self = .pane(id: try c.decode(UInt64.self, forKey: .id))
        case "split":
            let dir = try c.decode(SplitDirName.self, forKey: .dir)
            let ratio = try c.decodeIfPresent(Double.self, forKey: .ratio) ?? 0.5
            let first = try c.decode(LayoutNode.self, forKey: .first)
            let second = try c.decode(LayoutNode.self, forKey: .second)
            self = .split(dir: dir, ratio: ratio, first: first, second: second)
        default:
            throw DecodingError.dataCorruptedError(forKey: .type, in: c,
                                                   debugDescription: "unknown layout node \(type)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case .pane(let id):
            try c.encode("pane", forKey: .type)
            try c.encode(id, forKey: .id)
        case .split(let dir, let ratio, let first, let second):
            try c.encode("split", forKey: .type)
            try c.encode(dir, forKey: .dir)
            try c.encode(ratio, forKey: .ratio)
            try c.encode(first, forKey: .first)
            try c.encode(second, forKey: .second)
        }
    }

    var panes: [UInt64] {
        switch self {
        case .pane(let id):
            return [id]
        case .split(_, _, let first, let second):
            return first.panes + second.panes
        }
    }

    /// Drops panes for which `keep` is false, collapsing splits. Nil when nothing is left.
    func retaining(_ keep: (UInt64) -> Bool) -> LayoutNode? {
        switch self {
        case .pane(let id):
            return keep(id) ? self : nil
        case .split(let dir, let ratio, let first, let second):
            let a = first.retaining(keep)
            let b = second.retaining(keep)
            if let a = a, let b = b { return .split(dir: dir, ratio: ratio, first: a, second: b) }
            return a ?? b
        }
    }
}

struct TabLayout: Codable, Equatable {
    var title: String?
    var root: LayoutNode
    var focused: UInt64
    var zoomed: UInt64?
}

struct WindowLayout: Codable, Equatable {
    var frame: [Double]?
    var tabs: [TabLayout]
    var selectedTab: Int
    var fullscreen: Bool
    /// The workspace the window shows (0 = none yet).
    var workspace: UInt64 = 0

    private enum CodingKeys: String, CodingKey {
        case frame, tabs
        case selectedTab = "selected_tab"
        case fullscreen, workspace
    }

    init(frame: [Double]?, tabs: [TabLayout], selectedTab: Int, fullscreen: Bool, workspace: UInt64 = 0) {
        self.frame = frame
        self.tabs = tabs
        self.selectedTab = selectedTab
        self.fullscreen = fullscreen
        self.workspace = workspace
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        frame = try c.decodeIfPresent([Double].self, forKey: .frame)
        tabs = try c.decodeIfPresent([TabLayout].self, forKey: .tabs) ?? []
        selectedTab = try c.decodeIfPresent(Int.self, forKey: .selectedTab) ?? 0
        fullscreen = try c.decodeIfPresent(Bool.self, forKey: .fullscreen) ?? false
        workspace = try c.decodeIfPresent(UInt64.self, forKey: .workspace) ?? 0
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        if let frame = frame {
            try c.encode(frame, forKey: .frame)
        } else {
            try c.encodeNil(forKey: .frame)
        }
        try c.encode(tabs, forKey: .tabs)
        try c.encode(selectedTab, forKey: .selectedTab)
        try c.encode(fullscreen, forKey: .fullscreen)
        try c.encode(workspace, forKey: .workspace)
    }
}

/// `thurm_proto::Workspace`: tabs are only stored here while no window shows it.
struct WorkspaceLayout: Codable, Equatable {
    var id: UInt64
    var name: String
    var lastActive: UInt64
    var tabs: [TabLayout]
    var selectedTab: Int

    private enum CodingKeys: String, CodingKey {
        case id, name, tabs
        case lastActive = "last_active"
        case selectedTab = "selected_tab"
    }

    init(id: UInt64, name: String, lastActive: UInt64, tabs: [TabLayout], selectedTab: Int) {
        self.id = id
        self.name = name
        self.lastActive = lastActive
        self.tabs = tabs
        self.selectedTab = selectedTab
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(UInt64.self, forKey: .id)
        name = try c.decodeIfPresent(String.self, forKey: .name) ?? ""
        lastActive = try c.decodeIfPresent(UInt64.self, forKey: .lastActive) ?? 0
        tabs = try c.decodeIfPresent([TabLayout].self, forKey: .tabs) ?? []
        selectedTab = try c.decodeIfPresent(Int.self, forKey: .selectedTab) ?? 0
    }
}

/// Same semantics as `retain_tabs` on the Rust side.
func retainTabs(_ tabs: [TabLayout], _ keep: (UInt64) -> Bool) -> [TabLayout] {
    tabs.compactMap { tab in
        var tab = tab
        guard let root = tab.root.retaining(keep) else { return nil }
        let ids = root.panes
        if !ids.contains(tab.focused), let first = ids.first { tab.focused = first }
        if let z = tab.zoomed, !ids.contains(z) { tab.zoomed = nil }
        tab.root = root
        return tab
    }
}

struct Layout: Codable, Equatable {
    var windows: [WindowLayout]
    var workspaces: [WorkspaceLayout] = []

    private enum CodingKeys: String, CodingKey {
        case windows, workspaces
    }

    init(windows: [WindowLayout], workspaces: [WorkspaceLayout] = []) {
        self.windows = windows
        self.workspaces = workspaces
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        windows = try c.decodeIfPresent([WindowLayout].self, forKey: .windows) ?? []
        workspaces = try c.decodeIfPresent([WorkspaceLayout].self, forKey: .workspaces) ?? []
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(windows, forKey: .windows)
        try c.encode(workspaces, forKey: .workspaces)
    }

    var panes: [UInt64] {
        (windows.flatMap { $0.tabs } + workspaces.flatMap { $0.tabs }).flatMap { $0.root.panes }
    }

    /// Same semantics as `Layout::retain_panes` on the Rust side.
    mutating func retainPanes(_ keep: (UInt64) -> Bool) {
        for wi in windows.indices {
            windows[wi].tabs = retainTabs(windows[wi].tabs, keep)
            if windows[wi].selectedTab >= windows[wi].tabs.count {
                windows[wi].selectedTab = max(0, windows[wi].tabs.count - 1)
            }
        }
        windows.removeAll { $0.tabs.isEmpty }
        for i in workspaces.indices {
            workspaces[i].tabs = retainTabs(workspaces[i].tabs, keep)
            if workspaces[i].selectedTab >= workspaces[i].tabs.count {
                workspaces[i].selectedTab = max(0, workspaces[i].tabs.count - 1)
            }
        }
        let shown = Set(windows.map { $0.workspace })
        workspaces.removeAll { $0.tabs.isEmpty && !shown.contains($0.id) }
    }

    func jsonString() -> String? {
        let encoder = JSONEncoder()
        encoder.outputFormatting = [.sortedKeys]
        guard let data = try? encoder.encode(self) else { return nil }
        return String(data: data, encoding: .utf8)
    }

    static func from(json: String) -> Layout? {
        guard let data = json.data(using: .utf8) else { return nil }
        do {
            return try JSONDecoder().decode(Layout.self, from: data)
        } catch {
            tlog("could not decode stored layout: \(error)")
            return nil
        }
    }
}
