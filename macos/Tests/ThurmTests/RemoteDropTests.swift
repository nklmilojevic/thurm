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

    /// A remote pane whose host takes files, recording each upload instead of sending it.
    private func remotePane(takesFiles: Bool = true) -> (TerminalView, () -> [[String]]) {
        let view = TerminalView(pane: PaneKey("devbox", 7))
        // No connection in the tests: the pane would show as offline and take no input.
        view.setOffline(nil)
        view.transfersFiles = takesFiles
        var uploads: [[String]] = []
        view.uploadFiles = { files in uploads.append(files.map(\.path)) }
        return (view, { uploads })
    }

    func testADropOrPasteSendsTheFilesAsOneBatchInOrder() throws {
        let urls = [try file("one.txt"), try file("two words.png"), try file("three")]
        pb.clearContents()
        pb.writeObjects(urls as [NSURL])
        let (view, uploads) = remotePane()
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        view.paste(from: pb)
        XCTAssertEqual(uploads(), [urls.map(\.path), urls.map(\.path)])
    }

    func testAnotherToastDoesNotTakeTheUploadsCancelAway() {
        let (view, _) = remotePane()
        view.showUploadToast("Sending 3 files to devbox…")
        view.showToast("Session restored", duration: 0.05)
        // Its fade has begun, and its removal is due, when the next progress comes.
        RunLoop.current.run(until: Date().addingTimeInterval(0.08))
        view.showUploadToast("Sending 3 files to devbox… 40%")
        RunLoop.current.run(until: Date().addingTimeInterval(0.6))
        XCTAssertTrue(view.showsUploadToast)
    }

    func testAFolderOrAnOlderHostSendsNothing() throws {
        let folder = dir.appendingPathComponent("folder", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        pb.clearContents()
        pb.writeObjects([try file("one.txt"), folder] as [NSURL])
        let (view, uploads) = remotePane()
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        XCTAssertEqual(uploads(), [])
        pb.clearContents()
        pb.writeObjects([try file("two.txt")] as [NSURL])
        let (older, olderUploads) = remotePane(takesFiles: false)
        _ = older.performDragOperation(FakeDrag(pb))
        XCTAssertEqual(olderUploads(), [])
    }

    func testDroppedFilesArePlannedInOrder() throws {
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

    func testDownloadsReserveAFreeName() throws {
        let first = try XCTUnwrap(SessionManager.reserveFreeName("a.txt", in: dir))
        XCTAssertEqual(first.lastPathComponent, "a.txt")
        // Reserved at once: a second download of the same name, started before the first
        // finished, gets another.
        let second = try XCTUnwrap(SessionManager.reserveFreeName("a.txt", in: dir))
        XCTAssertEqual(second.lastPathComponent, "a 2.txt")
        XCTAssertTrue(FileManager.default.fileExists(atPath: first.path))
        XCTAssertTrue(FileManager.default.fileExists(atPath: second.path))
        _ = try file("Makefile")
        XCTAssertEqual(SessionManager.reserveFreeName("Makefile", in: dir)?.lastPathComponent, "Makefile 2")
        XCTAssertNil(SessionManager.reserveFreeName("x", in: dir.appendingPathComponent("missing")))
    }
}

/// A drop of `draggingPasteboard`, for `performDragOperation`.
private final class FakeDrag: NSObject, NSDraggingInfo {
    let draggingPasteboard: NSPasteboard

    init(_ pb: NSPasteboard) {
        draggingPasteboard = pb
    }

    var draggingDestinationWindow: NSWindow? { nil }
    var draggingSourceOperationMask: NSDragOperation { .copy }
    var draggingLocation: NSPoint { .zero }
    var draggedImageLocation: NSPoint { .zero }
    var draggedImage: NSImage? { nil }
    var draggingSource: Any? { nil }
    var draggingSequenceNumber: Int { 1 }
    func slideDraggedImage(to screenPoint: NSPoint) {}
    var draggingFormation: NSDraggingFormation = .default
    var animatesToDestination = false
    var numberOfValidItemsForDrop = 1
    func enumerateDraggingItems(options enumOpts: NSDraggingItemEnumerationOptions = [], for view: NSView?,
                                classes classArray: [AnyClass],
                                searchOptions: [NSPasteboard.ReadingOptionKey: Any] = [:],
                                using block: (NSDraggingItem, Int, UnsafeMutablePointer<ObjCBool>) -> Void) {}
    var springLoadingHighlight: NSSpringLoadingHighlight { .none }
    func resetSpringLoading() {}
}

