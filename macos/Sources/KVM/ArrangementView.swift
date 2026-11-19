import SwiftUI

/// Settings > Switching: this Mac's displays and the other computer's, which you drag to
/// the side it is on, like System Settings > Displays > Arrange. It snaps against the
/// nearest side when dropped.
struct ArrangementView: View {
    /// This Mac's displays, and the other computer's (empty, or without frames: unknown).
    let local: [DisplayInfo]
    let peer: [DisplayInfo]
    let peerName: String
    @Binding var side: Edge
    @Binding var offset: Double?
    var enabled = true

    /// How far the other computer's displays have been dragged, in view points.
    @State private var drag: CGSize?

    private struct Tile: Identifiable {
        let id: UInt32
        let name: String
        let rect: CGRect
        let main: Bool
    }

    var body: some View {
        GeometryReader { geo in
            let layout = Layout(local: localTiles, peer: peerTiles, side: side, offset: offset, size: geo.size)
            ZStack(alignment: .topLeading) {
                ForEach(localTiles) { tile($0, frame: layout.view(layout.localShown($0.rect)), mine: true) }
                ZStack(alignment: .topLeading) {
                    ForEach(peerTiles) { tile($0, frame: layout.view(layout.peerShown($0.rect)), mine: false) }
                }
                .offset(drag ?? .zero)
                .gesture(
                    DragGesture(minimumDistance: 2)
                        .onChanged { drag = $0.translation }
                        .onEnded { value in
                            let dropped = layout.placedPeer.offsetBy(dx: value.translation.width / layout.scale,
                                                                     dy: value.translation.height / layout.scale)
                            let snapped = Arrangement.snap(dropped, local: layout.localBox)
                            withAnimation(.easeOut(duration: 0.15)) {
                                side = snapped.side
                                offset = snapped.offset
                                drag = nil
                            }
                        }
                )
                .help("Drag to where \(peerName) is")
            }
        }
        .frame(height: 220)
        .background(RoundedRectangle(cornerRadius: 8).fill(Color(nsColor: .controlBackgroundColor)))
        .clipShape(RoundedRectangle(cornerRadius: 8))
        .opacity(enabled ? 1 : 0.45)
        .allowsHitTesting(enabled)
    }

    private var localTiles: [Tile] {
        local.compactMap { d in d.bounds.map { Tile(id: d.id, name: d.name, rect: $0, main: d.main) } }
    }

    /// Without its displays' frames, the other computer shows as one display the size of
    /// this Mac's main one.
    private var peerTiles: [Tile] {
        let known = peer.compactMap { d in d.bounds.map { Tile(id: d.id, name: d.name, rect: $0, main: d.main) } }
        if !known.isEmpty { return known }
        let size = localTiles.first(where: \.main)?.rect.size ?? CGSize(width: 1920, height: 1080)
        return [Tile(id: 0, name: peerName, rect: CGRect(origin: .zero, size: size), main: true)]
    }

    private func tile(_ t: Tile, frame: CGRect, mine: Bool) -> some View {
        let tint = mine ? Color.accentColor : Color.orange
        return ZStack(alignment: .top) {
            RoundedRectangle(cornerRadius: 3)
                .fill(tint.opacity(drag != nil && !mine ? 0.5 : 0.3))
            RoundedRectangle(cornerRadius: 3)
                .strokeBorder(tint, lineWidth: 1.5)
            if t.main {
                // The menu bar, as System Settings draws it on the main display.
                Rectangle().fill(Color.white.opacity(0.8)).frame(height: max(2, frame.height * 0.06))
                    .padding(.horizontal, 1.5).padding(.top, 1.5)
            }
            VStack(spacing: 1) {
                Text(t.name)
                    .font(.caption2.weight(.medium))
                if !mine, t.main, t.name != peerName { Text(peerName).font(.caption2).foregroundStyle(.secondary) }
            }
            .lineLimit(1)
            .minimumScaleFactor(0.5)
            .padding(4)
            .frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(width: frame.width, height: frame.height)
        .offset(x: frame.minX, y: frame.minY)
    }

    /// Both desktops scaled into the view and centered. The scale leaves room for the other
    /// computer's on any side of this Mac's, so it stays put while dragging.
    private struct Layout {
        let localBox: CGRect
        let peerBox: CGRect
        let placedPeer: CGRect
        let scale: CGFloat
        let origin: CGPoint

        init(local: [Tile], peer: [Tile], side: Edge, offset: Double?, size: CGSize) {
            let union = { (rects: [CGRect]) in rects.dropFirst().reduce(rects.first ?? .zero) { $0.union($1) } }
            localBox = union(local.map(\.rect))
            peerBox = union(peer.map(\.rect))
            placedPeer = Arrangement.placed(peer: peerBox, local: localBox, side: side, offset: offset)
            // Room for it beside or above this Mac, whichever is the tighter fit.
            let margin: CGFloat = 16
            let beside = CGSize(width: localBox.width + peerBox.width, height: max(localBox.height, peerBox.height) * 1.6)
            let above = CGSize(width: max(localBox.width, peerBox.width) * 1.6, height: localBox.height + peerBox.height)
            let fit = { (s: CGSize) in min((size.width - 2 * margin) / max(s.width, 1), (size.height - 2 * margin) / max(s.height, 1)) }
            scale = max(min(fit(beside), fit(above)), 0.001)
            let shown = localBox.union(placedPeer)
            origin = CGPoint(x: size.width / 2 - shown.midX * scale, y: size.height / 2 - shown.midY * scale)
        }

        func localShown(_ r: CGRect) -> CGRect { r }

        /// A peer display (its own coordinates) where it is placed next to this Mac.
        func peerShown(_ r: CGRect) -> CGRect {
            r.offsetBy(dx: placedPeer.minX - peerBox.minX, dy: placedPeer.minY - peerBox.minY)
        }

        /// This Mac's coordinates to view points, with a hairline gap between displays.
        func view(_ r: CGRect) -> CGRect {
            CGRect(x: origin.x + r.minX * scale, y: origin.y + r.minY * scale,
                   width: r.width * scale, height: r.height * scale).insetBy(dx: 1, dy: 1)
        }
    }
}
