import AppKit
import CGVirtualDisplay

/// A virtual display at the viewer's size or shape. Mirrored by the main display, the whole
/// desktop is laid out at the viewer's resolution or aspect ratio while it is viewed (the
/// physical screen shows a letterboxed copy); not mirrored, it is an extra screen to the
/// right of the others, like Sidecar. One per app, shared by the screen hosts: the last to
/// ask wins. Main thread only.
final class VirtualDisplay {
    static let shared = VirtualDisplay()

    private var display: CGVirtualDisplay?
    /// The physical main display, and whether it mirrors ours yet.
    private var physical: CGDirectDisplayID = 0
    /// Its pixel area, read before it mirrors ours: mirroring changes its reported mode
    /// (letterboxed to our shape), which made each re-fit smaller than the last.
    private var physicalArea: CGFloat = 0
    private var mirrored = false
    /// As an extra screen: whether it was put to the right of the others yet.
    private var placed = false
    /// Unmirrored and waiting to be removed once capture has moved off it.
    private(set) var released = false
    private var size = CGSize.zero
    private weak var owner: AnyObject?
    /// Mirrored (or an extra screen), as last asked; and a count that cancels pending retries.
    private var wantsMirror = true
    private var setupGeneration = 0

    private static let maxPixels: UInt32 = 7680

    var id: CGDirectDisplayID? { display?.displayID }

    /// Creates or resizes the display for `owner` to about `pixels` (`shapeOnly`: to their
    /// aspect ratio at the physical display's pixel count) and returns its id, or nil if it
    /// could not be created.
    /// `mirror`: the main display mirrors it; otherwise it is an extra screen.
    func fit(to pixels: CGSize, shapeOnly: Bool, mirror: Bool = true, owner: AnyObject) -> CGDirectDisplayID? {
        self.owner = owner
        released = false
        let wanted = shapeOnly ? shape(pixels.width / max(pixels.height, 1)) : VirtualDisplay.round(pixels)
        if display == nil {
            physical = CGMainDisplayID()
            physicalArea = VirtualDisplay.pixelArea(of: physical)
            guard let created = VirtualDisplay.create(like: physical) else {
                Log.warn("creating a virtual display failed")
                return nil
            }
            display = created
            size = .zero
            Log.info("virtual display \(created.displayID) created")
        }
        guard let display else { return nil }
        if wanted != size {
            guard display.apply(VirtualDisplay.settings(for: wanted)) else {
                Log.warn("virtual display: \(Int(wanted.width))x\(Int(wanted.height)) refused")
                return size == .zero ? nil : display.displayID
            }
            size = wanted
            Log.info("virtual display -> \(Int(wanted.width))x\(Int(wanted.height))")
        }
        wantsMirror = mirror
        if mirror {
            if !mirrored { self.mirror(onto: display.displayID, quiet: true) }
        } else {
            unmirror()
            if !placed { place(display.displayID) }
        }
        setupGeneration += 1
        if !isSetUp { retrySetup() }
        Displays.extended.withLock { $0 = mirror ? 0 : display.displayID }
        return display.displayID
    }

    private var isSetUp: Bool { wantsMirror ? mirrored : placed }

