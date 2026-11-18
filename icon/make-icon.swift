// Draws the KVM app icon, in one of several styles.
//   swift make-icon.swift <style 1-6> <ios.png> <mac.png>     (the apps use style 6)
//   swift make-icon.swift sheet <sheet.png>      all styles side by side, to choose from
// The iOS PNG goes to ios/KVM/Assets.xcassets/AppIcon.appiconset/AppIcon.png; the macOS one
// becomes macos/Resources/AppIcon.icns (sips each iconset size, then iconutil -c icns).
// The iOS one is full-bleed (iOS rounds the corners); the macOS one has the standard
// rounded square with margin and shadow.
import AppKit

let space = CGColorSpaceCreateDeviceRGB()

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: CGFloat(hex >> 16 & 0xFF) / 255, green: CGFloat(hex >> 8 & 0xFF) / 255, blue: CGFloat(hex & 0xFF) / 255, alpha: a)
}

func gradient(_ ctx: CGContext, _ colors: [CGColor], from: CGPoint, to: CGPoint) {
    let g = CGGradient(colorsSpace: space, colors: colors as CFArray, locations: nil)!
    ctx.drawLinearGradient(g, start: from, end: to, options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
}

func rounded(_ r: CGRect, _ radius: CGFloat) -> CGPath {
    CGPath(roundedRect: r, cornerWidth: radius, cornerHeight: radius, transform: nil)
}

/// The classic arrow pointer, tip at `tip`, `size` tall.
func pointer(tip: CGPoint, size: CGFloat) -> CGPath {
    let u = size / 75
    let pts: [(CGFloat, CGFloat)] = [(0, 0), (0, -64), (15, -50), (26, -75), (37, -70), (26, -46), (46, -46)]
    let p = CGMutablePath()
    p.move(to: tip)
    for (x, y) in pts.dropFirst() { p.addLine(to: CGPoint(x: tip.x + x * u, y: tip.y + y * u)) }
    p.closeSubpath()
    return p
}

func drawPointer(_ ctx: CGContext, tip: CGPoint, size: CGFloat, fill: CGColor, stroke: CGColor, s: CGFloat) {
    let p = pointer(tip: tip, size: size)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.006), blur: s * 0.02, color: CGColor(gray: 0, alpha: 0.4))
    ctx.setFillColor(fill)
    ctx.addPath(p)
    ctx.fillPath()
    ctx.restoreGState()
    ctx.setStrokeColor(stroke)
    ctx.setLineWidth(size * 0.04)
    ctx.setLineJoin(.round)
    ctx.addPath(p)
    ctx.strokePath()
}

/// A monitor: bezel, glass, stand. `outline`: strokes only.
func monitor(_ ctx: CGContext, _ r: CGRect, color: CGColor, glass: [CGColor]?, outline: CGFloat = 0, stand: Bool = true) {
    let radius = r.width * 0.07
    let neck = CGRect(x: r.midX - r.width * 0.07, y: r.minY - r.height * 0.16, width: r.width * 0.14, height: r.height * 0.17)
    let foot = CGRect(x: r.midX - r.width * 0.2, y: neck.minY - r.height * 0.05, width: r.width * 0.4, height: r.height * 0.06)
    if outline > 0 {
        ctx.setStrokeColor(color)
        ctx.setLineWidth(outline)
        ctx.setLineJoin(.round)
        ctx.setLineCap(.round)
        ctx.addPath(rounded(r, radius))
        if !stand {
            ctx.strokePath()
            return
        }
        ctx.move(to: CGPoint(x: r.midX, y: r.minY))
        ctx.addLine(to: CGPoint(x: r.midX, y: foot.midY))
        ctx.move(to: CGPoint(x: foot.minX, y: foot.midY))
        ctx.addLine(to: CGPoint(x: foot.maxX, y: foot.midY))
        ctx.strokePath()
        return
    }
    ctx.setFillColor(color)
    ctx.addPath(rounded(r, radius))
    ctx.fillPath()
    ctx.fill(neck)
    ctx.addPath(rounded(foot, foot.height / 2))
    ctx.fillPath()
    if let glass {
        let g = r.insetBy(dx: r.width * 0.055, dy: r.width * 0.055)
        ctx.saveGState()
        ctx.addPath(rounded(g, radius / 2))
        ctx.clip()
        gradient(ctx, glass, from: CGPoint(x: g.minX, y: g.maxY), to: CGPoint(x: g.maxX, y: g.minY))
        ctx.restoreGState()
    }
}

