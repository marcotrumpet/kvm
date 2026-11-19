import AppKit
import CGVirtualDisplay

/// A virtual display at the viewer's size or shape that the main display mirrors, so the
/// whole desktop is laid out at the viewer's resolution or aspect ratio while it is viewed.
/// The physical screen shows a letterboxed copy. One per app, shared by the screen hosts:
/// the last to ask wins. Main thread only.
final class VirtualDisplay {
    static let shared = VirtualDisplay()

    private var display: CGVirtualDisplay?
    /// The physical main display, and whether it mirrors ours yet.
    private var physical: CGDirectDisplayID = 0
    private var mirrored = false
    /// Unmirrored and waiting to be removed once capture has moved off it.
    private(set) var released = false
    private var size = CGSize.zero
    private weak var owner: AnyObject?

    private static let maxPixels: UInt32 = 7680

    var id: CGDirectDisplayID? { display?.displayID }

    /// Creates or resizes the display for `owner` to about `pixels` (`shapeOnly`: to their
    /// aspect ratio at the physical display's pixel count) and returns its id, or nil if it
    /// could not be created.
    func fit(to pixels: CGSize, shapeOnly: Bool, owner: AnyObject) -> CGDirectDisplayID? {
        self.owner = owner
        released = false
        let wanted = shapeOnly ? shape(pixels.width / max(pixels.height, 1)) : VirtualDisplay.round(pixels)
        if display == nil {
            physical = CGMainDisplayID()
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
        if !mirrored { mirror(onto: display.displayID) }
        return display.displayID
    }

    /// The pixel size the display was set to (zero when there is none).
    var pixelSize: CGSize { display == nil ? .zero : size }

    /// The physical display's pixel area at `aspect`. Keeps the current size for about the
    /// same aspect, so a viewer asking for more pixels (zooming in) doesn't re-lay out.
    private func shape(_ aspect: CGFloat) -> CGSize {
        if size != .zero, abs(size.width / size.height / aspect - 1) < 0.02 { return size }
        let main = display == nil ? CGMainDisplayID() : physical
        let mode = CGDisplayCopyDisplayMode(main)
        let area = CGFloat((mode?.pixelWidth ?? 2880) * (mode?.pixelHeight ?? 1800))
        return VirtualDisplay.round(CGSize(width: (area * aspect).squareRoot(), height: (area / aspect).squareRoot()))
    }

    /// For `owner`: unmirrors the display, so the physical one is the main display again,
    /// but keeps it until `remove` (while it is still being captured). Returns the physical
    /// display's id, or nil if there is nothing to release.
    func release(owner: AnyObject) -> CGDirectDisplayID? {
        guard display != nil, self.owner === owner || self.owner == nil else { return nil }
        unmirror()
        released = true
        return physical
    }

    /// Removes the display if `owner` made it; the main display goes back to how it was.
    func remove(owner: AnyObject) {
        guard display != nil, self.owner === owner || self.owner == nil else { return }
        unmirror()
        released = false
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

    /// Makes the physical main display mirror ours. `.forAppOnly`: undone if the app quits
    /// or crashes. Retried on the next `fit` if the new display is not online yet.
    private func mirror(onto id: CGDirectDisplayID) {
        var config: CGDisplayConfigRef?
        guard CGBeginDisplayConfiguration(&config) == .success, let config else { return }
        var result = CGConfigureDisplayMirrorOfDisplay(config, physical, id)
        guard result == .success else {
            CGCancelDisplayConfiguration(config)
            Log.warn("mirroring display \(physical) onto the virtual display failed (\(result.rawValue))")
            return
        }
        result = CGCompleteDisplayConfiguration(config, .forAppOnly)
        guard result == .success else {
            Log.warn("mirroring display \(physical) onto the virtual display: completing failed (\(result.rawValue))")
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
