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
            // A URL is not a path; an absolute path may hold `://` in a name.
            guard value.hasPrefix("/") || !value.contains("://") else { throw FilePreviewError.invalid }
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
/// `maxBytes` in all, downloads under way included.
///
/// An entry is a directory per host and path. Each fetched copy goes in a folder of its own
/// inside it (`c-<id>/<name>`), and `.current` names the one in use: a refresh never replaces a
/// file Quick Look is showing, and a copy it shows is deleted only once let go. Main thread only.
final class RemoteFileCache {
    static let shared = RemoteFileCache()

    /// The largest file fetched, and the most kept and on its way.
    let maxBytes: UInt64

    let root: URL

    init(root: URL? = nil, maxBytes: UInt64 = 256 << 20) {
        self.maxBytes = maxBytes
        self.root = root ?? FileManager.default.urls(for: .cachesDirectory, in: .userDomainMask)[0]
            .appendingPathComponent(Bundle.main.bundleIdentifier ?? "Thurm", isDirectory: true)
            .appendingPathComponent("remote-files", isDirectory: true)
        // What an earlier run left half-downloaded goes now, and so does what no longer fits.
        prune()
    }

    /// Whether `file` in an entry is a download's: one written to (`.staging-…`), or the part
    /// the transfer writes beside it (`.thurm-<pid>-<n>.part`).
    private static func isDownload(_ file: URL) -> Bool {
        let name = file.lastPathComponent
        return name.hasPrefix(".staging-") || (name.hasPrefix(".thurm-") && name.hasSuffix(".part"))
    }

    /// The process a download's file belongs to, from its name (`.staging-<pid>-…`,
    /// `.thurm-<pid>-<n>.part`); nil for a name without one (an older build's).
    static func downloadOwner(_ file: URL) -> pid_t? {
        let name = file.lastPathComponent
        for prefix in [".staging-", ".thurm-"] where name.hasPrefix(prefix) {
            let rest = name.dropFirst(prefix.count)
            return rest.split(separator: "-", maxSplits: 1).first.flatMap { pid_t($0) }
        }
        return nil
    }

    /// Entries this process uses (a copy shown, a download under way), with how many uses: each
    /// marked on disk with `.use-<pid>`, so another instance sharing the folder leaves it alone.
    private var uses: [String: Int] = [:]

    private func use(_ entry: URL) {
        let key = Self.path(entry)
        if uses[key, default: 0] == 0 {
            FileManager.default.createFile(atPath: entry.appendingPathComponent(".use-\(getpid())").path,
                                           contents: nil)
        }
        uses[key, default: 0] += 1
    }

    private func unuse(_ entry: URL) {
        let key = Self.path(entry)
        guard let n = uses[key] else { return }
        if n > 1 {
            uses[key] = n - 1
        } else {
            uses[key] = nil
            try? FileManager.default.removeItem(at: entry.appendingPathComponent(".use-\(getpid())"))
        }
    }

