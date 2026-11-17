import AppKit
import XCTest
@testable import Thurm

/// What clipboard tools on a host (pbpaste, xclip -o, wl-paste) get from this Mac's clipboard.
final class ClipboardToolsTests: XCTestCase {
    private var pb: NSPasteboard!

    override func setUp() {
        pb = NSPasteboard(name: NSPasteboard.Name("thurm-clip-tests-\(UUID().uuidString)"))
        pb.clearContents()
    }

    override func tearDown() {
        pb.releaseGlobally()
    }

    private func png() -> Data {
        let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: 2, pixelsHigh: 2, bitsPerSample: 8,
                                   samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                                   colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
        return rep.representation(using: .png, properties: [:])!
    }

    func testTextComesAsUTF8() {
        pb.setString("héllo", forType: .string)
        XCTAssertEqual(Remotes.clipboardTypes(pb), ["text/plain"])
        XCTAssertEqual(Remotes.clipboardData(pb, mime: "text/plain"), Data("héllo".utf8))
        XCTAssertNil(Remotes.clipboardData(pb, mime: "image/png"))
    }

    func testAnEmptyClipboardHasNothing() {
        XCTAssertEqual(Remotes.clipboardTypes(pb), [])
        XCTAssertNil(Remotes.clipboardData(pb, mime: "text/plain"))
        XCTAssertNil(Remotes.clipboardData(pb, mime: "image/png"))
    }

    func testImagesComeAsPNG() {
        let data = png()
        pb.setData(data, forType: .png)
        XCTAssertEqual(Remotes.clipboardTypes(pb), ["image/png"])
        XCTAssertEqual(Remotes.clipboardData(pb, mime: "image/png"), data)
    }

    /// Screenshots copy as TIFF.
    func testTIFFIsConvertedToPNG() {
        let tiff = NSBitmapImageRep(data: png())!.tiffRepresentation!
        pb.setData(tiff, forType: .tiff)
        XCTAssertEqual(Remotes.clipboardTypes(pb), ["image/png"])
        let out = Remotes.clipboardData(pb, mime: "image/png")!
        XCTAssertEqual(Array(out.prefix(4)), [0x89, 0x50, 0x4E, 0x47])
    }

    func testOnlyTheTypesToolsAskFor() {
        pb.setString("x", forType: .string)
        XCTAssertNil(Remotes.clipboardData(pb, mime: "TARGETS"))
        XCTAssertNil(Remotes.clipboardData(pb, mime: "image/jpeg"))
    }

    func testMoreThan32MiBIsRefused() {
        pb.setString(String(repeating: "a", count: Remotes.clipMax + 1), forType: .string)
        XCTAssertNil(Remotes.clipboardData(pb, mime: "text/plain"))
    }
}
