// Renders the Icon Composer icon (macos/Resources/AppIcon.icon) to the PNGs used outside macOS:
// linux/rs.thurm.Thurm.png and the docs logo and favicon. The app bundle compiles the .icon
// itself (macos/build.sh, via actool).
//   swift macos/Resources/icon/make_icon.swift
import AppKit

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let resources = here.deletingLastPathComponent()
let root = resources.deletingLastPathComponent().deletingLastPathComponent()
let developer = Process()
developer.executableURL = URL(fileURLWithPath: "/usr/bin/xcode-select")
developer.arguments = ["-p"]
let pipe = Pipe()
developer.standardOutput = pipe
try! developer.run()
developer.waitUntilExit()
let developerDir = String(data: pipe.fileHandleForReading.readDataToEndOfFile(), encoding: .utf8)!
    .trimmingCharacters(in: .whitespacesAndNewlines)
let ictool = URL(fileURLWithPath: developerDir)
    .appendingPathComponent("../Applications/Icon Composer.app/Contents/Executables/ictool").standardized

let full = FileManager.default.temporaryDirectory.appendingPathComponent("thurm-icon.png")
let p = Process()
p.executableURL = ictool
p.arguments = [resources.appendingPathComponent("AppIcon.icon").path, "--export-image", "--output-file", full.path,
               "--platform", "macOS", "--rendition", "Default", "--width", "512", "--height", "512", "--scale", "2"]
p.standardOutput = FileHandle.nullDevice
try! p.run()
p.waitUntilExit()
guard p.terminationStatus == 0, let master = NSImage(contentsOf: full) else { fatalError("ictool failed") }

func render(_ px: Int, to url: URL) {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                               bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    NSGraphicsContext.current!.imageInterpolation = .high
    // ictool exports the shape full bleed; macOS draws it on an 824/1024 grid, so match that.
    let s = CGFloat(px), inset = s * 100 / 1024
    master.draw(in: NSRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset))
    NSGraphicsContext.restoreGraphicsState()
    try! rep.representation(using: .png, properties: [:])!.write(to: url)
    print("wrote \(url.path)")
}

render(256, to: root.appendingPathComponent("linux/rs.thurm.Thurm.png"))
render(512, to: root.appendingPathComponent("docs/src/assets/icon.png"))
render(64, to: root.appendingPathComponent("docs/public/favicon.png"))
