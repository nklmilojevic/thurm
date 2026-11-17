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

    /// Transfers started, standing in for Core: each with its progress, end and cancels.
    final class FakeTransfers {
        var started: [[String]] = []
        var progress: [(TransferProgress) -> Void] = []
        var finish: [(TransferOutcome) -> Void] = []
        var cancels = 0
        /// Pasted files asked about, and the answer.
        var asked: [[String]] = []
        var answer = true

        func upload(_ pane: PaneKey, _ files: [(path: String, name: String?)],
                    _ onProgress: @escaping (TransferProgress) -> Void,
                    _ onFinish: @escaping (TransferOutcome) -> Void) -> (() -> Void)? {
            started.append(files.map(\.path))
            progress.append(onProgress)
            finish.append(onFinish)
            return { [weak self] in self?.cancels += 1 }
        }

        func download(_ pane: PaneKey, _ path: String, _ dest: URL, _ ifVersion: String?, _ maxBytes: UInt64,
                      _ onProgress: @escaping (TransferProgress) -> Void,
                      _ onFinish: @escaping (TransferOutcome) -> Void) -> (() -> Void)? {
            started.append([path, dest.path])
            progress.append(onProgress)
            finish.append(onFinish)
            return { [weak self] in self?.cancels += 1 }
        }
    }

    /// The images saved for uploads now.
    private func drops() -> Set<URL> {
        let dir = FileManager.default.temporaryDirectory.appendingPathComponent("thurm-drops")
        let files = (try? FileManager.default.contentsOfDirectory(at: dir, includingPropertiesForKeys: nil)) ?? []
        return Set(files.map(\.standardizedFileURL))
    }

    /// A remote pane whose host takes files, its uploads going to a `FakeTransfers`.
    private func remotePane(takesFiles: Bool = true) -> (TerminalView, FakeTransfers) {
        let view = TerminalView(pane: PaneKey("devbox", 7))
        // No connection in the tests: the pane would show as offline and take no input.
        view.setOffline(nil)
        view.transfersFiles = takesFiles
        let fake = FakeTransfers()
        view.uploader = fake.upload
        view.confirmSend = { urls, _ in
            fake.asked.append(urls.map(\.path))
            return fake.answer
        }
        return (view, fake)
    }

    func testADropOrPasteSendsTheFilesAsOneBatchInOrder() throws {
        let urls = [try file("one.txt"), try file("two words.png"), try file("three")]
        pb.clearContents()
        pb.writeObjects(urls as [NSURL])
        let (view, fake) = remotePane()
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        XCTAssertEqual(fake.started, [urls.map(\.path)])
        fake.finish[0](TransferOutcome())
        view.paste(from: pb)
        XCTAssertEqual(fake.started, [urls.map(\.path), urls.map(\.path)])
    }

    func testASecondDropWaitsForTheFirstInsteadOfCancellingIt() throws {
        let first = [try file("a.txt")]
        let second = [try file("b.txt"), try file("c.txt")]
        let (view, fake) = remotePane()
        pb.clearContents()
        pb.writeObjects(first as [NSURL])
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        pb.clearContents()
        pb.writeObjects(second as [NSURL])
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        XCTAssertEqual(fake.started, [first.map(\.path)])
        XCTAssertEqual(fake.cancels, 0)
        XCTAssertEqual(view.shownTransfer, "Sending a.txt to devbox… (1 more)")
        // The first lands; then the second goes.
        fake.finish[0](TransferOutcome())
        XCTAssertEqual(fake.started, [first.map(\.path), second.map(\.path)])
        XCTAssertEqual(fake.cancels, 0)
        XCTAssertEqual(view.shownTransfer, "Sending 2 files to devbox…")
        // Cancel stops the one shown, and only it.
        view.cancelShownTransfer(nil)
        XCTAssertEqual(fake.cancels, 1)
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        fake.finish[1](cancelled)
        XCTAssertNil(view.shownTransfer)
    }

    func testAnotherToastDoesNotTakeTheUploadsCancelAway() throws {
        let (view, fake) = remotePane()
        pb.clearContents()
        pb.writeObjects([try file("a.txt")] as [NSURL])
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        view.showToast("Session restored", duration: 0.05)
        // Its fade has begun, and its removal is due, when the next progress comes.
        RunLoop.current.run(until: Date().addingTimeInterval(0.08))
        fake.progress[0](TransferProgress(done: 2, total: 5))
        RunLoop.current.run(until: Date().addingTimeInterval(0.6))
        XCTAssertEqual(view.shownTransfer, "Sending a.txt to devbox… 40%")
    }

    func testADownloadCanBeCancelledAndLeavesNoFile() throws {
        let (view, fake) = remotePane()
        var revealed: [URL] = []
        view.download(remotePath: "/srv/big.iso", into: dir, start: fake.download) { revealed.append($0) }
        let dest = dir.appendingPathComponent("big.iso")
        // The name is reserved at once.
        XCTAssertEqual(fake.started, [["/srv/big.iso", dest.path]])
        XCTAssertTrue(FileManager.default.fileExists(atPath: dest.path))
        fake.progress[0](TransferProgress(done: 1, total: 4))
        XCTAssertEqual(view.shownTransfer, "Downloading big.iso from devbox… 25%")
        view.cancelShownTransfer(nil)
        XCTAssertEqual(fake.cancels, 1)
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        fake.finish[0](cancelled)
        XCTAssertFalse(FileManager.default.fileExists(atPath: dest.path))
        XCTAssertNil(view.shownTransfer)
        XCTAssertEqual(revealed, [])
        // One that finishes is shown in Finder, with what was saved in it.
        view.download(remotePath: "/srv/big.iso", into: dir, start: fake.download) { revealed.append($0) }
        try Data("iso bytes".utf8).write(to: URL(fileURLWithPath: fake.started[1][1]))
        fake.finish[1](TransferOutcome())
        XCTAssertEqual(revealed, [dest])
        XCTAssertEqual(try String(contentsOf: dest, encoding: .utf8), "iso bytes")
    }

    func testATransfersCancelComesBackAfterAnotherToast() throws {
        let (view, _) = remotePane()
        pb.clearContents()
        pb.writeObjects([try file("a.txt")] as [NSURL])
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        // A message comes and goes, and the transfer reports nothing more.
        view.showToast("Session restored", duration: 0.05)
        XCTAssertNil(view.shownTransfer)
        RunLoop.current.run(until: Date().addingTimeInterval(0.6))
        XCTAssertEqual(view.shownTransfer, "Sending a.txt to devbox…")
    }

    func testAPastedOrDroppedImageIsSentAsAFile() throws {
        let (view, fake) = remotePane()
        pb.clearContents()
        pb.setData(png, forType: .png)
        view.paste(from: pb)
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        // The second waits for the first; each is the image as a PNG file while it is sent.
        for i in 0..<2 {
            XCTAssertEqual(fake.started.count, i + 1)
            let batch = fake.started[i]
            XCTAssertEqual(batch.count, 1)
            XCTAssertTrue(batch[0].hasSuffix(".png"), batch[0])
            XCTAssertEqual(try Data(contentsOf: URL(fileURLWithPath: batch[0])), png)
            fake.finish[i](TransferOutcome())
        }
    }

    func testPastedTextIsPastedEvenWithAFileBesideIt() throws {
        let secret = try file("id_ed25519")
        let (view, fake) = remotePane()
        // An app copied some text and put a file's address beside it.
        pb.clearContents()
        pb.writeObjects([secret] as [NSURL])
        pb.setString("make test", forType: .string)
        view.paste(from: pb)
        XCTAssertEqual(fake.started, [])
        // Finder's copy of files has their names as its text: the files go.
        let copied = [try file("one.txt"), try file("two.txt")]
        pb.clearContents()
        pb.writeObjects(copied as [NSURL])
        pb.setString("one.txt\rtwo.txt", forType: .string)
        view.paste(from: pb)
        XCTAssertEqual(fake.started, [copied.map(\.path)])
    }

    func testAnImageMadeForAnUploadGoesWhenTheUploadEnds() throws {
        let (view, fake) = remotePane()
        pb.clearContents()
        pb.setData(png, forType: .png)
        view.paste(from: pb)
        view.paste(from: pb)
        let first = fake.started[0][0]
        XCTAssertTrue(FileManager.default.fileExists(atPath: first))
        fake.finish[0](TransferOutcome())
        XCTAssertFalse(FileManager.default.fileExists(atPath: first))
        // The second waited, then went; cancelled, its image goes too.
        let second = fake.started[1][0]
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        fake.finish[1](cancelled)
        XCTAssertFalse(FileManager.default.fileExists(atPath: second))
        // One waiting behind a file of the user's goes once sent and ended; the user's file
        // stays. (A waiting batch is dropped, with its image, when its pane's view goes:
        // testAViewGoingAwayEndsItsTransfers.)
        let user = try file("mine.txt")
        pb.clearContents()
        pb.writeObjects([user] as [NSURL])
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        pb.clearContents()
        pb.setData(png, forType: .png)
        let before = drops()
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        let added = drops().subtracting(before)
        XCTAssertEqual(added.count, 1, "\(added)")
        let image = try XCTUnwrap(added.first)
        fake.finish[2](TransferOutcome())
        XCTAssertEqual(fake.started.count, 4)
        fake.finish[3](cancelled)
        XCTAssertFalse(FileManager.default.fileExists(atPath: image.path))
        // A file of the user's is never deleted.
        XCTAssertTrue(FileManager.default.fileExists(atPath: user.path))
    }

    func testPastedFilesGoOnlyOnceTheUserSaysSo() throws {
        let files = [try file("one.txt"), try file("two.txt")]
        let (view, fake) = remotePane()
        pb.clearContents()
        pb.writeObjects(files as [NSURL])
        fake.answer = false
        view.paste(from: pb)
        XCTAssertEqual(fake.asked, [files.map(\.path)])
        XCTAssertEqual(fake.started, [])
        fake.answer = true
        view.paste(from: pb)
        XCTAssertEqual(fake.started, [files.map(\.path)])
        // A drop is its own consent; a pasted image names no file of this Mac.
        fake.finish[0](TransferOutcome())
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        fake.finish[1](TransferOutcome())
        pb.clearContents()
        pb.setData(png, forType: .png)
        view.paste(from: pb)
        XCTAssertEqual(fake.asked.count, 2)
        XCTAssertEqual(fake.started.count, 3)
    }

    func testAViewGoingAwayEndsItsTransfers() throws {
        let fake = FakeTransfers()
        let before = drops()
        autoreleasepool {
            let view = TerminalView(pane: PaneKey("devbox", 8))
            view.setOffline(nil)
            view.transfersFiles = true
            view.uploader = fake.upload
            pb.clearContents()
            pb.setData(png, forType: .png)
            view.paste(from: pb)
            view.paste(from: pb)
        }
        // The one being sent is cancelled; the one waiting is dropped with its image.
        XCTAssertEqual(fake.started.count, 1)
        XCTAssertEqual(fake.cancels, 1)
        let left = drops().subtracting(before)
        XCTAssertEqual(left.map(\.path), [fake.started[0][0]].map { URL(fileURLWithPath: $0).standardizedFileURL.path })
        // The sent one's image goes when its upload ends, and the dropped one never starts.
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        fake.finish[0](cancelled)
        XCTAssertEqual(drops().subtracting(before), [])
        XCTAssertEqual(fake.started.count, 1)
    }

    func testTheSendQuestionNamesEveryFile() throws {
        let urls = try (1...12).map { try file("f\($0).txt") }
        let alert = TerminalView.sendAlert(urls, "devbox")
        XCTAssertEqual(alert.messageText, "Send 12 files to devbox?")
        let list = try XCTUnwrap((alert.accessoryView as? NSScrollView)?.documentView as? NSTextView)
        // The ninth and every later one too, none behind an ellipsis.
        XCTAssertEqual(list.string, urls.map(\.path).joined(separator: "\n"))
        XCTAssertFalse(alert.informativeText.contains("…"))
    }

    func testAPreviewBeingFetchedShowsProgressAndCancels() throws {
        let (view, fake) = remotePane()
        let root = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-preview-\(UUID().uuidString)", isDirectory: true)
        defer { try? FileManager.default.removeItem(at: root) }
        let cache = RemoteFileCache(root: root)
        cache.download = fake.download
        view.previewCache = cache
        view.previewRemote(.path("/srv/notes.md"), cwd: nil)
        XCTAssertEqual(view.shownTransfer, "Fetching notes.md from devbox…")
        fake.progress[0](TransferProgress(done: 1, total: 2))
        XCTAssertEqual(view.shownTransfer, "Fetching notes.md from devbox… 50%")
        view.cancelShownTransfer(nil)
        XCTAssertEqual(fake.cancels, 1)
        XCTAssertNil(view.shownTransfer)
        var cancelled = TransferOutcome(error: "cancelled")
        cancelled.cancelled = true
        fake.finish[0](cancelled)
        XCTAssertNil(view.shownTransfer)
        // Nothing staged is left of it.
        let entries = (try? FileManager.default.subpathsOfDirectory(atPath: root.path)) ?? []
        XCTAssertEqual(entries.filter { $0.contains(".staging") }, [])
    }

    func testAFolderOrAnOlderHostSendsNothing() throws {
        let folder = dir.appendingPathComponent("folder", isDirectory: true)
        try FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        pb.clearContents()
        pb.writeObjects([try file("one.txt"), folder] as [NSURL])
        let (view, fake) = remotePane()
        XCTAssertTrue(view.performDragOperation(FakeDrag(pb)))
        XCTAssertEqual(fake.started, [])
        pb.clearContents()
        pb.writeObjects([try file("two.txt")] as [NSURL])
        let (older, olderFake) = remotePane(takesFiles: false)
        _ = older.performDragOperation(FakeDrag(pb))
        XCTAssertEqual(olderFake.started, [])
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
        // A name as long as names go is shortened to make room for its number.
        let long = String(repeating: "l", count: 251) + ".txt"
        XCTAssertEqual(SessionManager.reserveFreeName(long, in: dir)?.lastPathComponent, long)
        let numbered = try XCTUnwrap(SessionManager.reserveFreeName(long, in: dir)).lastPathComponent
        XCTAssertEqual(numbered.utf8.count, 255)
        XCTAssertTrue(numbered.hasSuffix(" 2.txt"), numbered)
        // So is an extension as long as names go.
        let longExt = "a." + String(repeating: "e", count: 253)
        let first2 = try XCTUnwrap(SessionManager.reserveFreeName(longExt, in: dir)).lastPathComponent
        XCTAssertEqual(first2, longExt)
        let second2 = try XCTUnwrap(SessionManager.reserveFreeName(longExt, in: dir)).lastPathComponent
        XCTAssertNotEqual(second2, first2)
        XCTAssertLessThanOrEqual(second2.utf8.count, 255)
        XCTAssertTrue(second2.hasPrefix("a 2."), second2)
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

