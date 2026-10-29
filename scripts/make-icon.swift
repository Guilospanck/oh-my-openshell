// Draws the oh-my-openshell icon (charcoal squircle, mint shell prompt) to <out.png>, 1024x1024.
// Usage: swift scripts/make-icon.swift <out.png>
import AppKit

let px = 1024
let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: px, pixelsHigh: px, bitsPerSample: 8,
                           samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

// Squircle with the macOS icon margin.
ctx.saveGState()
ctx.addPath(CGPath(roundedRect: CGRect(x: 100, y: 100, width: 824, height: 824), cornerWidth: 185, cornerHeight: 185, transform: nil))
ctx.clip()
let grad = CGGradient(colorsSpace: CGColorSpaceCreateDeviceRGB(), colors: [
    CGColor(red: 0.20, green: 0.24, blue: 0.30, alpha: 1), CGColor(red: 0.07, green: 0.09, blue: 0.12, alpha: 1)
] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(grad, start: CGPoint(x: 512, y: 924), end: CGPoint(x: 512, y: 100), options: [])
ctx.restoreGState()

// Prompt: chevron and cursor bar.
let mint = CGColor(red: 0.29, green: 0.87, blue: 0.50, alpha: 1)
ctx.setStrokeColor(mint)
ctx.setLineWidth(76); ctx.setLineCap(.round); ctx.setLineJoin(.round)
ctx.move(to: CGPoint(x: 300, y: 650)); ctx.addLine(to: CGPoint(x: 490, y: 512)); ctx.addLine(to: CGPoint(x: 300, y: 374))
ctx.strokePath()
ctx.move(to: CGPoint(x: 560, y: 360)); ctx.addLine(to: CGPoint(x: 740, y: 360))
ctx.strokePath()

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
