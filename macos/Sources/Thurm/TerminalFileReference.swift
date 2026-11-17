import CryptoKit
import Foundation

/// A file reference from the clicked terminal cell.
enum TerminalFileReference: Equatable {
    case path(String)
    case fileURL(URL)

    private static let tokens = try! NSRegularExpression(
        pattern: #""[^"\r\n]+"|'[^'\r\n]+'|`[^`\r\n]+`|(?:\\[^\r\n]|[^\s<>"'`])+"#)
    private static let locationSuffix = try! NSRegularExpression(pattern: #":\d+(?::\d+)?$"#)

    private static func withoutLocationSuffix(_ path: String) -> String {
        locationSuffix.stringByReplacingMatches(in: path,
            range: NSRange(location: 0, length: (path as NSString).length), withTemplate: "")
    }

    /// The column map uses UTF-16 offsets, as does the terminal row text.
    static func at(text: String, columns: [Int], column: Int, link: URL?) -> Self? {
        if let link { return link.isFileURL ? .fileURL(link) : nil }
        let ns = text as NSString
        for match in tokens.matches(in: text, range: NSRange(location: 0, length: ns.length)) {
            let range = match.range
            guard range.location + range.length <= columns.count,
                  column >= columns[range.location],
                  column <= columns[range.location + range.length - 1] else { continue }
            var token = ns.substring(with: range)
            let quoted = token.first == "\"" || token.first == "'" || token.first == "`"
            if quoted {
                token = String(token.dropFirst().dropLast())
            } else {
                while let first = token.first, ["(", "[", "{"].contains(first) { token.removeFirst() }
                while let last = token.last {
                    if ".,;:!?".contains(last) { token.removeLast(); continue }
                    let pairs: [Character: Character] = [")": "(", "]": "[", "}": "{"]
                    if let opening = pairs[last],
                       token.filter({ $0 == last }).count > token.filter({ $0 == opening }).count {
                        token.removeLast()
                        continue
                    }
                    break
                }
                // Remove shell escapes without evaluating shell input.
                var decoded = ""
                var escaped = false
                for character in token {
                    if escaped { decoded.append(character); escaped = false }
                    else if character == "\\" { escaped = true }
                    else { decoded.append(character) }
                }
                if escaped { decoded.append("\\") }
                token = decoded
            }
            if token.lowercased().hasPrefix("file:"), let url = URL(string: token) {
                return .fileURL(url)
            }
            guard !token.isEmpty,
                  token.contains("/") || token.hasPrefix(".") || !(token as NSString).pathExtension.isEmpty else {
                return nil
            }
            return .path(token)
        }
        return nil
    }

    /// Resolve against the clicked pane, never the app's working directory.
    func resolve(cwd: String?, isRemote: Bool,
                 home: String = FileManager.default.homeDirectoryForCurrentUser.path,
                 localHostName: String = ProcessInfo.processInfo.hostName) throws -> URL {
        guard !isRemote else { throw FilePreviewError.remote }
        let path: String
        switch self {
        case .fileURL(let url):
            guard url.isFileURL,
                  let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
                  parts.user == nil, parts.password == nil, parts.port == nil,
                  parts.query == nil, parts.fragment == nil else { throw FilePreviewError.invalid }
            let host = (parts.host ?? "").lowercased()
            guard host.isEmpty || host == "localhost" || host == localHostName.lowercased() else {
                throw FilePreviewError.remote
            }
            path = parts.path
            guard path.hasPrefix("/") else { throw FilePreviewError.invalid }
        case .path(let value):
            guard !value.contains("://") else { throw FilePreviewError.remote }
            // A colon before the first slash identifies an SCP host, unless it is a
            // diagnostic suffix. Use ./ or an absolute path for a local colon filename.
            let base = Self.withoutLocationSuffix(value)
            if let colon = base.firstIndex(of: ":"), !base[..<colon].contains("/") {
                throw FilePreviewError.remote
            }
            if value.hasPrefix("~/") {
                path = home + "/" + value.dropFirst(2)
            } else if value.hasPrefix("~") {
                throw FilePreviewError.invalid
            } else if value.hasPrefix("/") {
                path = value
            } else {
                guard let cwd, cwd.hasPrefix("/"), !cwd.hasPrefix("//"), !cwd.contains("\0") else {
                    throw FilePreviewError.noDirectory
                }
                path = cwd + "/" + value
            }
        }
        guard !path.contains("\0"), !path.hasPrefix("//") else { throw FilePreviewError.invalid }
        return URL(fileURLWithPath: path).standardizedFileURL
    }

    /// The paths to ask a remote pane's host for, in order: the reference as written (relative
    /// to the pane's directory, `~` for home), then without a diagnostic line suffix. A
    /// `file://` URL names a file on the host that printed it, whatever host it carries.
    func remotePaths() throws -> [String] {
        let path: String
        switch self {
        case .fileURL(let url):
            guard url.isFileURL,
                  let parts = URLComponents(url: url, resolvingAgainstBaseURL: false),
                  parts.user == nil, parts.password == nil, parts.port == nil,
                  parts.query == nil, parts.fragment == nil, parts.path.hasPrefix("/") else {
                throw FilePreviewError.invalid
            }
            path = parts.path
        case .path(let value):
            guard !value.contains("://") else { throw FilePreviewError.invalid }
            let base = Self.withoutLocationSuffix(value)
            if let colon = base.firstIndex(of: ":"), !base[..<colon].contains("/") {
                throw FilePreviewError.invalid
            }
            guard !value.hasPrefix("~") || value.hasPrefix("~/") else { throw FilePreviewError.invalid }
            path = value
        }
        guard !path.isEmpty, !path.contains("\0") else { throw FilePreviewError.invalid }
        let stripped = Self.withoutLocationSuffix(path)
        return stripped == path || stripped.isEmpty ? [path] : [path, stripped]
    }

    /// Keep a literal filename if it exists. Otherwise remove a diagnostic line suffix.
    func localFile(cwd: String?, isRemote: Bool) throws -> URL {
        var url = try resolve(cwd: cwd, isRemote: isRemote)
        let fm = FileManager.default
        if !fm.fileExists(atPath: url.path) {
            // Resolve and validate the host before changing the decoded local path.
            let path = url.path
            let stripped = Self.withoutLocationSuffix(path)
            if stripped != path {
                url = URL(fileURLWithPath: stripped).standardizedFileURL
            }
        }
        guard fm.fileExists(atPath: url.path) else { throw FilePreviewError.missing }
        let values = try url.resolvingSymlinksInPath().resourceValues(forKeys: [.isRegularFileKey])
        guard values.isRegularFile == true else { throw FilePreviewError.unsupported }
        guard fm.isReadableFile(atPath: url.path) else { throw FilePreviewError.unreadable }
        return url
    }
}

enum FilePreviewError: Error, LocalizedError {
    case remote, invalid, noDirectory, missing, unsupported, unreadable

    var errorDescription: String? {
        switch self {
        case .remote: return "Quick Look is available only for files on this Mac. Copy the file to this Mac first."
        case .invalid: return "Quick Look cannot use this file path."
        case .noDirectory: return "The terminal directory is unknown. Use an absolute file path."
        case .missing: return "The file was not found. Relative paths use the pane's current directory."
        case .unsupported: return "Quick Look requires a regular file. Folders and special files are not supported."
        case .unreadable: return "The file cannot be read. Check its access permissions."
        }
    }
}

/// Files of remote panes, fetched to preview here: kept in the app's caches directory by host and
/// path, fetched again only when the host says the file changed, and never more than
/// `maxBytes` in all.
final class RemoteFileCache {
    static let shared = RemoteFileCache()

    /// The largest file fetched, and the most kept.
    let maxBytes: UInt64

    let root: URL

    init(root: URL? = nil, maxBytes: UInt64 = 256 << 20) {
        self.maxBytes = maxBytes
        self.root = root ?? FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(Bundle.main.bundleIdentifier ?? "Thurm", isDirectory: true)
            .appendingPathComponent("remote-files", isDirectory: true)
    }

    /// Where the file `path` on `host` (as `cwd` resolves it) is kept: a directory of its own.
    func entry(host: HostId, path: String, cwd: String?) -> URL {
        var key = path
        if !path.hasPrefix("/") && !path.hasPrefix("~") { key = (cwd ?? "") + "/" + path }
        let digest = SHA256.hash(data: Data((host + "\u{0}" + key).utf8))
        let name = digest.prefix(16).map { String(format: "%02x", $0) }.joined()
        return root.appendingPathComponent(name, isDirectory: true)
    }

    /// The cached copy in `entry` and the version it is, if any.
    func cached(_ entry: URL) -> (file: URL, version: String)? {
        guard let version = try? String(contentsOf: entry.appendingPathComponent(".version"), encoding: .utf8),
              let name = try? String(contentsOf: entry.appendingPathComponent(".name"), encoding: .utf8),
              !name.isEmpty, !name.contains("/") else { return nil }
        let file = entry.appendingPathComponent(name)
        guard FileManager.default.fileExists(atPath: file.path) else { return nil }
        return (file, version)
    }

    /// The file named `name` (on the host) to download into `entry`; `entry` is emptied first.
    func prepare(_ entry: URL) throws -> URL {
        let fm = FileManager.default
        try? fm.removeItem(at: entry)
        try fm.createDirectory(at: entry, withIntermediateDirectories: true,
                               attributes: [.posixPermissions: 0o700])
        return entry.appendingPathComponent(".download")
    }

    /// Moves the downloaded file to its own name and records its version (none: fetched again
    /// next time). Returns where it is.
    func store(_ entry: URL, name: String, version: String?) throws -> URL {
        let safe = name.isEmpty || name.contains("/") || name.hasPrefix(".") ? "file" : name
        let file = entry.appendingPathComponent(safe)
        try FileManager.default.moveItem(at: entry.appendingPathComponent(".download"), to: file)
        try safe.write(to: entry.appendingPathComponent(".name"), atomically: true, encoding: .utf8)
        if let version {
            try version.write(to: entry.appendingPathComponent(".version"), atomically: true, encoding: .utf8)
        }
        prune(keeping: entry)
        return file
    }

    /// Forgets the least recently fetched entries until the rest fit in `maxBytes`.
    func prune(keeping keep: URL? = nil) {
        let fm = FileManager.default
        let keys: [URLResourceKey] = [.contentModificationDateKey, .totalFileAllocatedSizeKey, .fileSizeKey]
        guard let entries = try? fm.contentsOfDirectory(at: root, includingPropertiesForKeys: keys) else { return }
        var sized: [(url: URL, date: Date, size: UInt64)] = entries.map { entry in
            let files = (try? fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: keys)) ?? []
            // The cached file; its name and version beside it are a few bytes.
            let size = files.filter { !$0.lastPathComponent.hasPrefix(".") }.reduce(UInt64(0)) { sum, f in
                sum + UInt64((try? f.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
            }
            let date = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate)
                ?? .distantPast
            return (entry, date, size)
        }
        sized.sort { $0.date < $1.date }
        var total = sized.reduce(UInt64(0)) { $0 + $1.size }
        for e in sized where total > maxBytes && e.url.standardizedFileURL != keep?.standardizedFileURL {
            try? fm.removeItem(at: e.url)
            total -= e.size
        }
    }

    /// Fetches `reference`, as `pane` sees it, into the cache (or finds it there unchanged);
    /// `completion` runs on the main thread.
    func fetch(_ reference: TerminalFileReference, pane: PaneKey, cwd: String?,
               completion: @escaping (Result<URL, Error>) -> Void) {
        let paths: [String]
        do { paths = try reference.remotePaths() } catch { return completion(.failure(error)) }
        fetch(paths[...], pane: pane, cwd: cwd, firstError: nil, completion: completion)
    }

    private func fetch(_ paths: ArraySlice<String>, pane: PaneKey, cwd: String?, firstError: String?,
                       completion: @escaping (Result<URL, Error>) -> Void) {
        guard let path = paths.first else {
            return completion(.failure(RemoteFileError.failed(firstError ?? "The file was not found.")))
        }
        let entry = self.entry(host: pane.host, path: path, cwd: cwd)
        let cached = cached(entry)
        let dest: URL
        if cached == nil {
            do { dest = try prepare(entry) } catch { return completion(.failure(error)) }
        } else {
            dest = entry.appendingPathComponent(".download")
        }
        Core.shared.download(pane, path: path, to: dest, ifVersion: cached?.version, maxBytes: maxBytes,
                             onProgress: { _ in }) { [self] outcome in
            if outcome.notModified, let cached {
                try? FileManager.default.setAttributes([.modificationDate: Date()], ofItemAtPath: entry.path)
                return completion(.success(cached.file))
            }
            if let error = outcome.error {
                if outcome.unsupported { return completion(.failure(FilePreviewError.remote)) }
                return fetch(paths.dropFirst(), pane: pane, cwd: cwd, firstError: firstError ?? error,
                             completion: completion)
            }
            do {
                if cached != nil { _ = try prepareKeepingDownload(entry) }
                completion(.success(try store(entry, name: outcome.name ?? "file", version: outcome.version)))
            } catch {
                completion(.failure(error))
            }
        }
    }

    /// Empties `entry` but for the file just downloaded into it.
    private func prepareKeepingDownload(_ entry: URL) throws {
        let fm = FileManager.default
        for f in try fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: nil)
            where f.lastPathComponent != ".download" {
            try fm.removeItem(at: f)
        }
    }
}

enum RemoteFileError: Error, LocalizedError {
    case failed(String)

    var errorDescription: String? {
        switch self {
        case .failed(let why): return why
        }
    }
}
