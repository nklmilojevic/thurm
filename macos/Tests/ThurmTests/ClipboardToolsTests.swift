import AppKit
import CThurm
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

    /// Listing the types must not fetch them: a lazy provider is asked only for a paste.
    func testListingTypesReadsNothing() {
        final class Provider: NSObject, NSPasteboardItemDataProvider {
            var asked = 0
            func pasteboard(_ pasteboard: NSPasteboard?, item: NSPasteboardItem,
                            provideDataForType type: NSPasteboard.PasteboardType) {
                asked += 1
                item.setString("lazy", forType: type)
            }
        }
        let provider = Provider()
        let item = NSPasteboardItem()
        item.setDataProvider(provider, forTypes: [.string])
        pb.writeObjects([item])
        XCTAssertEqual(Remotes.clipboardTypes(pb), ["text/plain"])
        XCTAssertEqual(provider.asked, 0, "the types are listed without fetching the text")
        XCTAssertEqual(Remotes.clipboardData(pb, mime: "text/plain"), Data("lazy".utf8))
        XCTAssertEqual(provider.asked, 1)
    }

    /// OSC 52 reads and clipboard tools' pastes share one decision; listing types asks nothing
    /// but is refused where reads are.
    func testReadsAndToolPastesAreDecidedAlike() {
        XCTAssertEqual(Remotes.clipboardRead(policy: nil, listing: false), .allow, "this Mac's panes")
        XCTAssertEqual(Remotes.clipboardRead(policy: "allow", listing: false), .allow)
        XCTAssertEqual(Remotes.clipboardRead(policy: "ask", listing: false), .ask)
        XCTAssertEqual(Remotes.clipboardRead(policy: "ask", listing: true), .allow)
        XCTAssertEqual(Remotes.clipboardRead(policy: "deny", listing: false), .deny)
        XCTAssertEqual(Remotes.clipboardRead(policy: "deny", listing: true), .deny)
        XCTAssertEqual(Remotes.clipboardRead(policy: "bogus", listing: true), .deny)
    }

    /// This Mac's `terminal.osc52` caps a remote host's tool pastes, as it caps OSC 52 reads: the
    /// policy the app asks for (`clipboard_read`, through the FFI) denies them unless it allows
    /// pastes, even for a host whose `clipboard_read` is "always".
    func testThisMacsOSC52CapsToolPastes() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("thurm-clip-policy-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        let saved = ProcessInfo.processInfo.environment["THURM_CONFIG_DIR"]
        setenv("THURM_CONFIG_DIR", dir.path, 1)
        defer {
            if let saved { setenv("THURM_CONFIG_DIR", saved, 1) } else { unsetenv("THURM_CONFIG_DIR") }
            try? FileManager.default.removeItem(at: dir)
        }
        func policy(osc52: String) throws -> String? {
            try "[terminal]\nosc52 = \"\(osc52)\"\n[[remote]]\nname = \"devbox\"\nhost = \"devbox\"\nclipboard_read = \"always\"\n"
                .write(to: dir.appendingPathComponent("config.toml"), atomically: true, encoding: .utf8)
            guard let raw = thurm_remote_call(#"{"op":"clipboard_read","host":"devbox"}"#) else { return nil }
            defer { thurm_string_free(raw) }
            return JSON.decode(String(cString: raw)) as? String
        }
        for osc52 in ["copy", "disabled"] {
            let p = try policy(osc52: osc52)
            XCTAssertEqual(Remotes.clipboardRead(policy: p, listing: false), .deny, osc52)
            XCTAssertEqual(Remotes.clipboardRead(policy: p, listing: true), .deny, "\(osc52): types too")
        }
        for osc52 in ["paste", "copy-paste"] {
            XCTAssertEqual(Remotes.clipboardRead(policy: try policy(osc52: osc52), listing: false), .allow, osc52)
        }
    }

    /// After a clear, listing the types finds none: not even empty text.
    func testAClearLeavesNoType() {
        pb.setString("secret", forType: .string)
        Remotes.clearClipboard(pb)
        XCTAssertEqual(Remotes.clipboardTypes(pb), [])
        XCTAssertNil(Remotes.clipboardData(pb, mime: "text/plain"))
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
