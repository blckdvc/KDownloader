// Draws the DMG window background at 1x and 2x. Nothing sits low in the
// window: Finder's tab, path and status bars can hide the bottom.
// Build: swiftc background.swift -o bg && ./bg . && tiffutil -cathidpicheck bg.png bg@2x.png -out background.tiff
import AppKit
let W: CGFloat = 660, H: CGFloat = 420
let appX: CGFloat = 170, appsX: CGFloat = 490, iconY: CGFloat = 210  // icon centers, from the top
func hex(_ v: Int, _ a: CGFloat = 1) -> NSColor { NSColor(srgbRed: CGFloat(v >> 16) / 255, green: CGFloat((v >> 8) & 255) / 255, blue: CGFloat(v & 255) / 255, alpha: a) }
let accent = hex(0x3b82f6)

func draw(scale: CGFloat, to path: String) {
  let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(W * scale), pixelsHigh: Int(H * scale), bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
  rep.size = NSSize(width: W, height: H)
  NSGraphicsContext.saveGraphicsState()
  NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
  let ctx = NSGraphicsContext.current!.cgContext
  // Flip so y runs down, like Finder's icon positions.
  ctx.translateBy(x: 0, y: H); ctx.scaleBy(x: 1, y: -1)
  NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: true)

  // Background: the app's light grey, warming to white, with a soft accent glow.
  NSGradient(starting: hex(0xffffff), ending: hex(0xeef1f5))!.draw(in: NSRect(x: 0, y: 0, width: W, height: H), angle: 90)
  let glow = NSGradient(colors: [accent.withAlphaComponent(0.10), accent.withAlphaComponent(0)])!
  glow.draw(fromCenter: NSPoint(x: W / 2, y: iconY), radius: 0, toCenter: NSPoint(x: W / 2, y: iconY), radius: 300, options: [])

  // Title and subtitle.
  func text(_ s: String, size: CGFloat, weight: NSFont.Weight, color: NSColor, y: CGFloat) {
    let p = NSMutableParagraphStyle(); p.alignment = .center
    let attrs: [NSAttributedString.Key: Any] = [.font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color, .paragraphStyle: p]
    NSAttributedString(string: s, attributes: attrs).draw(in: NSRect(x: 0, y: y, width: W, height: size * 1.5))
  }
  text("KDownloader", size: 24, weight: .semibold, color: hex(0x1c1f23), y: 40)
  text("Drag the app to Applications to install it", size: 13, weight: .regular, color: hex(0x6b7280), y: 76)

  // Arrow from the app to Applications: a short line and a chevron, in the accent.
  let left = appX + 84, right = appsX - 84
  ctx.setStrokeColor(accent.cgColor); ctx.setLineCap(.round); ctx.setLineJoin(.round)
  ctx.setLineWidth(3)
  ctx.setLineDash(phase: 0, lengths: [0.1, 9])
  ctx.move(to: CGPoint(x: left, y: iconY)); ctx.addLine(to: CGPoint(x: right - 10, y: iconY)); ctx.strokePath()
  ctx.setLineDash(phase: 0, lengths: [])
  ctx.move(to: CGPoint(x: right - 12, y: iconY - 10)); ctx.addLine(to: CGPoint(x: right, y: iconY)); ctx.addLine(to: CGPoint(x: right - 12, y: iconY + 10)); ctx.strokePath()

  NSGraphicsContext.restoreGraphicsState()
  try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: path))
}
draw(scale: 1, to: CommandLine.arguments[1] + "/bg.png")
draw(scale: 2, to: CommandLine.arguments[1] + "/bg@2x.png")