    /// A new display takes a moment to come online, and mirroring or placing it fails until
    /// then: try again shortly instead of waiting for the viewer's next request (which, from
    /// an iPad, only came with a rotation).
    private func retrySetup(attempt: Int = 1) {
        let generation = setupGeneration
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.25) { [self] in
            guard generation == setupGeneration, let display, !released, !isSetUp else { return }
            let last = attempt == 20
            if wantsMirror { mirror(onto: display.displayID, quiet: !last) } else { place(display.displayID) }
            if isSetUp { return }
            if last {
                Log.warn("virtual display \(display.displayID): still not \(wantsMirror ? "mirrored" : "placed") after 5 s")
            } else {
                retrySetup(attempt: attempt + 1)
            }
        }
    }

    /// The pixel size the display was set to (zero when there is none).
    var pixelSize: CGSize { display == nil ? .zero : size }

    /// The physical display's pixel area at `aspect`. Keeps the current size for about the
    /// same aspect, so a viewer asking for more pixels (zooming in) doesn't re-lay out.
    private func shape(_ aspect: CGFloat) -> CGSize {
        if size != .zero, abs(size.width / size.height / aspect - 1) < 0.02 { return size }
        let area = display == nil ? VirtualDisplay.pixelArea(of: CGMainDisplayID()) : physicalArea
        return VirtualDisplay.round(CGSize(width: (area * aspect).squareRoot(), height: (area / aspect).squareRoot()))
    }

    private static func pixelArea(of id: CGDirectDisplayID) -> CGFloat {
        let mode = CGDisplayCopyDisplayMode(id)
        return CGFloat((mode?.pixelWidth ?? 2880) * (mode?.pixelHeight ?? 1800))
    }

    /// For `owner`: unmirrors the display, so the physical one is the main display again,
    /// but keeps it until `remove` (while it is still being captured). Returns the physical
    /// display's id, or nil if there is nothing to release.
    func release(owner: AnyObject) -> CGDirectDisplayID? {
        guard display != nil, self.owner === owner || self.owner == nil else { return nil }
        unmirror()
        Displays.extended.withLock { $0 = 0 }
        setupGeneration += 1
        released = true
        return physical
    }

    /// Removes the display if `owner` made it; the main display goes back to how it was.
    func remove(owner: AnyObject) {
        guard display != nil, self.owner === owner || self.owner == nil else { return }
        unmirror()
        Displays.extended.withLock { $0 = 0 }
        released = false
        placed = false
        setupGeneration += 1
        display = nil // releasing it unplugs it
        size = .zero
        self.owner = nil
        Log.info("virtual display removed")
    }

    private func unmirror() {
        guard mirrored else { return }
        var config: CGDisplayConfigRef?
        if CGBeginDisplayConfiguration(&config) == .success, let config {
            CGConfigureDisplayMirrorOfDisplay(config, physical, kCGNullDirectDisplay)
            CGCompleteDisplayConfiguration(config, .forAppOnly)
        }
        mirrored = false
    }

    /// Puts the extra screen to the right of the others, top-aligned with the main one.
    /// Fails (quietly) while the new display is not online yet.
    private func place(_ id: CGDirectDisplayID) {
        var count: UInt32 = 0
        CGGetActiveDisplayList(0, nil, &count)
        var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
        CGGetActiveDisplayList(count, &ids, &count)
        guard ids.contains(id) else { return }
        let right = ids.filter { $0 != id }.map { CGDisplayBounds($0).maxX }.max() ?? 0
        var config: CGDisplayConfigRef?
        guard CGBeginDisplayConfiguration(&config) == .success, let config else { return }
        CGConfigureDisplayOrigin(config, id, Int32(right), Int32(CGDisplayBounds(physical).minY))
        guard CGCompleteDisplayConfiguration(config, .forAppOnly) == .success else { return }
        placed = true
        Log.info("virtual display \(id) is an extra screen")
    }

    /// Makes the physical main display mirror ours. `.forAppOnly`: undone if the app quits
    /// or crashes. `quiet`: failing is expected (the new display may not be online yet).
    private func mirror(onto id: CGDirectDisplayID, quiet: Bool = false) {
        var config: CGDisplayConfigRef?
        guard CGBeginDisplayConfiguration(&config) == .success, let config else { return }
        var result = CGConfigureDisplayMirrorOfDisplay(config, physical, id)
        guard result == .success else {
            CGCancelDisplayConfiguration(config)
            if !quiet { Log.warn("mirroring display \(physical) onto the virtual display failed (\(result.rawValue))") }
            return
        }
        result = CGCompleteDisplayConfiguration(config, .forAppOnly)
        guard result == .success else {
            if !quiet { Log.warn("mirroring display \(physical) onto the virtual display: completing failed (\(result.rawValue))") }
            return
        }
        mirrored = true
        Log.info("display \(physical) mirrors the virtual display")
    }

    private static func create(like physical: CGDirectDisplayID) -> CGVirtualDisplay? {
        let descriptor = CGVirtualDisplayDescriptor()
        descriptor.queue = .main
        descriptor.name = "KVM"
        descriptor.maxPixelsWide = maxPixels
        descriptor.maxPixelsHigh = maxPixels
        // Same physical size as the real screen, so macOS picks similar UI defaults.
        let mm = CGDisplayScreenSize(physical)
        descriptor.sizeInMillimeters = mm.width > 0 ? mm : CGSize(width: 600, height: 340)
        descriptor.vendorID = 0x4B56 // "KV"
        descriptor.productID = 0x4D01
        descriptor.serialNum = 1
        descriptor.terminationHandler = { _, _ in Log.warn("virtual display terminated") }
        return CGVirtualDisplay(descriptor: descriptor)
    }

    /// One mode of `pixels`. HiDPI when that still leaves a usable desktop, so text is drawn
    /// at the viewer's scale rather than tiny.
    private static func settings(for pixels: CGSize) -> CGVirtualDisplaySettings {
        let hiDPI = min(pixels.width, pixels.height) >= 1080
        let scale = hiDPI ? 2.0 : 1.0
        let settings = CGVirtualDisplaySettings()
        settings.hiDPI = hiDPI ? 1 : 0
        settings.modes = [CGVirtualDisplayMode(width: UInt(pixels.width / scale), height: UInt(pixels.height / scale),
                                               refreshRate: 60)]
        return settings
    }

    /// Multiples of 4 (even in pixels and in HiDPI points), within what the display accepts.
    private static func round(_ pixels: CGSize) -> CGSize {
        func clamp(_ v: CGFloat, _ low: CGFloat) -> CGFloat {
            CGFloat(Int(min(max(v, low), CGFloat(maxPixels))) & ~3)
        }
        return CGSize(width: clamp(pixels.width, 640), height: clamp(pixels.height, 480))
    }
}
