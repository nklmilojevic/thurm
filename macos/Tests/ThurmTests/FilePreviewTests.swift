import AppKit
import QuickLookUI
import XCTest
@testable import Thurm

final class FilePreviewTests: XCTestCase {
    private func reference(_ text: String, at needle: String, link: URL? = nil) -> TerminalFileReference? {
        let range = (text as NSString).range(of: needle)
        return TerminalFileReference.at(text: text, columns: Array(0..<text.utf16.count),
                                        column: range.location, link: link)
    }

    func testPathsAndDiagnosticLocations() {
        for path in ["/tmp/image.png", "./report.pdf", "../assets/icon.png", "~/photo.jpg",
                     "report.pdf", "src/main.swift:12:3", "./LICENSE", ".env", "/tmp/file(1)"] {
            XCTAssertEqual(reference("See \(path)", at: path), .path(path))
        }
        XCTAssertEqual(reference("See (./report.pdf), next", at: "report"), .path("./report.pdf"))
        XCTAssertNil(reference("ordinary output", at: "output"))
        XCTAssertNil(reference("image.png   ", at: "   "))
        XCTAssertEqual(reference("src/main.swift:12:3:", at: "main"), .path("src/main.swift:12:3"))
    }

    func testQuotedAndEscapedSpaces() {
        XCTAssertEqual(reference(#"See "./My Report.pdf" now"#, at: "Report"), .path("./My Report.pdf"))
        XCTAssertEqual(reference("'./My Report.pdf'", at: "Report"), .path("./My Report.pdf"))
        XCTAssertEqual(reference("`./My Report.pdf`", at: "Report"), .path("./My Report.pdf"))
        XCTAssertEqual(reference(#"./My\ Report.pdf"#, at: "Report"), .path("./My Report.pdf"))
        XCTAssertEqual(reference("'./file(1).pdf'", at: "file"), .path("./file(1).pdf"))
    }

    func testCellMapAndHyperlinkPriority() {
        let text = "😀 report.pdf"
        let columns = [0, 0, 2] + Array(3..<13)
        XCTAssertEqual(TerminalFileReference.at(text: text, columns: columns, column: 8, link: nil),
                       .path("report.pdf"))
        XCTAssertNil(TerminalFileReference.at(text: text, columns: [], column: 8, link: nil))
        let web = URL(string: "https://example.com/image.png")!
        XCTAssertNil(reference("image.png", at: "image", link: web))
        let file = URL(string: "file:///tmp/My%20Report.pdf")!
        XCTAssertEqual(reference("click here", at: "here", link: file), .fileURL(file))
    }

    func testResolutionUsesPaneDirectoryAndHome() throws {
        XCTAssertEqual(try TerminalFileReference.path("../image.png").resolve(cwd: "/work/sub", isRemote: false).path,
                       "/work/image.png")
        XCTAssertEqual(try TerminalFileReference.path("image.png").resolve(cwd: "/other", isRemote: false).path,
                       "/other/image.png")
        XCTAssertEqual(try TerminalFileReference.path("~/image.png").resolve(cwd: nil, isRemote: false,
                                                                             home: "/Users/test").path,
                       "/Users/test/image.png")
        XCTAssertEqual(try TerminalFileReference.path("/tmp/image.png").resolve(cwd: nil, isRemote: false).path,
                       "/tmp/image.png")
        for cwd in [nil, "", "relative", "//server/share"] as [String?] {
            XCTAssertThrowsError(try TerminalFileReference.path("image.png").resolve(cwd: cwd, isRemote: false)) {
                XCTAssertEqual($0 as? FilePreviewError, .noDirectory)
            }
        }
    }

    func testLocalFileURLsDecodeOnce() throws {
        for host in ["", "localhost", "test.local"] {
            let ref = TerminalFileReference.fileURL(URL(string: "file://\(host)/tmp/My%20Report%2520.pdf")!)
            XCTAssertEqual(try ref.resolve(cwd: nil, isRemote: false, localHostName: "test.local").path,
                           "/tmp/My Report%20.pdf")
        }
        XCTAssertEqual(try TerminalFileReference.path("/tmp/a%20b.pdf").resolve(cwd: nil, isRemote: false).path,
                       "/tmp/a%20b.pdf")
    }

    func testRemoteReferencesNeverResolveOnThisMac() {
        for ref in [TerminalFileReference.path("/etc/hosts"), .path("~/image.png"),
                    .fileURL(URL(string: "file:///etc/hosts")!)] {
            XCTAssertThrowsError(try ref.resolve(cwd: "/tmp", isRemote: true)) {
                XCTAssertEqual($0 as? FilePreviewError, .remote)
            }
        }
        XCTAssertThrowsError(try TerminalFileReference.fileURL(URL(string: "file://other-host/tmp/a.pdf")!)
            .resolve(cwd: nil, isRemote: false)) { XCTAssertEqual($0 as? FilePreviewError, .remote) }
        for path in ["sftp://host/tmp/a.pdf", "host:/tmp/a.pdf", "user@host:report.pdf"] {
            XCTAssertThrowsError(try TerminalFileReference.path(path).resolve(cwd: "/tmp", isRemote: false)) {
                XCTAssertEqual($0 as? FilePreviewError, .remote)
            }
        }
    }

    func testInvalidReferences() {
        for path in ["~other/image.png", "/tmp/a\0.pdf", "//host/share/image.png"] {
            XCTAssertThrowsError(try TerminalFileReference.path(path).resolve(cwd: "/tmp", isRemote: false))
        }
        for url in ["file:relative.pdf", "file:///tmp/a.pdf?query", "file:///tmp/a.pdf#fragment",
                    "file://user@localhost/tmp/a.pdf"] {
            XCTAssertThrowsError(try TerminalFileReference.fileURL(URL(string: url)!).resolve(cwd: nil, isRemote: false))
        }
    }

    func testHostRelativeSCPPathsRejectLocalNameCollisions() throws {
        let fm = FileManager.default
        let directory = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: directory) }
        for name in ["server:report.pdf", "server:report.pdf:12:3", "server.example:report.pdf"] {
            let file = directory.appendingPathComponent(name)
            try Data("local file".utf8).write(to: file)
            let ref = try XCTUnwrap(reference(name, at: name))
            XCTAssertThrowsError(try ref.localFile(cwd: directory.path, isRemote: false)) {
                XCTAssertEqual($0 as? FilePreviewError, .remote)
            }
            for explicitPath in ["./" + name, file.path] {
                XCTAssertEqual(try TerminalFileReference.path(explicitPath)
                    .localFile(cwd: directory.path, isRemote: false).path, file.path)
            }
        }
        for path in ["server:reports/report.pdf", "server:~/report.pdf", "server:report.pdf:12"] {
            XCTAssertThrowsError(try TerminalFileReference.path(path).resolve(cwd: nil, isRemote: false)) {
                XCTAssertEqual($0 as? FilePreviewError, .remote)
            }
        }
    }

    func testFileURLDiagnosticFallbackKeepsLiteralNamesAndValidatesHost() throws {
        let fm = FileManager.default
        let directory = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: directory) }
        let file = directory.appendingPathComponent("My Report%20.pdf")
        try Data("base file".utf8).write(to: file)
        for suffix in [":42", ":42:3"] {
            let literal = directory.appendingPathComponent(file.lastPathComponent + suffix)
            for host in ["", "localhost", ProcessInfo.processInfo.hostName] {
                var parts = try XCTUnwrap(URLComponents(url: literal, resolvingAgainstBaseURL: false))
                parts.host = host
                let url = try XCTUnwrap(parts.url)
                let ref = try XCTUnwrap(reference("report", at: "report", link: url))
                XCTAssertEqual(try ref.localFile(cwd: nil, isRemote: false).path, file.path)
                try Data("literal file".utf8).write(to: literal)
                XCTAssertEqual(try ref.localFile(cwd: nil, isRemote: false).path, literal.path)
                parts.host = "other-host.invalid"
                let remote = TerminalFileReference.fileURL(try XCTUnwrap(parts.url))
                XCTAssertThrowsError(try remote.localFile(cwd: directory.path, isRemote: false)) {
                    XCTAssertEqual($0 as? FilePreviewError, .remote)
                }
                try fm.removeItem(at: literal)
                XCTAssertThrowsError(try remote.localFile(cwd: directory.path, isRemote: false)) {
                    XCTAssertEqual($0 as? FilePreviewError, .remote)
                }
                parts.host = "localhost"
                parts.query = "query"
                XCTAssertThrowsError(try TerminalFileReference.fileURL(XCTUnwrap(parts.url))
                    .localFile(cwd: nil, isRemote: false)) {
                    XCTAssertEqual($0 as? FilePreviewError, .invalid)
                }
            }
        }
    }

