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

        try Data("1234".utf8).write(to: try cache.prepare(entry))
        let file = try cache.store(entry, name: "a.txt", version: "v1")
        XCTAssertEqual(file.lastPathComponent, "a.txt")
        XCTAssertEqual(cache.cached(entry)?.file, file)
        XCTAssertEqual(cache.cached(entry)?.version, "v1")
        // A file changing as it was read has no version: it is fetched again next time.
        let fresh = cache.entry(host: "devbox", path: "/b.log", cwd: nil)
        try Data("12".utf8).write(to: try cache.prepare(fresh))
        _ = try cache.store(fresh, name: "b.log", version: nil)
        XCTAssertNil(cache.cached(fresh))
        // A name from the host never leaves the entry.
        let odd = cache.entry(host: "devbox", path: "/c", cwd: nil)
        try Data("1".utf8).write(to: try cache.prepare(odd))
        XCTAssertEqual(try cache.store(odd, name: "../../x", version: "v").lastPathComponent, "file")

        // Over the limit, the least recently fetched entries go; the one just stored stays.
        try FileManager.default.setAttributes([.modificationDate: Date.distantPast], ofItemAtPath: entry.path)
        let big = cache.entry(host: "devbox", path: "/big", cwd: nil)
        try Data(repeating: 7, count: 8).write(to: try cache.prepare(big))
        _ = try cache.store(big, name: "big", version: "v")
        XCTAssertFalse(FileManager.default.fileExists(atPath: entry.path))
        XCTAssertNotNil(cache.cached(big))
    }
}