    /// Whether another running process uses `entry`; its marks of ended ones go on the way.
    private func usedElsewhere(_ entry: URL) -> Bool {
        let fm = FileManager.default
        let me = getpid()
        let marks = ((try? fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: nil)) ?? [])
            .filter { $0.lastPathComponent.hasPrefix(".use-") }
        var other = false
        for mark in marks {
            guard let pid = pid_t(mark.lastPathComponent.dropFirst(".use-".count)) else { continue }
            if pid == me { continue }
            if Self.running(pid, madeAfter: mark) {
                other = true
            } else {
                try? fm.removeItem(at: mark)
            }
        }
        return other
    }

    /// Whether process `pid` still runs (one of another user's counts as running).
    private static func running(_ pid: pid_t) -> Bool {
        kill(pid, 0) == 0 || errno == EPERM
    }

    /// Whether the process that made `file`, `pid`, still runs: a process by that number that
    /// started after the file was made is another one, the number used again.
    static func running(_ pid: pid_t, madeAfter file: URL) -> Bool {
        guard running(pid) else { return false }
        guard let started = startTime(pid),
              let made = (try? file.resourceValues(forKeys: [.creationDateKey]))?.creationDate
        else { return true }
        return started <= made.addingTimeInterval(1)
    }

    /// When process `pid` started, if it can be told.
    static func startTime(_ pid: pid_t) -> Date? {
        var info = kinfo_proc()
        var size = MemoryLayout<kinfo_proc>.stride
        var mib: [Int32] = [CTL_KERN, KERN_PROC, KERN_PROC_PID, pid]
        guard sysctl(&mib, u_int(mib.count), &info, &size, nil, 0) == 0, size > 0 else { return nil }
        let t = info.kp_proc.p_un.__p_starttime
        return Date(timeIntervalSince1970: Double(t.tv_sec) + Double(t.tv_usec) / 1_000_000)
    }

    /// Deletes the downloads in `entry` no fetch is writing: this process's, as it writes into
    /// none of them, and those of a process that has ended (left by an earlier run), its number
    /// taken since by another or not. Another running instance's are its own. Returns the bytes of those left, which still count.
    private func clearStaleDownloads(in entry: URL) -> UInt64 {
        let fm = FileManager.default
        let me = getpid()
        let files = (try? fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: [.fileSizeKey])) ?? []
        return files.filter(Self.isDownload).reduce(UInt64(0)) { left, file in
            let owner = Self.downloadOwner(file)
            let stale = owner.map { $0 == me || !Self.running($0, madeAfter: file) } ?? true
            if stale, (try? fm.removeItem(at: file)) != nil { return left }
            return left + UInt64((try? file.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
        }
    }

    /// Writes a metadata file whole (tests make it fail).
    var writeText: (String, URL) throws -> Void = { try $0.write(to: $1, atomically: true, encoding: .utf8) }

    /// Downloads into the cache (`Core.startDownload`; tests stand in for it).
    var download: Core.DownloadStarter = Core.startDownload

    /// Where the file `path` on `host` (as `cwd` resolves it) is kept: a directory of its own.
    func entry(host: HostId, path: String, cwd: String?) -> URL {
        var key = path
        if !path.hasPrefix("/") && !path.hasPrefix("~") { key = (cwd ?? "") + "/" + path }
        let digest = SHA256.hash(data: Data((host + "\u{0}" + key).utf8))
        let name = digest.prefix(16).map { String(format: "%02x", $0) }.joined()
        return root.appendingPathComponent(name, isDirectory: true).standardizedFileURL
    }

    /// The copy in use in `entry`, as `.current` names it (`c-<id>/<name>`), whether or not it
    /// has a version.
    private func current(_ entry: URL) -> URL? {
        guard let rel = try? String(contentsOf: entry.appendingPathComponent(".current"), encoding: .utf8)
        else { return nil }
        let parts = rel.split(separator: "/", omittingEmptySubsequences: false).map(String.init)
        guard parts.count == 2, parts[0].hasPrefix("c-"), !parts[1].isEmpty, !parts[1].hasPrefix(".")
        else { return nil }
        let file = entry.appendingPathComponent(parts[0], isDirectory: true).appendingPathComponent(parts[1])
        return FileManager.default.fileExists(atPath: file.path) ? file : nil
    }

    /// `name` within the 255 bytes a file name may take: the part before its extension is cut,
    /// by whole characters, and the extension kept when it fits.
    static func fitName(_ name: String, limit: Int = 255) -> String {
        if name.utf8.count <= limit { return name }
        let ext = (name as NSString).pathExtension
        let keep = ext.isEmpty || ext.utf8.count + 2 > limit ? "" : "." + ext
        var stem = Substring(keep.isEmpty ? name : String(name.dropLast(keep.count)))
        while stem.utf8.count + keep.utf8.count > limit { stem = stem.dropLast() }
        return String(stem) + keep
    }

    /// The cached copy in `entry` and the version it is, if any. The version is kept in the
    /// copy's own folder, written before the copy is put in use: whichever copy `.current`
    /// names, its version is its own, even when two app instances install at once.
    func cached(_ entry: URL) -> (file: URL, version: String)? {
        guard let file = current(entry),
              let version = try? String(contentsOf: file.deletingLastPathComponent()
                  .appendingPathComponent(".version"), encoding: .utf8)
        else { return nil }
        return (file, version)
    }

    /// A new file in `entry` (made if needed) for one download to write: the copy in use, if
    /// any, stays as it is until `install` replaces it.
    func stage(_ entry: URL) throws -> URL {
        try FileManager.default.createDirectory(at: entry, withIntermediateDirectories: true,
                                                attributes: [.posixPermissions: 0o700])
        // Named for this process, so another instance sharing the folder leaves it alone.
        return entry.appendingPathComponent(".staging-\(getpid())-\(UUID().uuidString)")
    }

    /// Makes the downloaded `staged` file the copy in use, in a folder of its own under its own
    /// name, and records its version (none: fetched again next time). The copy it replaces is
    /// deleted unless Quick Look shows it; then when it is let go. Returns where the new one is.
    func install(_ entry: URL, staged: URL, name: String, version: String?) throws -> URL {
        let fm = FileManager.default
        // A hidden name (.eslintrc.json) keeps its extension, for Quick Look to know it, but
        // not its leading dot, which marks the cache's own files.
        let safe = Self.fitName(name.isEmpty || name.contains("/") ? "file" : name.hasPrefix(".") ? "dot" + name : name)
        let copy = "c-\(UUID().uuidString)"
        let folder = entry.appendingPathComponent(copy, isDirectory: true)
        try fm.createDirectory(at: folder, withIntermediateDirectories: true, attributes: [.posixPermissions: 0o700])
        let file = folder.appendingPathComponent(safe)
        do {
            try fm.moveItem(at: staged, to: file)
        } catch {
            try? fm.removeItem(at: folder)
            throw error
        }
        // Its version beside it, before it is in use: a version that cannot be recorded only
        // means it is fetched again next time, not that this fetch failed.
        if let version {
            do {
                try writeText(version, folder.appendingPathComponent(".version"))
            } catch {
                tlog("remote preview cache: cannot record the version of \(file.path): \(error)")
            }
        }
        do {
            try writeText("\(copy)/\(safe)", entry.appendingPathComponent(".current"))
        } catch {
            try? fm.removeItem(at: folder)
            throw error
        }
        retireCopies(in: entry)
        // Over the limit even with everything else gone (a copy Quick Look still shows, say):
        // the new copy is shown but not kept, and goes once let go.
        if !prune(keeping: entry) {
            try? fm.removeItem(at: entry.appendingPathComponent(".current"))
        }
        return file
    }

    /// Deletes the copies in `entry` that are neither in use nor shown.
    private func retireCopies(in entry: URL) {
        // Another instance may show any of its copies.
        if usedElsewhere(entry) { return }
        let fm = FileManager.default
        let inUse = current(entry).map { Self.path($0.deletingLastPathComponent()) }
        let shown = Set(held.keys.map { Self.path($0.deletingLastPathComponent()) })
        let copies = (try? fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: nil)) ?? []
        for folder in copies where folder.lastPathComponent.hasPrefix("c-") {
            let path = Self.path(folder)
            if path == inUse || shown.contains(path) { continue }
            try? fm.removeItem(at: folder)
        }
    }

    /// `url` as a path to compare: a folder's URL may end in `/` or not, depending on where it
    /// came from (a listing, or a file's parent), its path never does.
    private static func path(_ url: URL) -> String {
        url.standardizedFileURL.path
    }

    /// Files Quick Look shows, with how many times: neither pruned nor replaced in place.
    private var held: [URL: Int] = [:]

    /// The entry `file` is a copy in, if it is one of the cache's.
    private func entryOf(_ file: URL) -> URL? {
        let entry = file.standardizedFileURL.deletingLastPathComponent().deletingLastPathComponent()
        return Self.path(entry.deletingLastPathComponent()) == Self.path(root) ? entry : nil
    }

    /// Keeps `file` while it is shown (no-op for other files).
    func hold(_ file: URL) {
        guard let entry = entryOf(file) else { return }
        held[file.standardizedFileURL, default: 0] += 1
        use(entry)
    }

    /// Quick Look moves from showing `old` to showing `new`: `new` is held before `old` is let
    /// go, so pruning on the way never takes the file about to be shown.
    func switchHeld(from old: URL?, to new: URL?) {
        if let new { hold(new) }
        if let old { release(old) }
    }

    /// Lets `file` go again: a copy no longer in use goes, and what no longer fits is pruned.
    func release(_ file: URL) {
        let file = file.standardizedFileURL
        guard let entry = entryOf(file), let n = held[file] else { return }
        held[file] = n > 1 ? n - 1 : nil
        unuse(entry)
        retireCopies(in: entry)
        // Only what was left over the limit (a shown copy in the way) needs a look now.
        if overLimit { prune() }
    }

    /// The last prune left more than `maxBytes`.
    private var overLimit = false

    /// How many times the cache was walked to prune (tests).
    private(set) var prunes = 0

    /// Bytes reserved by downloads under way, by fetch, with the entry each writes to.
    private var reserved: [UUID: (entry: URL, bytes: UInt64)] = [:]

    /// The bytes of the copies in `entry`.
    private func size(of entry: URL) -> UInt64 {
        let fm = FileManager.default
        let copies = (try? fm.contentsOfDirectory(at: entry, includingPropertiesForKeys: nil)) ?? []
        return copies.filter { $0.lastPathComponent.hasPrefix("c-") }.reduce(UInt64(0)) { sum, folder in
            let files = (try? fm.contentsOfDirectory(at: folder, includingPropertiesForKeys: [.fileSizeKey])) ?? []
            // The copy, not its version beside it (a few bytes).
            return files.filter { !$0.lastPathComponent.hasPrefix(".") }.reduce(sum) {
                $0 + UInt64((try? $1.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
            }
        }
    }

    /// Forgets the least recently fetched entries until the rest, with the downloads under way,
    /// fit in `maxBytes`. Entries shown, being downloaded to, or `keep` stay, and an entry that
    /// could not be deleted still counts. Whether it all fits.
    @discardableResult
    func prune(keeping keep: URL? = nil) -> Bool {
        prunes += 1
        let fm = FileManager.default
        let busy = Set((held.keys.compactMap(entryOf) + reserved.values.map(\.entry) + [keep].compactMap { $0 })
            .map(Self.path))
        let entries = (try? fm.contentsOfDirectory(at: root, includingPropertiesForKeys: [.contentModificationDateKey]))
            ?? []
        // Copies neither in use nor shown (one not kept, let go) go first, and downloads no
        // fetch is writing; one that cannot be deleted counts.
        let downloading = Set(reserved.values.map { Self.path($0.entry) })
        var stale: [String: UInt64] = [:]
        for entry in entries {
            retireCopies(in: entry)
            let path = Self.path(entry)
            if !downloading.contains(path) { stale[path] = clearStaleDownloads(in: entry) }
        }
        var sized: [(url: URL, date: Date, size: UInt64)] = entries.map { entry in
            let date = (try? entry.resourceValues(forKeys: [.contentModificationDateKey]).contentModificationDate)
                ?? .distantPast
            return (entry.standardizedFileURL, date, size(of: entry) + (stale[Self.path(entry)] ?? 0))
        }
        sized.sort { $0.date < $1.date }
        // A download refreshing an entry replaces its copy in use, unless Quick Look shows it,
        // here or in another instance: only what it adds beyond that copy is reserved.
        let extra = reserved.values.reduce(UInt64(0)) { sum, r in
            var replaced: UInt64 = 0
            if let file = current(r.entry), held[file.standardizedFileURL] == nil, !usedElsewhere(r.entry) {
                replaced = UInt64((try? file.resourceValues(forKeys: [.fileSizeKey]).fileSize) ?? 0)
            }
            return sum + (r.bytes > replaced ? r.bytes - replaced : 0)
        }
        var total = sized.reduce(UInt64(0)) { $0 + $1.size } + extra
        for e in sized where total > maxBytes && !busy.contains(Self.path(e.url)) && !usedElsewhere(e.url) {
            if (try? fm.removeItem(at: e.url)) != nil { total -= e.size }
        }
        overLimit = total > maxBytes
        return total <= maxBytes
    }

    /// A fetch under way: those waiting on it, and what stops its download.
    private final class Fetch {
        var waiters: [UUID: (progress: (TransferProgress) -> Void, done: (Result<URL, Error>) -> Void)] = [:]
        var cancel: (() -> Void)?
        /// No one waits for it any more: whatever its download brings, even a whole file, is
        /// thrown away, never put in the cache over a newer fetch's copy.
        var abandoned = false
    }

    /// Fetches under way, by the entry of their first path.
    private var inFlight: [URL: Fetch] = [:]

    /// Fetches `reference`, as `pane` sees it, into the cache (or finds it there unchanged);
    /// `progress` and `completion` run on the main thread. A fetch of a file already on its way
    /// waits for that one instead of starting another. Returns what cancels this request: it
    /// completes with `RemoteFileError.cancelled`, and the download stops once no one waits.
    @discardableResult
    func fetch(_ reference: TerminalFileReference, pane: PaneKey, cwd: String?,
               progress: @escaping (TransferProgress) -> Void = { _ in },
               completion: @escaping (Result<URL, Error>) -> Void) -> () -> Void {
        let paths: [String]
        do {
            // A relative path is the directory's the pane had when it was clicked, not the one
            // it is in by the time the host is asked.
            paths = try reference.remotePaths().map { path in
                if path.hasPrefix("/") || path.hasPrefix("~") { return path }
                guard let cwd, cwd.hasPrefix("/"), !cwd.hasPrefix("//"), !cwd.contains("\0") else {
                    throw FilePreviewError.noDirectory
                }
                return cwd.hasSuffix("/") ? cwd + path : cwd + "/" + path
            }
        } catch {
            completion(.failure(error))
            return {}
        }
        let key = entry(host: pane.host, path: paths[0], cwd: cwd)
        let id = UUID()
        let cancel: () -> Void = { [weak self] in
            guard let fetch = self?.inFlight[key], let waiter = fetch.waiters.removeValue(forKey: id) else {
                return
            }
            waiter.done(.failure(RemoteFileError.cancelled))
            // No one waits: it is given up, and a new request for the file starts afresh rather
            // than joining a download on its way out.
            if fetch.waiters.isEmpty {
                fetch.abandoned = true
                if self?.inFlight[key] === fetch { self?.inFlight[key] = nil }
                fetch.cancel?()
            }
        }
        if let running = inFlight[key] {
            running.waiters[id] = (progress, completion)
            return cancel
        }
        let fetch = Fetch()
        fetch.waiters[id] = (progress, completion)
        inFlight[key] = fetch
        self.fetch(paths[...], pane: pane, cwd: cwd, firstError: nil,
                   progress: { p in for w in fetch.waiters.values { w.progress(p) } },
                   started: { fetch.cancel = $0 },
                   abandoned: { fetch.abandoned }) { [self] result in
            if inFlight[key] === fetch { inFlight[key] = nil }
            for w in fetch.waiters.values { w.done(result) }
            fetch.waiters.removeAll()
        }
        return cancel
    }

    private func fetch(_ paths: ArraySlice<String>, pane: PaneKey, cwd: String?, firstError: String?,
                       progress: @escaping (TransferProgress) -> Void,
                       started: @escaping (@escaping () -> Void) -> Void,
                       abandoned: @escaping () -> Bool,
                       completion: @escaping (Result<URL, Error>) -> Void) {
        guard let path = paths.first else {
            return completion(.failure(RemoteFileError.failed(firstError ?? "The file was not found.")))
        }
        let entry = self.entry(host: pane.host, path: path, cwd: cwd)
        let cached = cached(entry)
        let staged: URL
        do { staged = try stage(entry) } catch { return completion(.failure(error)) }
        let token = UUID()
        reserved[token] = (entry, 0)
        use(entry)
        var cancel: (() -> Void)?
        var tooMuch = false
        let onProgress: (TransferProgress) -> Void = { [self] p in
            // The size is known from the first report: it is reserved, and must fit beside the
            // rest, or the download stops.
            progress(p)
            guard let r = reserved[token], r.bytes == 0, p.total > 0 else { return }
            reserved[token] = (entry, p.total)
            if !prune() {
                tooMuch = true
                cancel?()
            }
        }
        cancel = download(pane, path, staged, cached?.version, maxBytes, onProgress) { [self] outcome in
            reserved[token] = nil
            unuse(entry)
            // Given up: its file, however it ended, is not this cache's (a newer fetch's is).
            if abandoned() {
                try? FileManager.default.removeItem(at: staged)
                return completion(.failure(RemoteFileError.cancelled))
            }
            if outcome.notModified, let cached {
                try? FileManager.default.removeItem(at: staged)
                try? FileManager.default.setAttributes([.modificationDate: Date()], ofItemAtPath: entry.path)
                return completion(.success(cached.file))
            }
            if tooMuch {
                try? FileManager.default.removeItem(at: staged)
                return completion(.failure(RemoteFileError.failed(
                    "There is no room to preview this file while other previews download "
                        + "(\(maxBytes >> 20) MiB in all).")))
            }
            if let error = outcome.error {
                try? FileManager.default.removeItem(at: staged)
                if outcome.unsupported { return completion(.failure(FilePreviewError.remote)) }
                // Only a path with nothing at it falls back to the one without its `:line`
                // suffix: any other failure is this file's, and another file is not shown for it.
                guard outcome.notFound else { return completion(.failure(RemoteFileError.failed(error))) }
                return fetch(paths.dropFirst(), pane: pane, cwd: cwd, firstError: firstError ?? error,
                             progress: progress, started: started, abandoned: abandoned,
                             completion: completion)
            }
            do {
                completion(.success(try install(entry, staged: staged, name: outcome.name ?? "file",
                                                version: outcome.version)))
            } catch {
                try? FileManager.default.removeItem(at: staged)
                completion(.failure(error))
            }
        }
        started { cancel?() }
    }
}

enum RemoteFileError: Error, LocalizedError, Equatable {
    case failed(String)
    /// Its request was cancelled.
    case cancelled

    var errorDescription: String? {
        switch self {
        case .failed(let why): return why
        case .cancelled: return "Cancelled."
        }
    }
}
