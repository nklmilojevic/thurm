import Foundation

/// What a `thurm://` URL or a Finder open asks Thurm to do. A request only chooses where a new
/// shell starts: any app can open a URL, so no request carries a command or input for a pane.
enum ExternalRequest: Equatable {
    case tab(cwd: String?)
    case window(cwd: String?)
    case split(cwd: String?, direction: SplitDirName)
    case settings
    case workspace(name: String)
}

enum ItemKind {
    case directory, file
}

/// Parses `thurm://` URLs and files or folders opened from Finder. The file system is asked
/// through `kind`, so the rules can be tested without touching the disk.
enum ExternalRoute {
    static let scheme = "thurm"

    static func localKind(of path: String) -> ItemKind? {
        var isDirectory: ObjCBool = false
        guard FileManager.default.fileExists(atPath: path, isDirectory: &isDirectory) else { return nil }
        return isDirectory.boolValue ? .directory : .file
    }

    /// The request for a `thurm://` URL, or nil when the route is unknown or a part of it is
    /// invalid. Unknown parameters make the whole URL invalid, so `cmd=` is refused, not ignored.
    static func parse(_ url: URL, kind: (String) -> ItemKind? = ExternalRoute.localKind) -> ExternalRequest? {
        guard url.scheme?.lowercased() == scheme,
              let components = URLComponents(url: url, resolvingAgainstBaseURL: false),
              components.user == nil, components.password == nil, components.port == nil,
              components.fragment == nil,
              let route = components.host?.lowercased()
        else { return nil }
        var params: [String: String] = [:]
        for item in components.queryItems ?? [] {
            guard params[item.name] == nil else { return nil }
            params[item.name] = item.value ?? ""
        }
        let names = Set(params.keys)
        let path = params["path"]
        let pathValid = path.map { isDirectory($0, kind) } ?? true
        let noSubpath = components.path.isEmpty
        switch route {
        case "open" where noSubpath && names.isSubset(of: ["path"]) && pathValid:
            return .tab(cwd: path)
        case "new-window" where noSubpath && names.isSubset(of: ["path"]) && pathValid:
            return .window(cwd: path)
        case "split" where noSubpath && names.isSubset(of: ["path", "direction"]) && pathValid:
            switch params["direction"] ?? "right" {
            case "right": return .split(cwd: path, direction: .right)
            case "down": return .split(cwd: path, direction: .down)
            default: return nil
            }
        case "settings" where noSubpath && names.isEmpty:
            return .settings
        case "workspace" where names.isEmpty && components.path.hasPrefix("/"):
            let name = String(components.path.dropFirst())
            guard !name.isEmpty, !name.contains("/") else { return nil }
            return .workspace(name: name)
        default:
            return nil
        }
    }

    /// The folder a shell starts in for a file or folder opened from Finder (Open With, a Dock
    /// drop, `open -a Thurm`): the folder itself, or the folder that holds the file.
    static func directory(forOpened url: URL, kind: (String) -> ItemKind? = ExternalRoute.localKind) -> String? {
        guard url.isFileURL else { return nil }
        switch kind(url.path) {
        case .directory?: return url.path
        case .file?: return (url.path as NSString).deletingLastPathComponent
        case nil: return nil
        }
    }

    private static func isDirectory(_ path: String, _ kind: (String) -> ItemKind?) -> Bool {
        path.hasPrefix("/") && !path.contains("\0") && kind(path) == .directory
    }
}
