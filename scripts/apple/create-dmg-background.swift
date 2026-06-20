import AppKit
import Foundation

guard CommandLine.arguments.count >= 3 else {
  fputs("Usage: create-dmg-background.swift <output.png> <logo.png>\n", stderr)
  exit(1)
}

let outputURL = URL(fileURLWithPath: CommandLine.arguments[1])
let logoURL = URL(fileURLWithPath: CommandLine.arguments[2])
let canvasSize = NSSize(width: 760, height: 440)
let image = NSImage(size: canvasSize)

func drawText(_ text: String, x: CGFloat, y: CGFloat, width: CGFloat, size: CGFloat, color: NSColor, weight: NSFont.Weight, alignment: NSTextAlignment = .center) {
  let paragraph = NSMutableParagraphStyle()
  paragraph.alignment = alignment
  let attrs: [NSAttributedString.Key: Any] = [
    .font: NSFont.systemFont(ofSize: size, weight: weight),
    .foregroundColor: color,
    .paragraphStyle: paragraph,
    .kern: 1.1
  ]
  let rect = NSRect(x: x, y: y, width: width, height: size + 16)
  text.draw(in: rect, withAttributes: attrs)
}

image.lockFocus()

let rect = NSRect(origin: .zero, size: canvasSize)
NSColor(calibratedRed: 0.020, green: 0.024, blue: 0.035, alpha: 1).setFill()
rect.fill()

if let gradient = NSGradient(colors: [
  NSColor(calibratedRed: 0.035, green: 0.070, blue: 0.105, alpha: 1),
  NSColor(calibratedRed: 0.010, green: 0.014, blue: 0.025, alpha: 1)
]) {
  gradient.draw(in: rect, angle: -30)
}

NSGraphicsContext.current?.shouldAntialias = true

var seed: UInt64 = 0x5A57_4D47
func nextUnit() -> CGFloat {
  seed = seed &* 6364136223846793005 &+ 1442695040888963407
  return CGFloat((seed >> 32) & 0xffff) / CGFloat(0xffff)
}

for _ in 0..<140 {
  let x = nextUnit() * canvasSize.width
  let y = nextUnit() * canvasSize.height
  let radius = CGFloat(0.55 + nextUnit() * 1.35)
  let alpha = CGFloat(0.10 + nextUnit() * 0.40)
  NSColor(calibratedRed: 0.65, green: 0.84, blue: 1.0, alpha: alpha).setFill()
  NSBezierPath(ovalIn: NSRect(x: x, y: y, width: radius, height: radius)).fill()
}

let guideStart = NSPoint(x: 298, y: 214)
let guideEnd = NSPoint(x: 462, y: 214)

for (width, alpha) in [(12.0, 0.06), (6.0, 0.12), (3.0, 0.72)] {
  let guide = NSBezierPath()
  guide.move(to: guideStart)
  guide.line(to: guideEnd)
  guide.lineWidth = CGFloat(width)
  guide.lineCapStyle = .round
  NSColor(calibratedRed: 0.25, green: 0.76, blue: 1.0, alpha: CGFloat(alpha)).setStroke()
  guide.stroke()
}

let arrowHead = NSBezierPath()
arrowHead.move(to: guideEnd)
arrowHead.line(to: NSPoint(x: guideEnd.x - 22, y: guideEnd.y + 15))
arrowHead.move(to: guideEnd)
arrowHead.line(to: NSPoint(x: guideEnd.x - 22, y: guideEnd.y - 15))
arrowHead.lineWidth = 3.6
arrowHead.lineCapStyle = .round
arrowHead.lineJoinStyle = .round
NSColor(calibratedRed: 0.32, green: 0.82, blue: 1.0, alpha: 0.82).setStroke()
arrowHead.stroke()

let pulse = NSBezierPath(ovalIn: NSRect(x: guideEnd.x - 4, y: guideEnd.y - 4, width: 8, height: 8))
NSColor(calibratedRed: 0.42, green: 0.85, blue: 1.0, alpha: 0.85).setFill()
pulse.fill()

if let logo = NSImage(contentsOf: logoURL) {
  logo.draw(in: NSRect(x: 326, y: 292, width: 108, height: 108), from: .zero, operation: .sourceOver, fraction: 0.94)
}

drawText("ZENITH ASTRO STACKER", x: 190, y: 264, width: 380, size: 21, color: NSColor(calibratedWhite: 1, alpha: 0.92), weight: .heavy)
drawText("ARRASTRA PARA INSTALAR", x: 220, y: 54, width: 320, size: 12, color: NSColor(calibratedRed: 0.56, green: 0.78, blue: 0.95, alpha: 0.72), weight: .bold)

let labelAttrs: [NSAttributedString.Key: Any] = [
  .font: NSFont.systemFont(ofSize: 13, weight: .semibold),
  .foregroundColor: NSColor(calibratedWhite: 1, alpha: 0.62)
]
"Zenith Astro Stacker".draw(at: NSPoint(x: 104, y: 84), withAttributes: labelAttrs)
"Applications".draw(at: NSPoint(x: 541, y: 84), withAttributes: labelAttrs)

let bitmap = NSBitmapImageRep(focusedViewRect: rect)
image.unlockFocus()

guard let pngData = bitmap?.representation(using: .png, properties: [:]) else {
  fputs("Could not render DMG background.\n", stderr)
  exit(1)
}

try pngData.write(to: outputURL)
