import AppKit
import XCTest
@testable import Thurm

/// What a drop or paste on a remote pane sends to its host.
final class RemoteDropTests: XCTestCase {
    private var pb: NSPasteboard!
    private var dir: URL!

    override func setUpWithError() throws {
        pb = NSPasteboard(name: NSPasteboard.Name("thurm-tests-\(UUID().uuidString)"))
        dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-drop-\(UUID().uuidString)", isDirectory: true)
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
    }

    override func tearDown() {
        pb.releaseGlobally()
        try? FileManager.default.removeItem(at: dir)
    }

    private func file(_ name: String) throws -> URL {
        let url = dir.appendingPathComponent(name)
        try Data(name.utf8).write(to: url)
        return url
    }

    private var png: Data {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 1, pixelsHigh: 1, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                   colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        return rep.representation(using: .png, properties: [:])!
    }

    func testDroppedFilesGoAsOneBatchInOrder() throws {
        let urls = [try file("one.txt"), try file("two words.png")]
        pb.clearContents()
        pb.writeObjects(urls as [NSURL])
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: false), .files(urls))
        // Copied in Finder and pasted: the files, not their names as text.
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: true), .files(urls))
    }

    func testAFolderRefusesTheDrop() throws {
        let folder = dir.appendingPathComponent("folder", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        pb.clearContents()
        pb.writeObjects([try file("one.txt"), folder] as [NSURL])
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: false), .folder("folder"))
    }

    func testImagesGoAsFilesAndTextStaysText() {
        pb.clearContents()
        pb.setData(png, forType: .png)
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: true), .image)
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: false), .image)
        // An image that comes with text pastes the text; dropped, it is the image.
        pb.clearContents()
        pb.setData(png, forType: .png)
        pb.setString("caption", forType: .string)
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: true), .none)
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: false), .image)
        pb.clearContents()
        pb.setString("just text", forType: .string)
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: true), .none)
        pb.clearContents()
        pb.writeObjects([URL(string: "https://example.com")!] as [NSURL])
        XCTAssertEqual(TerminalView.uploadPlan(pb, pasting: false), .none)
    }

    func testDownloadsTakeAFreeName() throws {
        XCTAssertEqual(SessionManager.freeName("a.txt", in: dir).lastPathComponent, "a.txt")
        _ = try file("a.txt")
        XCTAssertEqual(SessionManager.freeName("a.txt", in: dir).lastPathComponent, "a 2.txt")
        _ = try file("a 2.txt")
        XCTAssertEqual(SessionManager.freeName("a.txt", in: dir).lastPathComponent, "a 3.txt")
        _ = try file("Makefile")
        XCTAssertEqual(SessionManager.freeName("Makefile", in: dir).lastPathComponent, "Makefile 2")
    }
}
