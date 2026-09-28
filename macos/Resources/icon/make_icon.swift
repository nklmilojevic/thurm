// Draws the Thurm app icon and writes macos/Resources/AppIcon.icns.
//   swift macos/Resources/icon/make_icon.swift
import AppKit

func draw(_ px: Int) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                               samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
                               bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let s = CGFloat(px)
    let ctx = NSGraphicsContext.current!.cgContext
    // macOS icon grid: the squircle spans ~80% of the canvas.
    let inset = s * 0.1
    let body = CGRect(x: inset, y: inset, width: s - 2 * inset, height: s - 2 * inset)
    let path = CGPath(roundedRect: body, cornerWidth: body.width * 0.225, cornerHeight: body.width * 0.225, transform: nil)
    // Soft shadow.
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.01), blur: s * 0.03,
                  color: NSColor.black.withAlphaComponent(0.35).cgColor)
    ctx.addPath(path)
    ctx.setFillColor(NSColor(srgbRed: 0.118, green: 0.118, blue: 0.180, alpha: 1).cgColor)
    ctx.fillPath()
    ctx.restoreGState()
    // Background gradient (Catppuccin Mocha base → crust).
    ctx.saveGState()
    ctx.addPath(path)
    ctx.clip()
    let colors = [NSColor(srgbRed: 0.192, green: 0.196, blue: 0.267, alpha: 1).cgColor,
                  NSColor(srgbRed: 0.067, green: 0.067, blue: 0.106, alpha: 1).cgColor] as CFArray
    let grad = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: colors, locations: [0, 1])!
    ctx.drawLinearGradient(grad, start: CGPoint(x: 0, y: body.maxY), end: CGPoint(x: 0, y: body.minY), options: [])
    // Top highlight.
    ctx.setFillColor(NSColor.white.withAlphaComponent(0.06).cgColor)
    ctx.fill(CGRect(x: body.minX, y: body.midY + body.height * 0.18, width: body.width, height: body.height * 0.32))
    ctx.restoreGState()
    // Prompt: a chevron and a cursor block.
    let ink = NSColor(srgbRed: 0.537, green: 0.706, blue: 0.980, alpha: 1)   // blue
    let accent = NSColor(srgbRed: 0.651, green: 0.890, blue: 0.631, alpha: 1) // green
    let w = body.width
    let lw = w * 0.085
    ctx.setLineCap(.round)
    ctx.setLineJoin(.round)
    ctx.setLineWidth(lw)
    ctx.setStrokeColor(ink.cgColor)
    let cx = body.minX + w * 0.30, cy = body.minY + w * 0.50, arm = w * 0.15
    ctx.move(to: CGPoint(x: cx - arm * 0.6, y: cy + arm))
    ctx.addLine(to: CGPoint(x: cx + arm * 0.6, y: cy))
    ctx.addLine(to: CGPoint(x: cx - arm * 0.6, y: cy - arm))
    ctx.strokePath()
    ctx.setFillColor(accent.cgColor)
    let cursor = CGRect(x: body.minX + w * 0.50, y: cy - arm, width: w * 0.20, height: lw * 1.1)
    ctx.addPath(CGPath(roundedRect: cursor, cornerWidth: lw * 0.3, cornerHeight: lw * 0.3, transform: nil))
    ctx.fillPath()
    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let here = URL(fileURLWithPath: CommandLine.arguments[0]).deletingLastPathComponent()
let resources = here.deletingLastPathComponent()
let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try! FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)
for base in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let name = scale == 1 ? "icon_\(base)x\(base).png" : "icon_\(base)x\(base)@2x.png"
        try! draw(base * scale).representation(using: .png, properties: [:])!.write(to: iconset.appendingPathComponent(name))
    }
}
let out = resources.appendingPathComponent("AppIcon.icns")
let p = Process()
p.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
p.arguments = ["-c", "icns", iconset.path, "-o", out.path]
try! p.run()
p.waitUntilExit()
print(p.terminationStatus == 0 ? "wrote \(out.path)" : "iconutil failed")