let styles: [(name: String, draw: (CGContext, CGFloat) -> Void)] = [
    ("Two screens", { ctx, s in
        gradient(ctx, [rgb(0x216BFA), rgb(0x40299E)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        let glass = [rgb(0x294DCC), rgb(0x1A246B)]
        monitor(ctx, CGRect(x: s * 0.10, y: s * 0.46, width: s * 0.50, height: s * 0.33), color: CGColor(gray: 1, alpha: 0.55), glass: glass)
        monitor(ctx, CGRect(x: s * 0.38, y: s * 0.28, width: s * 0.52, height: s * 0.34), color: CGColor(gray: 1, alpha: 1), glass: glass)
        drawPointer(ctx, tip: CGPoint(x: s * 0.47, y: s * 0.62), size: s * 0.315, fill: rgb(0x141414), stroke: .white, s: s)
    }),
    ("Mac and iPad", { ctx, s in
        gradient(ctx, [rgb(0x12C2B8), rgb(0x1E5BD8)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        // Laptop
        let screen = CGRect(x: s * 0.14, y: s * 0.40, width: s * 0.56, height: s * 0.36)
        ctx.setFillColor(.white)
        ctx.addPath(rounded(screen, s * 0.03))
        ctx.fillPath()
        let glass = screen.insetBy(dx: s * 0.025, dy: s * 0.025)
        ctx.saveGState()
        ctx.addPath(rounded(glass, s * 0.012))
        ctx.clip()
        gradient(ctx, [rgb(0x1F3B8F), rgb(0x0E1A45)], from: CGPoint(x: glass.minX, y: glass.maxY), to: CGPoint(x: glass.maxX, y: glass.minY))
        ctx.restoreGState()
        let base = CGMutablePath()
        base.move(to: CGPoint(x: s * 0.08, y: s * 0.36))
        base.addLine(to: CGPoint(x: s * 0.76, y: s * 0.36))
        base.addLine(to: CGPoint(x: s * 0.72, y: s * 0.40))
        base.addLine(to: CGPoint(x: s * 0.12, y: s * 0.40))
        base.closeSubpath()
        ctx.addPath(base)
        ctx.fillPath()
        // iPad, in front
        let pad = CGRect(x: s * 0.58, y: s * 0.18, width: s * 0.28, height: s * 0.38)
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.01), blur: s * 0.03, color: CGColor(gray: 0, alpha: 0.35))
        ctx.setFillColor(rgb(0x20242C))
        ctx.addPath(rounded(pad, s * 0.035))
        ctx.fillPath()
        ctx.restoreGState()
        let padGlass = pad.insetBy(dx: s * 0.018, dy: s * 0.018)
        ctx.saveGState()
        ctx.addPath(rounded(padGlass, s * 0.02))
        ctx.clip()
        gradient(ctx, [rgb(0x2A8CF0), rgb(0x1B3FA8)], from: CGPoint(x: padGlass.minX, y: padGlass.maxY), to: CGPoint(x: padGlass.maxX, y: padGlass.minY))
        ctx.restoreGState()
        // A finger touch on the iPad, the pointer on the Mac
        ctx.setFillColor(CGColor(gray: 1, alpha: 0.85))
        ctx.fillEllipse(in: CGRect(x: pad.midX - s * 0.035, y: pad.midY - s * 0.035, width: s * 0.07, height: s * 0.07))
        drawPointer(ctx, tip: CGPoint(x: s * 0.34, y: s * 0.66), size: s * 0.2, fill: .white, stroke: rgb(0x0E1A45), s: s)
    }),
    ("Pointer jump", { ctx, s in
        gradient(ctx, [rgb(0x2E3340), rgb(0x0D0F14)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        // Dashed arc from a dot (where the pointer was) to the pointer.
        ctx.setStrokeColor(rgb(0x3FE0FF))
        ctx.setLineWidth(s * 0.03)
        ctx.setLineCap(.round)
        ctx.setLineDash(phase: 0, lengths: [s * 0.001, s * 0.06])
        ctx.move(to: CGPoint(x: s * 0.22, y: s * 0.26))
        ctx.addCurve(to: CGPoint(x: s * 0.52, y: s * 0.58), control1: CGPoint(x: s * 0.20, y: s * 0.62), control2: CGPoint(x: s * 0.40, y: s * 0.72))
        ctx.strokePath()
        ctx.setLineDash(phase: 0, lengths: [])
        ctx.setFillColor(rgb(0x3FE0FF))
        ctx.fillEllipse(in: CGRect(x: s * 0.17, y: s * 0.21, width: s * 0.1, height: s * 0.1))
        drawPointer(ctx, tip: CGPoint(x: s * 0.52, y: s * 0.70), size: s * 0.42, fill: .white, stroke: rgb(0x0D0F14), s: s)
    }),
    ("Keycap", { ctx, s in
        gradient(ctx, [rgb(0x2BD67B), rgb(0x0F8A7E)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        let side = CGRect(x: s * 0.2, y: s * 0.17, width: s * 0.6, height: s * 0.6)
        ctx.saveGState()
        ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.025), blur: s * 0.05, color: CGColor(gray: 0, alpha: 0.35))
        ctx.setFillColor(rgb(0xD5DBD8))
        ctx.addPath(rounded(side, s * 0.11))
        ctx.fillPath()
        ctx.restoreGState()
        let top = CGRect(x: s * 0.25, y: s * 0.25, width: s * 0.5, height: s * 0.5)
        ctx.saveGState()
        ctx.addPath(rounded(top, s * 0.08))
        ctx.clip()
        gradient(ctx, [.white, rgb(0xEEF2F0)], from: CGPoint(x: 0, y: top.maxY), to: CGPoint(x: 0, y: top.minY))
        ctx.restoreGState()
        drawPointer(ctx, tip: CGPoint(x: s * 0.43, y: s * 0.66), size: s * 0.3, fill: rgb(0x0F6E64), stroke: .white, s: s)
    }),
    ("Split", { ctx, s in
        // Two machines: the square split on the diagonal, the pointer crossing it.
        gradient(ctx, [rgb(0x5B8CFF), rgb(0x2F5BE6)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        let half = CGMutablePath()
        half.move(to: CGPoint(x: s, y: s))
        half.addLine(to: CGPoint(x: s, y: 0))
        half.addLine(to: CGPoint(x: 0, y: 0))
        half.closeSubpath()
        ctx.saveGState()
        ctx.addPath(half)
        ctx.clip()
        gradient(ctx, [rgb(0x8A3FFC), rgb(0x4B1FA8)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        ctx.restoreGState()
        let w = s * 0.03
        monitor(ctx, CGRect(x: s * 0.12, y: s * 0.58, width: s * 0.3, height: s * 0.2), color: CGColor(gray: 1, alpha: 0.9), glass: nil, outline: w)
        monitor(ctx, CGRect(x: s * 0.58, y: s * 0.30, width: s * 0.3, height: s * 0.2), color: CGColor(gray: 1, alpha: 0.9), glass: nil, outline: w)
        drawPointer(ctx, tip: CGPoint(x: s * 0.43, y: s * 0.62), size: s * 0.34, fill: .white, stroke: rgb(0x2A1670), s: s)
    }),
    ("Sunset outline", { ctx, s in
        gradient(ctx, [rgb(0xFFB547), rgb(0xFF4F7B), rgb(0xB83BD6)], from: CGPoint(x: 0, y: s), to: CGPoint(x: s, y: 0))
        let w = s * 0.035
        let front = CGRect(x: s * 0.40, y: s * 0.30, width: s * 0.48, height: s * 0.31)
        // The back screen stops where the front one covers it.
        ctx.saveGState()
        let clip = CGMutablePath()
        clip.addRect(CGRect(x: 0, y: 0, width: s, height: s))
        clip.addPath(rounded(front.insetBy(dx: -w * 1.5, dy: -w * 1.5), front.width * 0.09))
        ctx.addPath(clip)
        ctx.clip(using: .evenOdd)
        monitor(ctx, CGRect(x: s * 0.12, y: s * 0.46, width: s * 0.46, height: s * 0.30), color: CGColor(gray: 1, alpha: 0.6), glass: nil, outline: w, stand: false)
        ctx.restoreGState()
        monitor(ctx, front, color: .white, glass: nil, outline: w)
    }),
]

func context(_ size: Int) -> CGContext {
    CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0, space: space,
              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
}

/// macOS grid: an 824/1024 rounded square, centred, with a soft shadow.
func drawMac(_ ctx: CGContext, size s: CGFloat, style: Int) {
    let inset = s * 100 / 1024
    let rect = CGRect(x: inset, y: inset + s * 0.01, width: s - 2 * inset, height: s - 2 * inset)
    let shape = rounded(rect, rect.width * 0.225)
    ctx.saveGState()
    ctx.setShadow(offset: CGSize(width: 0, height: -s * 0.01), blur: s * 0.03, color: CGColor(gray: 0, alpha: 0.35))
    ctx.addPath(shape)
    ctx.setFillColor(CGColor(gray: 0, alpha: 1))
    ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(shape)
    ctx.clip()
    ctx.translateBy(x: rect.minX, y: rect.minY)
    styles[style].draw(ctx, rect.width)
    ctx.restoreGState()
}

func png(_ ctx: CGContext, opaque: Bool = false) -> Data {
    let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
    if opaque { rep.hasAlpha = false }
    return rep.representation(using: .png, properties: [:])!
}

let args = CommandLine.arguments
if args[1] == "sheet" {
    let cell = 420, cols = 3, label = 56
    let w = cell * cols, h = (cell + label) * 2
    let ctx = CGContext(data: nil, width: w, height: h, bitsPerComponent: 8, bytesPerRow: 0, space: space,
                        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    ctx.setFillColor(rgb(0xF2F2F5))
    ctx.fill(CGRect(x: 0, y: 0, width: w, height: h))
    for (i, style) in styles.enumerated() {
        let x = (i % cols) * cell, y = h - (i / cols + 1) * (cell + label) + label
        ctx.saveGState()
        ctx.translateBy(x: CGFloat(x), y: CGFloat(y))
        drawMac(ctx, size: CGFloat(cell), style: i)
        ctx.restoreGState()
        let text = NSAttributedString(string: "\(i + 1)  \(style.name)", attributes: [
            .font: NSFont.systemFont(ofSize: 26, weight: .semibold), .foregroundColor: NSColor.black,
        ])
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: ctx, flipped: false)
        let size = text.size()
        text.draw(at: CGPoint(x: CGFloat(x) + (CGFloat(cell) - size.width) / 2, y: CGFloat(y) - CGFloat(label) + 14))
        NSGraphicsContext.restoreGraphicsState()
    }
    try png(ctx).write(to: URL(fileURLWithPath: args[2]))
} else {
    let style = (Int(args[1]) ?? 1) - 1
    let ios = context(1024)
    styles[style].draw(ios, 1024)
    try png(ios, opaque: true).write(to: URL(fileURLWithPath: args[2]))
    let mac = context(1024)
    drawMac(mac, size: 1024, style: style)
    try png(mac).write(to: URL(fileURLWithPath: args[3]))
}