    func testFileValidationAndLiteralSuffix() throws {
        let fm = FileManager.default
        let directory = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: directory) }
        let file = directory.appendingPathComponent("report.pdf")
        try Data("test".utf8).write(to: file)
        XCTAssertEqual(try TerminalFileReference.path("report.pdf:12:3")
            .localFile(cwd: directory.path, isRemote: false).path, file.path)
        let literal = directory.appendingPathComponent("report.pdf:12:3")
        try Data("literal".utf8).write(to: literal)
        XCTAssertEqual(try TerminalFileReference.path("report.pdf:12:3")
            .localFile(cwd: directory.path, isRemote: false).path, literal.path)
        let symlink = directory.appendingPathComponent("link.pdf")
        try fm.createSymbolicLink(at: symlink, withDestinationURL: file)
        XCTAssertNoThrow(try TerminalFileReference.path(symlink.path).localFile(cwd: nil, isRemote: false))
        XCTAssertThrowsError(try TerminalFileReference.path(directory.path).localFile(cwd: nil, isRemote: false)) {
            XCTAssertEqual($0 as? FilePreviewError, .unsupported)
        }
        try fm.removeItem(at: file)
        XCTAssertThrowsError(try TerminalFileReference.path(symlink.path).localFile(cwd: nil, isRemote: false)) {
            XCTAssertEqual($0 as? FilePreviewError, .missing)
        }
        XCTAssertThrowsError(try TerminalFileReference.path(file.path).localFile(cwd: nil, isRemote: false)) {
            XCTAssertEqual($0 as? FilePreviewError, .missing)
        }
    }

    /// A window whose pointer is wherever the test puts it.
    private final class PointerWindow: NSWindow {
        var pointer = NSPoint.zero
        override var mouseLocationOutsideOfEventStream: NSPoint { pointer }
    }

    func testPointerIsFoundInTheView() {
        _ = NSApplication.shared
        let window = PointerWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 300),
                                   styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        let content = NSView(frame: NSRect(x: 0, y: 0, width: 400, height: 300))
        window.contentView = content
        let view = TerminalView(pane: .local(987657))
        view.frame = NSRect(x: 50, y: 40, width: 200, height: 100)
        content.addSubview(view)
        // 10 points right of and above the view's bottom left corner; the view counts from its top.
        window.pointer = NSPoint(x: 60, y: 50)
        XCTAssertEqual(view.pointerLocation(), NSPoint(x: 10, y: 90))
        window.pointer = NSPoint(x: 300, y: 250)
        XCTAssertNil(view.pointerLocation())
    }

    func testPointerIsReadNowNotFromTheLastHover() throws {
        _ = NSApplication.shared
        let view = TerminalView(pane: .local(987656))
        XCTAssertNil(view.pointerLocation())
        // Far off every screen, so the real pointer cannot be over it.
        let window = NSWindow(contentRect: NSRect(x: -40000, y: -40000, width: 400, height: 300),
                              styleMask: [.borderless], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        defer { window.close() }
        window.contentView = view
        let hover = try XCTUnwrap(NSEvent.mouseEvent(with: .mouseMoved, location: NSPoint(x: 200, y: 150),
                                                     modifierFlags: [], timestamp: 0,
                                                     windowNumber: window.windowNumber, context: nil,
                                                     eventNumber: 0, clickCount: 0, pressure: 0))
        view.mouseMoved(with: hover)
        XCTAssertNil(view.pointerLocation())
        XCTAssertNil(view.pointedFile())
    }

    func testMenuKeepsClickedPaneContext() throws {
        _ = NSApplication.shared
        let pane = PaneKey.local(987654)
        let manager = SessionManager.shared
        let previous = manager.panes[pane]
        defer { manager.panes[pane] = previous }
        manager.panes[pane] = PaneInfo(json: ["id": pane.id, "cwd": "/clicked"])
        let view = TerminalView(pane: pane)
        let menu = view.contextMenu(link: nil, file: .path("report.pdf"))
        let item = try XCTUnwrap(menu.items.first { $0.title == "Quick Look" })
        let request = try XCTUnwrap(item.representedObject as? FilePreviewRequest)
        manager.panes[pane]?.cwd = "/changed"
        XCTAssertEqual(request.cwd, "/clicked")
        XCTAssertFalse(request.isRemote)
        XCTAssertEqual(request.reference, .path("report.pdf"))
        XCTAssertTrue(item.target === view)
        XCTAssertNil(view.contextMenu(link: URL(string: "https://example.com")).items.first { $0.title == "Quick Look" })
        let remote = TerminalView(pane: PaneKey("remote", 987654))
        let remoteMenu = remote.contextMenu(link: nil, file: .path("report.pdf"))
        XCTAssertTrue((remoteMenu.items.first?.representedObject as? FilePreviewRequest)?.isRemote == true)
    }

    func testPreviewPanelOpensChangesFileAndClosesWithView() throws {
        _ = NSApplication.shared
        let fm = FileManager.default
        let directory = fm.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        try fm.createDirectory(at: directory, withIntermediateDirectories: true)
        defer { try? fm.removeItem(at: directory) }
        let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: 400, height: 300),
                              styleMask: [.titled, .closable], backing: .buffered, defer: false)
        window.isReleasedWhenClosed = false
        let view = TerminalView(pane: .local(987655))
        window.contentView = view
        defer {
            if QLPreviewPanel.sharedPreviewPanelExists() { QLPreviewPanel.shared()?.orderOut(nil) }
            window.close()
        }
        window.makeKeyAndOrderFront(nil)
        window.makeMain()
        guard NSApp.mainWindow === window else {
            throw XCTSkip("This test requires an AppKit session that can make a window main.")
        }
        for name in ["first.txt", "second.txt"] {
            let file = directory.appendingPathComponent(name)
            try Data(name.utf8).write(to: file)
            let menu = view.contextMenu(link: nil, file: .path(file.path))
            let item = try XCTUnwrap(menu.items.first { $0.title == "Quick Look" })
            _ = view.perform(try XCTUnwrap(item.action), with: item)
            let panel = try XCTUnwrap(QLPreviewPanel.shared())
            XCTAssertTrue(panel.currentController as AnyObject? === view)
            XCTAssertTrue(panel.isVisible)
            XCTAssertEqual(panel.currentPreviewItem?.previewItemURL, file)
            XCTAssertEqual(view.numberOfPreviewItems(in: panel), 1)
            XCTAssertNil(view.previewPanel(panel, previewItemAt: 1))
        }
        view.removeFromSuperview()
        XCTAssertFalse(QLPreviewPanel.shared().isVisible)
    }
    func testRemoteReferencesAreAskedOfTheirHost() throws {
        XCTAssertEqual(try TerminalFileReference.path("src/main.swift:12:3").remotePaths(),
                       ["src/main.swift:12:3", "src/main.swift"])
        XCTAssertEqual(try TerminalFileReference.path("~/notes.md").remotePaths(), ["~/notes.md"])
        XCTAssertEqual(try TerminalFileReference.path("/etc/hosts").remotePaths(), ["/etc/hosts"])
        // A file URL printed on the host names the host's file, whatever host it carries.
        XCTAssertEqual(try TerminalFileReference.fileURL(URL(string: "file://devbox/tmp/My%20Report.pdf")!)
            .remotePaths(), ["/tmp/My Report.pdf"])
        // An absolute path may hold `://` in a name; a URL is still no path.
        XCTAssertEqual(try TerminalFileReference.path("/tmp/a://b").remotePaths(), ["/tmp/a://b"])
        for bad in ["~root/x", "sftp://host/tmp/a.pdf", "host:/tmp/a.pdf", "user@host:report.pdf"] {
            XCTAssertThrowsError(try TerminalFileReference.path(bad).remotePaths(), bad) {
                XCTAssertEqual($0 as? FilePreviewError, .invalid)
            }
        }
        XCTAssertThrowsError(try TerminalFileReference.fileURL(URL(string: "file:///a.pdf?x=1")!).remotePaths())
    }

    func testRemoteFilesAreCachedByHostPathAndVersion() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-remote-cache-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let cache = RemoteFileCache(root: root, maxBytes: 10)
        let entry = cache.entry(host: "devbox", path: "a.txt", cwd: "/work")
        XCTAssertEqual(entry, cache.entry(host: "devbox", path: "/work/a.txt", cwd: "/elsewhere"))
        XCTAssertNotEqual(entry, cache.entry(host: "other", path: "/work/a.txt", cwd: nil))
        XCTAssertNotEqual(entry, cache.entry(host: "devbox", path: "a.txt", cwd: "/other"))
        XCTAssertNil(cache.cached(entry))

        let stagedEntry = try cache.stage(entry)
        try Data("1234".utf8).write(to: stagedEntry)
        let file = try cache.install(entry, staged: stagedEntry, name: "a.txt", version: "v1")
        XCTAssertEqual(file.lastPathComponent, "a.txt")
        XCTAssertEqual(cache.cached(entry)?.file, file)
        XCTAssertEqual(cache.cached(entry)?.version, "v1")
        // A file changing as it was read has no version: it is fetched again next time.
        let fresh = cache.entry(host: "devbox", path: "/b.log", cwd: nil)
        let stagedFresh = try cache.stage(fresh)
        try Data("12".utf8).write(to: stagedFresh)
        _ = try cache.install(fresh, staged: stagedFresh, name: "b.log", version: nil)
        XCTAssertNil(cache.cached(fresh))
        // A name from the host never leaves the entry.
        let odd = cache.entry(host: "devbox", path: "/c", cwd: nil)
        let stagedOdd = try cache.stage(odd)
        try Data("1".utf8).write(to: stagedOdd)
        XCTAssertEqual(try cache.install(odd, staged: stagedOdd, name: "../../x", version: "v").lastPathComponent, "file")
        // A hidden name keeps its extension, for Quick Look to know what it is.
        let dotted = cache.entry(host: "devbox", path: "/.eslintrc.json", cwd: nil)
        let stagedDotted = try cache.stage(dotted)
        try Data("{}".utf8).write(to: stagedDotted)
        let kept = try cache.install(dotted, staged: stagedDotted, name: ".eslintrc.json", version: "v")
        XCTAssertEqual(kept.lastPathComponent, "dot.eslintrc.json")
        XCTAssertEqual(cache.cached(dotted)?.file, kept)
        // A hidden name as long as a name may be: cut to fit with its prefix, extension kept.
        let roomy = tempCache()
        let longName = "." + String(repeating: "é", count: 124) + ".json"
        XCTAssertEqual(longName.utf8.count, 254)
        let long = roomy.entry(host: "devbox", path: "/" + longName, cwd: nil)
        let stagedLong = try roomy.stage(long)
        try Data("{}".utf8).write(to: stagedLong)
        let longKept = try roomy.install(long, staged: stagedLong, name: longName, version: "v")
        let shown = longKept.lastPathComponent.precomposedStringWithCanonicalMapping
        XCTAssertLessThanOrEqual(shown.utf8.count, 255)
        XCTAssertTrue(shown.hasPrefix("dot."))
        XCTAssertTrue(shown.hasSuffix(".json"))
        XCTAssertEqual(roomy.cached(long)?.file, longKept)

        // Over the limit, the least recently fetched entries go; the one just stored stays.
        try FileManager.default.setAttributes([.modificationDate: Date.distantPast], ofItemAtPath: entry.path)
        let big = cache.entry(host: "devbox", path: "/big", cwd: nil)
        let stagedBig = try cache.stage(big)
        try Data(repeating: 7, count: 8).write(to: stagedBig)
        _ = try cache.install(big, staged: stagedBig, name: "big", version: "v")
        XCTAssertFalse(FileManager.default.fileExists(atPath: entry.path))
        XCTAssertNotNil(cache.cached(big))
    }

    private func tempCache(maxBytes: UInt64 = 1 << 20) -> RemoteFileCache {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-remote-cache-\(UUID().uuidString)", isDirectory: true)
        addTeardownBlock { try? FileManager.default.removeItem(at: root) }
        return RemoteFileCache(root: root, maxBytes: maxBytes)
    }

    private func outcome(name: String, version: String?) -> TransferOutcome {
        var o = TransferOutcome()
        o.name = name
        o.version = version
        return o
    }

    func testPreviewsOfAFileOnItsWayShareOneDownload() throws {
        let cache = tempCache()
        var calls: [(dest: URL, done: (TransferOutcome) -> Void)] = []
        cache.download = downloads { _, _, dest, _, done in calls.append((dest, done)) }
        let pane = PaneKey("devbox", 3)
        var got: [URL] = []
        for _ in 0..<2 {
            cache.fetch(.path("notes.md"), pane: pane, cwd: "/work") { got.append(try! $0.get()) }
        }
        XCTAssertEqual(calls.count, 1)
        try Data("notes".utf8).write(to: calls[0].dest)
        calls[0].done(outcome(name: "notes.md", version: "v1"))
        XCTAssertEqual(got.count, 2)
        XCTAssertEqual(got[0], got[1])
        XCTAssertEqual(try String(contentsOf: got[0], encoding: .utf8), "notes")
        // Once it is in, the next preview asks again, with the version it has.
        var asked: String?
        cache.download = downloads { _, _, _, version, done in
            asked = version
            var same = TransferOutcome()
            same.notModified = true
            done(same)
        }
        var again: URL?
        cache.fetch(.path("notes.md"), pane: pane, cwd: "/work") { again = try? $0.get() }
        XCTAssertEqual(asked, "v1")
        XCTAssertEqual(again, got[0])
    }

    func testAFailedRefreshKeepsTheCachedCopy() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        cache.download = downloads { _, _, dest, _, done in
            try! Data("old".utf8).write(to: dest)
            done(self.outcome(name: "a.txt", version: "v1"))
        }
        var first: URL?
        cache.fetch(.path("/a.txt"), pane: pane, cwd: nil) { first = try? $0.get() }
        let file = try XCTUnwrap(first)
        // The host's file changed, and the new copy breaks off half way.
        cache.download = downloads { _, _, dest, _, done in
            try! Data("ne".utf8).write(to: dest)
            done(TransferOutcome(error: "connection lost"))
        }
        var failed: Result<URL, Error>?
        cache.fetch(.path("/a.txt"), pane: pane, cwd: nil) { failed = $0 }
        XCTAssertThrowsError(try XCTUnwrap(failed).get())
        XCTAssertEqual(try String(contentsOf: file, encoding: .utf8), "old")
        let entry = cache.entry(host: "devbox", path: "/a.txt", cwd: nil)
        XCTAssertEqual(cache.cached(entry)?.version, "v1")
        XCTAssertEqual(try FileManager.default.contentsOfDirectory(atPath: entry.path)
            .filter { $0.hasPrefix(".staging") }, [])
        // A whole new copy replaces it, under the host's new name.
        cache.download = downloads { _, _, dest, _, done in
            try! Data("new".utf8).write(to: dest)
            done(self.outcome(name: "a.txt", version: "v2"))
        }
        var second: URL?
        cache.fetch(.path("/a.txt"), pane: pane, cwd: nil) { second = try? $0.get() }
        let replaced = try XCTUnwrap(second)
        XCTAssertEqual(replaced.lastPathComponent, "a.txt")
        XCTAssertEqual(try String(contentsOf: replaced, encoding: .utf8), "new")
        XCTAssertEqual(cache.cached(entry)?.file, replaced)
        XCTAssertEqual(cache.cached(entry)?.version, "v2")
        // Nothing showed the old copy: it is gone.
        XCTAssertFalse(FileManager.default.fileExists(atPath: file.path))
    }

    func testOnlyANotFoundPathFallsBackToItsStrippedSuffix() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        var asked: [String] = []
        // The literal `report:12` fails for another reason: `report` is not shown instead.
        cache.download = downloads { _, path, _, _, done in
            asked.append(path)
            done(TransferOutcome(error: "cannot open report:12: Permission denied"))
        }
        var result: Result<URL, Error>?
        cache.fetch(.path("report:12"), pane: pane, cwd: "/work") { result = $0 }
        XCTAssertEqual(asked, ["/work/report:12"])
        XCTAssertThrowsError(try XCTUnwrap(result).get()) {
            XCTAssertEqual($0.localizedDescription, "cannot open report:12: Permission denied")
        }
        // Nothing at `report:12`: then `report`.
        asked = []
        cache.download = downloads { _, path, dest, _, done in
            asked.append(path)
            if path == "/work/report:12" {
                var missing = TransferOutcome(error: "report:12: no such file")
                missing.notFound = true
                return done(missing)
            }
            try! Data("r".utf8).write(to: dest)
            done(self.outcome(name: "report", version: "v"))
        }
        result = nil
        cache.fetch(.path("report:12"), pane: pane, cwd: "/work") { result = $0 }
        XCTAssertEqual(asked, ["/work/report:12", "/work/report"])
        XCTAssertEqual(try XCTUnwrap(result).get().lastPathComponent, "report")
    }

    func testAFileShownInQuickLookIsNotPruned() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        func fetch(_ path: String, bytes: Int) -> URL? {
            cache.download = downloads { _, _, dest, _, done in
                try! Data(repeating: 1, count: bytes).write(to: dest)
                done(self.outcome(name: (path as NSString).lastPathComponent, version: "v"))
            }
            var got: URL?
            cache.fetch(.path(path), pane: pane, cwd: nil) { got = try? $0.get() }
            return got
        }
        let shown = try XCTUnwrap(fetch("/first.bin", bytes: 8))
        cache.hold(shown)
        // A second preview over the limit leaves the one on screen (and is itself shown).
        let second = try XCTUnwrap(fetch("/second.bin", bytes: 8))
        cache.hold(second)
        XCTAssertTrue(FileManager.default.fileExists(atPath: shown.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: second.path))
        // Let go, it is pruned.
        cache.release(shown)
        XCTAssertFalse(FileManager.default.fileExists(atPath: shown.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: second.path))
        // Files outside the cache are not its to hold.
        cache.hold(URL(fileURLWithPath: "/tmp/elsewhere.txt"))
        cache.release(URL(fileURLWithPath: "/tmp/elsewhere.txt"))
    }

    /// A stand-in download that cannot be cancelled and reports no progress.
    private func downloads(
        _ body: @escaping (PaneKey, String, URL, String?, @escaping (TransferOutcome) -> Void) -> Void
    ) -> Core.DownloadStarter {
        { pane, path, dest, version, _, _, done in
            body(pane, path, dest, version, done)
            return nil
        }
    }

    func testARefreshLeavesTheCopyQuickLookShows() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        func fetch(_ content: String, version: String) -> URL? {
            cache.download = downloads { _, _, dest, _, done in
                try! Data(content.utf8).write(to: dest)
                done(self.outcome(name: "log.txt", version: version))
            }
            var got: URL?
            cache.fetch(.path("/log.txt"), pane: pane, cwd: nil) { got = try? $0.get() }
            return got
        }
        let shown = try XCTUnwrap(fetch("old", version: "v1"))
        cache.hold(shown)
        // The host's file changed while Quick Look shows the old copy.
        let fresh = try XCTUnwrap(fetch("new", version: "v2"))
        XCTAssertNotEqual(fresh, shown)
        XCTAssertEqual(try String(contentsOf: shown, encoding: .utf8), "old")
        XCTAssertEqual(try String(contentsOf: fresh, encoding: .utf8), "new")
        let entry = cache.entry(host: "devbox", path: "/log.txt", cwd: nil)
        XCTAssertEqual(cache.cached(entry)?.file, fresh)
        // Let go, the old copy goes; the new one stays.
        cache.release(shown)
        XCTAssertFalse(FileManager.default.fileExists(atPath: shown.path))
        XCTAssertEqual(try String(contentsOf: fresh, encoding: .utf8), "new")
    }

    func testDownloadsUnderWayCountAgainstTheLimit() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        var cancels = 0
        cache.download = { _, _, dest, _, _, onProgress, done in
            dests.append(dest)
            progress.append(onProgress)
            finish.append(done)
            return { cancels += 1 }
        }
        var results: [String: Result<URL, Error>] = [:]
        cache.fetch(.path("/one.bin"), pane: pane, cwd: nil) { results["one"] = $0 }
        cache.fetch(.path("/two.bin"), pane: pane, cwd: nil) { results["two"] = $0 }
        // 8 bytes on their way, then 8 more: the second does not fit beside the first.
        progress[0](TransferProgress(done: 0, total: 8))
        XCTAssertEqual(cancels, 0)
        progress[1](TransferProgress(done: 0, total: 8))
        XCTAssertEqual(cancels, 1)
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        finish[1](cancelled)
        XCTAssertThrowsError(try XCTUnwrap(results["two"]).get()) {
            XCTAssertTrue($0.localizedDescription.contains("no room"), $0.localizedDescription)
        }
        try Data(repeating: 1, count: 8).write(to: dests[0])
        finish[0](outcome(name: "one.bin", version: "v"))
        XCTAssertNoThrow(try XCTUnwrap(results["one"]).get())
    }

    func testAnEntryThatCannotBeDeletedStillCounts() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        cache.download = downloads { _, _, dest, _, done in
            try! Data(repeating: 1, count: 8).write(to: dest)
            done(self.outcome(name: "a.bin", version: "v"))
        }
        var first: URL?
        cache.fetch(.path("/a.bin"), pane: pane, cwd: nil) { first = try? $0.get() }
        let copy = try XCTUnwrap(first).deletingLastPathComponent()
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: copy.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: copy.path) }
        let entry = cache.entry(host: "devbox", path: "/b.bin", cwd: nil)
        let staged = try cache.stage(entry)
        try Data(repeating: 2, count: 8).write(to: staged)
        _ = try cache.install(entry, staged: staged, name: "b.bin", version: "v")
        // 8 undeletable and 8 new, over 10: the undeletable copy is not counted as freed, so
        // the new one is not kept.
        XCTAssertNil(cache.cached(entry))
        XCTAssertTrue(FileManager.default.fileExists(atPath: try XCTUnwrap(first).path))
        XCTAssertTrue(cache.prune())
    }

    func testARelativePathIsTheDirectoryTheClickHad() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        var asked: [String] = []
        cache.download = downloads { _, path, dest, _, done in
            asked.append(path)
            try! Data("n".utf8).write(to: dest)
            done(self.outcome(name: "notes.md", version: "v"))
        }
        // The pane may be elsewhere by the time the host is asked: the clicked directory goes.
        cache.fetch(.path("notes.md"), pane: pane, cwd: "/work") { _ in }
        cache.fetch(.path("~/todo.txt"), pane: pane, cwd: "/work") { _ in }
        cache.fetch(.path("/etc/hosts"), pane: pane, cwd: nil) { _ in }
        XCTAssertEqual(asked, ["/work/notes.md", "~/todo.txt", "/etc/hosts"])
        var result: Result<URL, Error>?
        cache.fetch(.path("notes.md"), pane: pane, cwd: nil) { result = $0 }
        XCTAssertThrowsError(try XCTUnwrap(result).get()) {
            XCTAssertEqual($0 as? FilePreviewError, .noDirectory)
        }
        XCTAssertEqual(asked.count, 3)
    }

    func testARefreshTakesItsOldCopysPlaceUnlessItIsShown() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        var cancels = 0
        cache.download = { _, _, dest, _, _, onProgress, done in
            dests.append(dest)
            progress.append(onProgress)
            finish.append(done)
            return { cancels += 1 }
        }
        var got: [URL] = []
        func fetchAndFinish(_ content: UInt8, version: String) {
            cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { if let u = try? $0.get() { got.append(u) } }
            progress.last!(TransferProgress(done: 0, total: 8))
            try! Data(repeating: content, count: 8).write(to: dests.last!)
            finish.last!(outcome(name: "big.bin", version: version))
        }
        fetchAndFinish(1, version: "v1")
        // 8 kept and 8 more for its refresh, over 10: but the new copy replaces the old.
        fetchAndFinish(2, version: "v2")
        XCTAssertEqual(cancels, 0)
        XCTAssertEqual(got.count, 2)
        XCTAssertEqual(try Data(contentsOf: got[1]), Data(repeating: 2, count: 8))
        // Shown in Quick Look, the old copy stays, and both do not fit.
        cache.hold(got[1])
        cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { _ in }
        progress.last!(TransferProgress(done: 0, total: 8))
        XCTAssertEqual(cancels, 1)
    }

    func testARefreshDoesNotTakeThePlaceOfACopyAnotherInstanceShows() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        var cancels = 0
        cache.download = { _, _, dest, _, _, onProgress, done in
            dests.append(dest)
            progress.append(onProgress)
            finish.append(done)
            return { cancels += 1 }
        }
        cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { _ in }
        progress.last!(TransferProgress(done: 0, total: 8))
        try Data(repeating: 1, count: 8).write(to: dests.last!)
        finish.last!(outcome(name: "big.bin", version: "v1"))
        // Another running instance (the test runner's parent) shows the copy: it stays, so
        // the refresh's 8 come on top of it, over 10.
        let entry = cache.entry(host: "devbox", path: "/big.bin", cwd: nil)
        FileManager.default.createFile(atPath: entry.appendingPathComponent(".use-\(getppid())").path,
                                       contents: nil)
        cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { _ in }
        progress.last!(TransferProgress(done: 0, total: 8))
        XCTAssertEqual(cancels, 1)
    }

    func testAFailedVersionWriteNeverPairsANewCopyWithTheOldVersion() throws {
        let cache = tempCache()
        let entry = cache.entry(host: "devbox", path: "/a.txt", cwd: nil)
        let first = try cache.stage(entry)
        try Data("old".utf8).write(to: first)
        _ = try cache.install(entry, staged: first, name: "a.txt", version: "v1")
        // The disk fills as the new version is written.
        cache.writeText = { text, url in
            if url.lastPathComponent == ".version" { throw CocoaError(.fileWriteOutOfSpace) }
            try text.write(to: url, atomically: true, encoding: .utf8)
        }
        let second = try cache.stage(entry)
        try Data("new".utf8).write(to: second)
        // The copy is in and shown; only its version is not recorded, so it is fetched again.
        let installed = try cache.install(entry, staged: second, name: "a.txt", version: "v2")
        XCTAssertEqual(try String(contentsOf: installed, encoding: .utf8), "new")
        if let cached = cache.cached(entry) {
            XCTAssertFalse(cached.version == "v1" && (try? String(contentsOf: cached.file, encoding: .utf8)) == "new")
        }
        XCTAssertNil(cache.cached(entry))
    }

    func testACopyOverTheLimitIsShownButNotKept() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        cache.download = { _, _, dest, _, _, onProgress, done in
            dests.append(dest)
            progress.append(onProgress)
            finish.append(done)
            return {}
        }
        var got: [URL] = []
        cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { if let u = try? $0.get() { got.append(u) } }
        progress[0](TransferProgress(done: 0, total: 8))
        try Data(repeating: 1, count: 8).write(to: dests[0])
        finish[0](outcome(name: "big.bin", version: "v1"))
        // A refresh fits in place of the old copy, but Quick Look shows that copy before the new
        // one is in: both cannot stay.
        cache.fetch(.path("/big.bin"), pane: pane, cwd: nil) { if let u = try? $0.get() { got.append(u) } }
        progress[1](TransferProgress(done: 0, total: 8))
        cache.hold(got[0])
        try Data(repeating: 2, count: 8).write(to: dests[1])
        finish[1](outcome(name: "big.bin", version: "v2"))
        XCTAssertEqual(try Data(contentsOf: got[1]), Data(repeating: 2, count: 8))
        let entry = cache.entry(host: "devbox", path: "/big.bin", cwd: nil)
        XCTAssertNil(cache.cached(entry))
        cache.hold(got[1])
        // Let go, neither is kept past the limit.
        cache.release(got[0])
        cache.release(got[1])
        XCTAssertFalse(FileManager.default.fileExists(atPath: got[0].path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: got[1].path))
        XCTAssertTrue(cache.prune())
    }

    func testQuickLookHoldsTheNextPreviewBeforeLettingTheLastGo() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        func fetch(_ path: String) -> URL? {
            cache.download = downloads { _, _, dest, _, done in
                try! Data(repeating: 1, count: 8).write(to: dest)
                done(self.outcome(name: (path as NSString).lastPathComponent, version: "v"))
            }
            var got: URL?
            cache.fetch(.path(path), pane: pane, cwd: nil) { got = try? $0.get() }
            return got
        }
        let first = try XCTUnwrap(fetch("/first.bin"))
        cache.switchHeld(from: nil, to: first)
        // Over the limit beside the shown one: shown, not kept.
        let second = try XCTUnwrap(fetch("/second.bin"))
        cache.switchHeld(from: first, to: second)
        XCTAssertTrue(FileManager.default.fileExists(atPath: second.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: first.path))
        cache.switchHeld(from: second, to: nil)
        XCTAssertFalse(FileManager.default.fileExists(atPath: second.path))
    }

    func testAFetchReportsProgressAndCancelsOnlyWhenNoOneWaits() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        var cancels = 0
        cache.download = { _, _, dest, _, _, onProgress, done in
            dests.append(dest)
            progress.append(onProgress)
            finish.append(done)
            return { cancels += 1 }
        }
        var seen: [[UInt64]] = [[], []]
        var results: [Result<URL, Error>?] = [nil, nil]
        let cancelFirst = cache.fetch(.path("/a.bin"), pane: pane, cwd: nil,
                                      progress: { seen[0].append($0.done) }) { results[0] = $0 }
        cache.fetch(.path("/a.bin"), pane: pane, cwd: nil,
                    progress: { seen[1].append($0.done) }) { results[1] = $0 }
        XCTAssertEqual(progress.count, 1)
        progress[0](TransferProgress(done: 1, total: 2))
        XCTAssertEqual(seen, [[1], [1]])
        // One of two gives up: it is told so, and the download goes on for the other.
        cancelFirst()
        XCTAssertThrowsError(try XCTUnwrap(results[0]).get()) {
            XCTAssertEqual($0 as? RemoteFileError, .cancelled)
        }
        XCTAssertEqual(cancels, 0)
        try Data("ab".utf8).write(to: dests[0])
        finish[0](outcome(name: "a.bin", version: "v"))
        XCTAssertEqual(try String(contentsOf: try XCTUnwrap(results[1]).get(), encoding: .utf8), "ab")
        // Alone, giving up stops the download.
        var last: Result<URL, Error>?
        let cancel = cache.fetch(.path("/b.bin"), pane: pane, cwd: nil) { last = $0 }
        cancel()
        XCTAssertEqual(cancels, 1)
        XCTAssertThrowsError(try XCTUnwrap(last).get()) {
            XCTAssertEqual($0 as? RemoteFileError, .cancelled)
        }
    }

    func testAFileAskedForAgainAfterACancelIsFetchedAfresh() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        var cancels = 0
        cache.download = { _, _, dest, _, _, _, done in
            dests.append(dest)
            finish.append(done)
            return { cancels += 1 }
        }
        let cancel = cache.fetch(.path("/a.bin"), pane: pane, cwd: nil) { _ in }
        cancel()
        XCTAssertEqual(cancels, 1)
        // Asked for again before the cancelled download has ended: a download of its own.
        var again: Result<URL, Error>?
        cache.fetch(.path("/a.bin"), pane: pane, cwd: nil) { again = $0 }
        XCTAssertEqual(finish.count, 2)
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        finish[0](cancelled)
        XCTAssertNil(again)
        try Data("a".utf8).write(to: dests[1])
        finish[1](outcome(name: "a.bin", version: "v"))
        XCTAssertEqual(try String(contentsOf: try XCTUnwrap(again).get(), encoding: .utf8), "a")
    }

    func testAnAbandonedDownloadThatEndsLateLeavesTheNewerCopy() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        var finish: [(TransferOutcome) -> Void] = []
        var dests: [URL] = []
        cache.download = { _, _, dest, _, _, _, done in
            dests.append(dest)
            finish.append(done)
            return {}
        }
        let cancel = cache.fetch(.path("/a.txt"), pane: pane, cwd: nil) { _ in }
        cancel()
        var again: Result<URL, Error>?
        cache.fetch(.path("/a.txt"), pane: pane, cwd: nil) { again = $0 }
        // The new fetch is in first; the given-up one, too late to stop, ends whole after it.
        try Data("new".utf8).write(to: dests[1])
        finish[1](outcome(name: "a.txt", version: "v2"))
        try Data("old".utf8).write(to: dests[0])
        finish[0](outcome(name: "a.txt", version: "v1"))
        let entry = cache.entry(host: "devbox", path: "/a.txt", cwd: nil)
        let cached = try XCTUnwrap(cache.cached(entry))
        XCTAssertEqual(cached.version, "v2")
        XCTAssertEqual(try String(contentsOf: cached.file, encoding: .utf8), "new")
        XCTAssertEqual(try XCTUnwrap(again).get(), cached.file)
        XCTAssertFalse(FileManager.default.fileExists(atPath: dests[0].path))
    }

    func testDownloadsLeftHalfDoneAreClearedButNotOnesUnderWay() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-stale-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        // An earlier run stopped mid-download, twice.
        let entry = root.appendingPathComponent("0123", isDirectory: true)
        try FileManager.default.createDirectory(at: entry, withIntermediateDirectories: true)
        let staged = entry.appendingPathComponent(".staging-old")
        let part = entry.appendingPathComponent(".thurm-99-0.part")
        try Data(repeating: 1, count: 100).write(to: staged)
        try Data(repeating: 1, count: 100).write(to: part)
        let cache = RemoteFileCache(root: root, maxBytes: 1 << 20)
        XCTAssertFalse(FileManager.default.fileExists(atPath: staged.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: part.path))
        // A download under way keeps what it writes, whoever prunes meanwhile.
        var dests: [URL] = []
        var finish: [(TransferOutcome) -> Void] = []
        cache.download = { _, _, dest, _, _, _, done in
            dests.append(dest)
            finish.append(done)
            return {}
        }
        cache.fetch(.path("/live.bin"), pane: PaneKey("devbox", 3), cwd: nil) { _ in }
        try Data("live".utf8).write(to: dests[0])
        XCTAssertTrue(cache.prune())
        XCTAssertTrue(FileManager.default.fileExists(atPath: dests[0].path))
        finish[0](outcome(name: "live.bin", version: "v"))
        // One that cannot be deleted counts against the limit.
        let small = RemoteFileCache(root: root, maxBytes: 50)
        let stuck = root.appendingPathComponent("4567", isDirectory: true)
        try FileManager.default.createDirectory(at: stuck, withIntermediateDirectories: true)
        try Data(repeating: 1, count: 100).write(to: stuck.appendingPathComponent(".staging-stuck"))
        try FileManager.default.setAttributes([.posixPermissions: 0o500], ofItemAtPath: stuck.path)
        defer { try? FileManager.default.setAttributes([.posixPermissions: 0o700], ofItemAtPath: stuck.path) }
        XCTAssertFalse(small.prune(keeping: stuck))
    }

    func testAShownCopyIsKnownWhateverItsFolderURLLooksLike() throws {
        let cache = tempCache()
        let pane = PaneKey("devbox", 3)
        func fetch(_ content: String, version: String) -> URL? {
            cache.download = downloads { _, _, dest, _, done in
                try! Data(content.utf8).write(to: dest)
                done(self.outcome(name: "log.txt", version: version))
            }
            var got: URL?
            cache.fetch(.path("/log.txt"), pane: pane, cwd: nil) { got = try? $0.get() }
            return got
        }
        let shown = try XCTUnwrap(fetch("old", version: "v1"))
        // Held under a URL spelled another way: through its folder with a trailing slash.
        let respelled = URL(fileURLWithPath: shown.deletingLastPathComponent().path + "/", isDirectory: true)
            .appendingPathComponent(shown.lastPathComponent)
        cache.hold(respelled)
        _ = try XCTUnwrap(fetch("new", version: "v2"))
        XCTAssertEqual(try String(contentsOf: shown, encoding: .utf8), "old")
        cache.release(respelled)
        XCTAssertFalse(FileManager.default.fileExists(atPath: shown.path))
    }

    func testAnotherRunningInstancesDownloadIsLeftAlone() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-owner-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let entry = root.appendingPathComponent("89ab", isDirectory: true)
        try FileManager.default.createDirectory(at: entry, withIntermediateDirectories: true)
        // The test runner's parent runs; a pid this high does not.
        let live = entry.appendingPathComponent(".staging-\(getppid())-x")
        let livePart = entry.appendingPathComponent(".thurm-\(getppid())-0.part")
        let dead = entry.appendingPathComponent(".staging-99999999-x")
        let mine = entry.appendingPathComponent(".staging-\(getpid())-x")
        // Made before the running process by that number started: an ended one's, its number
        // used again since.
        let reused = entry.appendingPathComponent(".staging-\(getppid())-old")
        let reusedPart = entry.appendingPathComponent(".thurm-\(getppid())-1.part")
        for f in [live, livePart, dead, mine, reused, reusedPart] { try Data("x".utf8).write(to: f) }
        let started = try XCTUnwrap(RemoteFileCache.startTime(getppid()))
        for f in [reused, reusedPart] {
            try FileManager.default.setAttributes([.creationDate: started.addingTimeInterval(-3600)],
                                                  ofItemAtPath: f.path)
        }
        XCTAssertEqual(RemoteFileCache.downloadOwner(live), getppid())
        _ = RemoteFileCache(root: root)
        XCTAssertTrue(FileManager.default.fileExists(atPath: live.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: livePart.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: dead.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: mine.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: reused.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: reusedPart.path))
    }

    func testLettingAPreviewGoWalksTheCacheOnlyWhenOverItsLimit() throws {
        let cache = tempCache(maxBytes: 10)
        let pane = PaneKey("devbox", 3)
        func fetch(_ path: String, bytes: Int) -> URL? {
            cache.download = downloads { _, _, dest, _, done in
                try! Data(repeating: 1, count: bytes).write(to: dest)
                done(self.outcome(name: (path as NSString).lastPathComponent, version: "v"))
            }
            var got: URL?
            cache.fetch(.path(path), pane: pane, cwd: nil) { got = try? $0.get() }
            return got
        }
        let small = try XCTUnwrap(fetch("/small.bin", bytes: 2))
        cache.hold(small)
        let before = cache.prunes
        cache.release(small)
        XCTAssertEqual(cache.prunes, before)
        // Over the limit with a shown copy in the way: letting it go prunes.
        let shown = try XCTUnwrap(fetch("/a.bin", bytes: 8))
        cache.hold(shown)
        let other = try XCTUnwrap(fetch("/b.bin", bytes: 8))
        cache.hold(other)
        let walked = cache.prunes
        cache.release(shown)
        XCTAssertEqual(cache.prunes, walked + 1)
        XCTAssertFalse(FileManager.default.fileExists(atPath: shown.path))
    }

    func testAnEntryAnotherInstanceUsesIsNotPruned() throws {
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-uses-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        func entry(_ name: String, usedBy pid: pid_t) throws -> URL {
            let e = root.appendingPathComponent(name, isDirectory: true)
            let copy = e.appendingPathComponent("c-1", isDirectory: true)
            try FileManager.default.createDirectory(at: copy, withIntermediateDirectories: true)
            try Data(repeating: 1, count: 8).write(to: copy.appendingPathComponent("f"))
            try "c-1/f".write(to: e.appendingPathComponent(".current"), atomically: true, encoding: .utf8)
            FileManager.default.createFile(atPath: e.appendingPathComponent(".use-\(pid)").path, contents: nil)
            return e
        }
        // One used by a running instance (the test runner's parent), one by an ended one.
        let live = try entry("aaaa", usedBy: getppid())
        let gone = try entry("bbbb", usedBy: 99999999)
        _ = RemoteFileCache(root: root, maxBytes: 4)
        XCTAssertTrue(FileManager.default.fileExists(atPath: live.path))
        XCTAssertFalse(FileManager.default.fileExists(atPath: gone.path))
    }

    func testAVersionGoesWithItsOwnCopyWhoeverInstallsAtOnce() throws {
        let cache = tempCache()
        let other = RemoteFileCache(root: cache.root)
        let entry = cache.entry(host: "devbox", path: "/a.txt", cwd: nil)
        func install(_ by: RemoteFileCache, _ content: String, _ version: String) throws -> URL {
            let staged = try by.stage(entry)
            try Data(content.utf8).write(to: staged)
            return try by.install(entry, staged: staged, name: "a.txt", version: version)
        }
        // Two instances install the same entry in turn: whichever copy is in use, the version
        // read with it is its own.
        _ = try install(cache, "one", "v1")
        _ = try install(other, "two", "v2")
        let seen = try XCTUnwrap(cache.cached(entry))
        XCTAssertEqual(seen.version, "v2")
        XCTAssertEqual(try String(contentsOf: seen.file, encoding: .utf8), "two")
        // A version left beside the entry by an older build is no copy's.
        try "stale".write(to: entry.appendingPathComponent(".version"), atomically: true, encoding: .utf8)
        XCTAssertEqual(cache.cached(entry)?.version, "v2")
    }
}
