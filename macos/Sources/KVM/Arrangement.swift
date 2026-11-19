import CoreGraphics

/// Where the peer's desktop sits next to this Mac's, for moving the pointer across: on
/// `side`, the start of its bounding box along that side `offset` points from the start of
/// this Mac's (top or left; nil: centered). Points map one to one across, so displays line
/// up by how big things look on them.
struct Arrangement {
    let side: Edge
    let offset: Double?
    let local: [(id: UInt32, rect: CGRect)]
    let peer: [(id: UInt32, rect: CGRect)]

    /// Where the pointer enters the peer after leaving this Mac at `p` (on its outer edge
    /// on `side`): a peer display and fractions 0...1 of it.
    func entry(leaving p: CGPoint) -> (display: UInt32, x: Double, y: Double)? {
        guard let (lBox, pBox) = boxes else { return nil }
        let shift = peerShift(lBox, pBox)
        // Peer displays in the shared space, as if the peer were on the right.
        let along = norm(p).y
        guard let target = facing(peer.map { (id: $0.id, rect: norm($0.rect).offsetBy(dx: shift.x, dy: shift.y)) },
                                  along: along, depth: { $0.minX - lBox.maxX }),
              let real = peer.first(where: { $0.id == target.id })?.rect else { return nil }
        let entered = CGPoint(x: target.rect.minX, y: min(max(along, target.rect.minY), target.rect.maxY - 1))
        let q = denorm(CGPoint(x: entered.x - shift.x, y: entered.y - shift.y))
        let fx = (q.x - real.minX) / max(real.width - 1, 1), fy = (q.y - real.minY) / max(real.height - 1, 1)
        return (target.id, min(max(fx, 0), 1), min(max(fy, 0), 1))
    }

    /// Where the pointer comes back on this Mac after the peer's pointer left `display` at `along`
    /// (0...65535) of its edge facing this Mac: a few points inside this Mac's edge.
    func returnPoint(display: UInt32, along: UInt16) -> CGPoint? {
        guard let (lBox, pBox) = boxes, let real = peer.first(where: { $0.id == display })?.rect else { return nil }
        let f = CGFloat(along) / 65535
        let exit = switch side.opposite {
        case .left: CGPoint(x: real.minX, y: real.minY + f * real.height)
        case .right: CGPoint(x: real.maxX - 1, y: real.minY + f * real.height)
        case .top: CGPoint(x: real.minX + f * real.width, y: real.minY)
        case .bottom: CGPoint(x: real.minX + f * real.width, y: real.maxY - 1)
        }
        let along = norm(exit).y + peerShift(lBox, pBox).y
        guard let target = facing(local.map { (id: $0.id, rect: norm($0.rect)) }, along: along,
                                  depth: { lBox.maxX - $0.maxX }) else { return nil }
        let inset: CGFloat = 3
        return denorm(CGPoint(x: target.rect.maxX - 1 - inset, y: min(max(along, target.rect.minY), target.rect.maxY - 1)))
    }

    // Everything below works as if the peer were on the right: `norm` turns this Mac's
    // and the peer's coordinates that way (mirrored and/or with x and y swapped), `denorm`
    // back. Rectangles go through their corners.

    private var boxes: (CGRect, CGRect)? {
        guard !local.isEmpty, !peer.isEmpty else { return nil }
        let union = { (rects: [CGRect]) in rects.dropFirst().reduce(rects[0]) { $0.union($1) } }
        return (union(local.map { norm($0.rect) }), union(peer.map { norm($0.rect) }))
    }

    /// What to add to a normalized peer point to put it in this Mac's normalized space.
    /// Normalizing never mirrors the axis along the side, so `offset` applies as it is.
    private func peerShift(_ lBox: CGRect, _ pBox: CGRect) -> CGPoint {
        CGPoint(x: lBox.maxX - pBox.minX, y: lBox.minY + (offset.map { CGFloat($0) } ?? (lBox.height - pBox.height) / 2) - pBox.minY)
    }

    // For drawing and dragging the arrangement: `local` and `peer` are the two desktops'
    // bounding boxes, each in its own coordinates.

    /// Where the peer's desktop goes in this Mac's coordinates.
    static func placed(peer: CGRect, local: CGRect, side: Edge, offset: Double?) -> CGRect {
        let horizontal = side == .left || side == .right
        let along = offset.map { CGFloat($0) } ?? (horizontal ? local.height - peer.height : local.width - peer.width) / 2
        let origin = switch side {
        case .left: CGPoint(x: local.minX - peer.width, y: local.minY + along)
        case .right: CGPoint(x: local.maxX, y: local.minY + along)
        case .top: CGPoint(x: local.minX + along, y: local.minY - peer.height)
        case .bottom: CGPoint(x: local.minX + along, y: local.maxY)
        }
        return CGRect(origin: origin, size: peer.size)
    }

    /// The side and offset nearest to the peer's desktop dropped at `dropped` (this Mac's
    /// coordinates): against the side its center is most beyond, overlapping this Mac's
    /// along it by at least a quarter of the shorter of the two.
    static func snap(_ dropped: CGRect, local: CGRect) -> (side: Edge, offset: Double) {
        let dx = (dropped.midX - local.midX) / (dropped.width + local.width)
        let dy = (dropped.midY - local.midY) / (dropped.height + local.height)
        let side: Edge = abs(dx) >= abs(dy) ? (dx < 0 ? .left : .right) : (dy < 0 ? .top : .bottom)
        let horizontal = side == .left || side == .right
        let (start, length, localLength) = horizontal ? (dropped.minY - local.minY, dropped.height, local.height)
                                                      : (dropped.minX - local.minX, dropped.width, local.width)
        let overlap = min(length, localLength) / 4
        return (side, min(max(start, overlap - length), localLength - overlap))
    }

    /// Of `rects`, the one whose edge facing the other computer is at `along`: nearest along
    /// it first (0 if it spans `along`), then nearest the boundary between the two desktops.
    private func facing(_ rects: [(id: UInt32, rect: CGRect)], along: CGFloat,
                        depth: (CGRect) -> CGFloat) -> (id: UInt32, rect: CGRect)? {
        func gap(_ r: CGRect) -> CGFloat { along < r.minY ? r.minY - along : along >= r.maxY ? along - r.maxY + 1 : 0 }
        return rects.min { (gap($0.rect), depth($0.rect)) < (gap($1.rect), depth($1.rect)) }
    }

    private func norm(_ p: CGPoint) -> CGPoint {
        switch side {
        case .right: p
        case .left: CGPoint(x: -p.x, y: p.y)
        case .bottom: CGPoint(x: p.y, y: p.x)
        case .top: CGPoint(x: -p.y, y: p.x)
        }
    }

    private func denorm(_ p: CGPoint) -> CGPoint {
        switch side {
        case .right: p
        case .left: CGPoint(x: -p.x, y: p.y)
        case .bottom: CGPoint(x: p.y, y: p.x)
        case .top: CGPoint(x: p.y, y: -p.x)
        }
    }

    private func norm(_ r: CGRect) -> CGRect {
        let a = norm(CGPoint(x: r.minX, y: r.minY)), b = norm(CGPoint(x: r.maxX, y: r.maxY))
        return CGRect(x: min(a.x, b.x), y: min(a.y, b.y), width: abs(b.x - a.x), height: abs(b.y - a.y))
    }
}
