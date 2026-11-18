import AppKit
import AVFoundation
import CoreMedia

/// Window showing the peer's screen. Decoding and display happen on the GPU through
/// AVSampleBufferDisplayLayer. Clicking into it takes control of the peer (the hotkey
/// gives it back), with the peer's pointer jumping to the clicked point.
final class ScreenViewer: NSObject, NSWindowDelegate {
    private var window: NSWindow?
    private let view = VideoView()
    private var format: CMVideoFormatDescription?
    private var sized = false
    /// Main thread.
    var onClose: (() -> Void)?
    /// Main thread; point on the peer's screen as fractions (0...1, top-left origin).
    var onClick: ((CGPoint) -> Void)?
    /// Main thread; decoding failed and the stream should be restarted.
    var onDecodeFailure: (() -> Void)?
    /// Main thread; files dropped onto the window, to send to the peer.
    var onDrop: (([URL]) -> Void)?
    /// Main thread; quality or (when matching it) window size changed: ask for a new stream.
    var onStreamSettingsChanged: (() -> Void)?

    private let qualityMenu = NSPopUpButton(frame: .zero, pullsDown: false)
    private let displayMenu = NSPopUpButton(frame: .zero, pullsDown: false)
    private var displays: [DisplayInfo] = []
    /// The peer's display being viewed (0: its main display).
    private(set) var selectedDisplay: UInt32 = 0
    private let matchWindow = NSButton(checkboxWithTitle: "Match window size", target: nil, action: nil)
    private let hoverControl = NSButton(checkboxWithTitle: "Control on hover", target: nil, action: nil)
    private lazy var fullScreen = NSButton(
        image: NSImage(systemSymbolName: "arrow.up.left.and.arrow.down.right", accessibilityDescription: "Full Screen")!,
        target: self, action: #selector(toggleFullScreen))
    /// Main thread; the picture moved or the hover setting changed.
    var onPictureMoved: (() -> Void)?
    private let stats = NSTextField(labelWithString: "")
    private var statsTimer: Timer?
    private var framesThisSecond = 0
    private var bytesThisSecond = 0
    private var resizeDebounce: DispatchWorkItem?
    /// The pixel size last reported via `onStreamSettingsChanged`.
    private var requestedSize: CGSize?

    var isOpen: Bool { window != nil }

    /// The video area's size in pixels: what the stream should match.
    var videoPixelSize: CGSize? {
        guard let window else { return nil }
        let scale = window.backingScaleFactor
        let size = CGSize(width: view.bounds.width * scale, height: view.bounds.height * scale)
        requestedSize = size
        return size
    }

    /// Main thread.
    func show(title: String) {
        if window == nil {
            let window = NSWindow(
                contentRect: NSRect(x: 0, y: 0, width: 960, height: 600),
                styleMask: [.titled, .closable, .resizable, .miniaturizable],
                backing: .buffered, defer: false)
            window.contentView = view
            window.delegate = self
            window.isReleasedWhenClosed = false
            window.backgroundColor = .black
            window.collectionBehavior.insert(.fullScreenPrimary)
            window.center()
            view.onClick = { [weak self] point in self?.onClick?(point) }
            view.onDrop = { [weak self] urls in self?.onDrop?(urls) }
            window.addTitlebarAccessoryViewController(makeControls())
            self.window = window
            statsTimer = Timer.scheduledTimer(withTimeInterval: 1, repeats: true) { [weak self] _ in self?.updateStats() }
            sized = false
            format = nil
            view.status = "Waiting for the screen…"
        }
        window?.title = title
        NSApp.activate(ignoringOtherApps: true)
        window?.makeKeyAndOrderFront(nil)
    }

    func close() {
        window?.close()
    }

    func setStatus(_ text: String?) {
        view.status = text
    }

    func windowWillClose(_ notification: Notification) {
        window = nil
        onPictureMoved?()
        statsTimer?.invalidate()
        statsTimer = nil
        selectedDisplay = 0
        displays = []
        resizeDebounce?.cancel()
        requestedSize = nil
        view.displayLayer.flushAndRemoveImage()
        onClose?()
    }

    /// Any thread: one encoded frame in ScreenHost's payload format.
    func handle(frame: Data, keyframe: Bool) {
        DispatchQueue.main.async { [self] in
            guard window != nil else { return }
            enqueue(frame, keyframe: keyframe)
        }
    }

    private func makeControls() -> NSTitlebarAccessoryViewController {
        qualityMenu.removeAllItems()
        qualityMenu.addItems(withTitles: VideoQuality.allCases.map(\.title))
        qualityMenu.selectItem(at: VideoQuality.allCases.firstIndex(of: Settings.viewerQuality) ?? 0)
        qualityMenu.target = self
        qualityMenu.action = #selector(qualityChanged)
        qualityMenu.controlSize = .small
        qualityMenu.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        matchWindow.state = Settings.viewerMatchesWindow ? .on : .off
        matchWindow.target = self
        matchWindow.action = #selector(matchWindowChanged)
        matchWindow.controlSize = .small
        matchWindow.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        hoverControl.state = Settings.viewerHoverControl ? .on : .off
        hoverControl.target = self
        hoverControl.action = #selector(hoverControlChanged)
        hoverControl.controlSize = .small
        hoverControl.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        stats.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        stats.textColor = .secondaryLabelColor
        let label = NSTextField(labelWithString: "Quality:")
        label.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        displayMenu.target = self
        displayMenu.action = #selector(displayChanged)
        displayMenu.controlSize = .small
        displayMenu.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        displayMenu.isHidden = true
        let spacer = NSView()
        spacer.setContentHuggingPriority(.init(1), for: .horizontal)
        fullScreen.bezelStyle = .accessoryBarAction
        fullScreen.isBordered = false
        fullScreen.toolTip = "Full screen, one-to-one"
        let bar = NSStackView(views: [displayMenu, label, qualityMenu, matchWindow, hoverControl, spacer, stats, fullScreen])
        bar.orientation = .horizontal
        bar.spacing = 8
        bar.edgeInsets = NSEdgeInsets(top: 3, left: 10, bottom: 3, right: 10)
        bar.frame = NSRect(x: 0, y: 0, width: 600, height: 28)
        let accessory = NSTitlebarAccessoryViewController()
        accessory.view = bar
        accessory.layoutAttribute = .bottom
        return accessory
    }

    /// The peer's displays; the menu shows only when there is more than one.
    func setDisplays(_ list: [DisplayInfo]) {
        displays = list
        if selectedDisplay != 0, !list.contains(where: { $0.id == selectedDisplay }) {
            selectedDisplay = 0 // unplugged: the peer falls back to its main display
        }
        displayMenu.removeAllItems()
        for d in list {
            displayMenu.addItem(withTitle: "\(d.name)\(d.main ? " (main)" : "")")
        }
        let current = list.firstIndex { selectedDisplay == 0 ? $0.main : $0.id == selectedDisplay } ?? 0
        if !list.isEmpty { displayMenu.selectItem(at: current) }
        displayMenu.isHidden = list.count < 2
        updateSourcePixels()
    }

    @objc private func displayChanged() {
        let index = displayMenu.indexOfSelectedItem
        guard displays.indices.contains(index) else { return }
        let d = displays[index]
        selectedDisplay = d.main ? 0 : d.id
        updateSourcePixels()
        sized = window?.styleMask.contains(.fullScreen) == true // windowed: refit to the new shape
        onStreamSettingsChanged?()
    }

    @objc private func qualityChanged() {
        Settings.viewerQuality = VideoQuality.allCases[max(qualityMenu.indexOfSelectedItem, 0)]
        onStreamSettingsChanged?()
    }

    @objc private func toggleFullScreen() {
        window?.toggleFullScreen(nil)
    }

    func windowWillEnterFullScreen(_ notification: Notification) {
        // The aspect lock would fight the screen's own shape.
        window?.contentResizeIncrements = NSSize(width: 1, height: 1)
        view.oneToOne = true
    }

    func windowDidEnterFullScreen(_ notification: Notification) {
        onStreamSettingsChanged?() // the picture can now be as big as the screen
        onPictureMoved?()
    }

    func windowDidExitFullScreen(_ notification: Notification) {
        view.oneToOne = false
        if let format {
            let dims = CMVideoFormatDescriptionGetDimensions(format)
            window?.contentAspectRatio = CGSize(width: CGFloat(dims.width) / CGFloat(max(dims.height, 1)), height: 1)
        }
        onStreamSettingsChanged?()
        onPictureMoved?()
    }

    /// The viewed display's native pixel size, from the peer's display list.
    private func updateSourcePixels() {
        let d = displays.first { selectedDisplay == 0 ? $0.main : $0.id == selectedDisplay }
        view.sourcePixels = d.map { CGSize(width: $0.width, height: $0.height) }
    }

    @objc private func hoverControlChanged() {
        Settings.viewerHoverControl = hoverControl.state == .on
        onPictureMoved?()
    }

    func windowDidMove(_ notification: Notification) {
        onPictureMoved?()
    }

    /// The picture in AppKit screen coordinates (bottom-left origin).
    private var pictureOnScreen: CGRect? {
        guard let window, format != nil, window.isVisible, !window.isMiniaturized else { return nil }
        return window.convertToScreen(view.convert(view.videoRect, to: nil))
    }

    /// The picture in global display coordinates (top-left origin), for the event tap.
    var pictureRect: CGRect? {
        guard let r = pictureOnScreen, let primary = NSScreen.screens.first else { return nil }
        return CGRect(x: r.minX, y: primary.frame.maxY - r.maxY, width: r.width, height: r.height)
    }

    /// Whether the viewer is what's under `point` (global coordinates), not a window on top.
    func isFrontmost(at point: CGPoint) -> Bool {
        guard let window, let primary = NSScreen.screens.first else { return false }
        let appKit = NSPoint(x: point.x, y: primary.frame.maxY - point.y)
        return NSWindow.windowNumber(at: appKit, belowWindowWithWindowNumber: 0) == window.windowNumber
    }

    /// `point` (global coordinates) as a fraction of the picture, top-left origin.
    func fraction(at point: CGPoint) -> CGPoint? {
        guard let r = pictureRect, r.width > 0, r.height > 0 else { return nil }
        return CGPoint(x: (point.x - r.minX) / r.width, y: (point.y - r.minY) / r.height)
    }

    /// After an edge exit: put the cursor just outside the picture, on the side and at the
    /// spot where the peer's pointer left its screen.
    func placeCursorOutside(edge: Int, along: UInt16) {
        guard let r = pictureRect else { return }
        let f = CGFloat(along) / 65535
        let gap: CGFloat = 12
        var p: CGPoint = switch edge {
        case 0: CGPoint(x: r.minX - gap, y: r.minY + f * r.height)
        case 1: CGPoint(x: r.maxX + gap, y: r.minY + f * r.height)
        case 2: CGPoint(x: r.minX + f * r.width, y: r.minY - gap)
        default: CGPoint(x: r.minX + f * r.width, y: r.maxY + gap)
        }
        // At a screen edge there's no "outside": stay on the screen (hover stays disarmed
        // until the pointer leaves the picture anyway).
        let screens = Injector.displayBounds()
        if !screens.contains(where: { $0.contains(p) }), let home = screens.first(where: { $0.intersects(r) }) {
            p = CGPoint(x: min(max(p.x, home.minX), home.maxX - 1), y: min(max(p.y, home.minY), home.maxY - 1))
        }
        CGEventSource(stateID: .combinedSessionState)?.localEventsSuppressionInterval = 0
        CGWarpMouseCursorPosition(p)
    }

    @objc private func matchWindowChanged() {
        Settings.viewerMatchesWindow = matchWindow.state == .on
        onStreamSettingsChanged?()
    }

    func windowDidResize(_ notification: Notification) {
        onPictureMoved?()
        guard Settings.viewerMatchesWindow else { return }
        resizeDebounce?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, let window, let last = requestedSize else { return }
            let scale = window.backingScaleFactor
            let now = CGSize(width: view.bounds.width * scale, height: view.bounds.height * scale)
            // Ask again only for a real change, not a few pixels.
            if abs(now.width - last.width) / last.width > 0.1 || abs(now.height - last.height) / last.height > 0.1 {
                onStreamSettingsChanged?()
            }
        }
        resizeDebounce = work
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.4, execute: work)
    }

    private func updateStats() {
        defer { (framesThisSecond, bytesThisSecond) = (0, 0) }
        guard let format else {
            stats.stringValue = ""
            return
        }
        let dims = CMVideoFormatDescriptionGetDimensions(format)
        let mbps = Double(bytesThisSecond) * 8 / 1_000_000
        stats.stringValue = String(format: "%d×%d · %d fps · %.1f Mbit/s", dims.width, dims.height, framesThisSecond, mbps)
    }

    private func enqueue(_ frame: Data, keyframe: Bool) {
        framesThisSecond += 1
        bytesThisSecond += frame.count
        let layer = view.displayLayer
        if layer.status == .failed {
            Log.warn("video decode failed: \(String(describing: layer.error)), restarting stream")
            layer.flush()
            format = nil
            onDecodeFailure?()
            return
        }
        guard let (format, avcc) = parse(frame, keyframe: keyframe) else { return }
        guard let sample = makeSample(avcc, format: format) else { return }
        layer.enqueue(sample)
        if view.status != nil { view.status = nil }
        if !sized {
            sizeWindow(to: format)
            onPictureMoved?() // the picture now exists (and has its size)
        }
    }

    /// Splits off (and on keyframes, updates) the H.264 parameter sets.
    private func parse(_ frame: Data, keyframe: Bool) -> (CMVideoFormatDescription, Data)? {
        let bytes = [UInt8](frame)
        guard bytes.count >= 2 else { return nil }
        let nalLength = Int32(bytes[0])
        let count = Int(bytes[1])
        var offset = 2
        var sets: [[UInt8]] = []
        for _ in 0..<count {
            guard offset + 2 <= bytes.count else { return nil }
            let len = Int(bytes[offset]) << 8 | Int(bytes[offset + 1])
            offset += 2
            guard offset + len <= bytes.count else { return nil }
            sets.append(Array(bytes[offset..<offset + len]))
            offset += len
        }
        if !sets.isEmpty {
            var newFormat: CMVideoFormatDescription?
            let status = withPointers(sets) { pointers, sizes in
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    allocator: nil, parameterSetCount: sets.count, parameterSetPointers: pointers,
                    parameterSetSizes: sizes, nalUnitHeaderLength: nalLength, formatDescriptionOut: &newFormat)
            }
            if status == noErr, let newFormat {
                // A new size (the host adapting quality) keeps the window as it is.
                format = newFormat
                let dims = CMVideoFormatDescriptionGetDimensions(newFormat)
                view.videoSize = CGSize(width: CGFloat(dims.width), height: CGFloat(dims.height))
            }
        }
        guard let format else { return nil } // not decodable until the first keyframe
        return (format, Data(bytes[offset...]))
    }

    private func withPointers<R>(_ sets: [[UInt8]], _ body: (UnsafePointer<UnsafePointer<UInt8>>, UnsafePointer<Int>) -> R) -> R {
        let buffers = sets.map { set -> UnsafeMutablePointer<UInt8> in
            let p = UnsafeMutablePointer<UInt8>.allocate(capacity: set.count)
            p.initialize(from: set, count: set.count)
            return p
        }
        defer { buffers.forEach { $0.deallocate() } }
        let pointers = buffers.map { UnsafePointer($0) }
        let sizes = sets.map(\.count)
        return pointers.withUnsafeBufferPointer { p in
            sizes.withUnsafeBufferPointer { s in body(p.baseAddress!, s.baseAddress!) }
        }
    }

    private func makeSample(_ avcc: Data, format: CMVideoFormatDescription) -> CMSampleBuffer? {
        var block: CMBlockBuffer?
        guard CMBlockBufferCreateWithMemoryBlock(
            allocator: nil, memoryBlock: nil, blockLength: avcc.count, blockAllocator: nil,
            customBlockSource: nil, offsetToData: 0, dataLength: avcc.count, flags: 0,
            blockBufferOut: &block) == noErr, let block else { return nil }
        avcc.withUnsafeBytes { raw in
            _ = CMBlockBufferReplaceDataBytes(with: raw.baseAddress!, blockBuffer: block, offsetIntoDestination: 0, dataLength: avcc.count)
        }
        var sample: CMSampleBuffer?
        var size = avcc.count
        guard CMSampleBufferCreateReady(
            allocator: nil, dataBuffer: block, formatDescription: format, sampleCount: 1,
            sampleTimingEntryCount: 0, sampleTimingArray: nil, sampleSizeEntryCount: 1,
            sampleSizeArray: &size, sampleBufferOut: &sample) == noErr, let sample else { return nil }
        if let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: true),
           CFArrayGetCount(attachments) > 0 {
            let dict = unsafeBitCast(CFArrayGetValueAtIndex(attachments, 0), to: CFMutableDictionary.self)
            CFDictionarySetValue(dict, Unmanaged.passUnretained(kCMSampleAttachmentKey_DisplayImmediately).toOpaque(),
                                 Unmanaged.passUnretained(kCFBooleanTrue).toOpaque())
        }
        return sample
    }

    /// First frame: size the window to the stream's aspect ratio, within the screen.
    private func sizeWindow(to format: CMVideoFormatDescription) {
        guard let window, let screen = window.screen ?? NSScreen.main else { return }
        sized = true
        guard !window.styleMask.contains(.fullScreen) else { return }
        let dims = CMVideoFormatDescriptionGetDimensions(format)
        let aspect = CGFloat(dims.width) / CGFloat(max(dims.height, 1))
        let visible = screen.visibleFrame.insetBy(dx: 40, dy: 40)
        var size = CGSize(width: visible.width, height: visible.width / aspect)
        if size.height > visible.height {
            size = CGSize(width: visible.height * aspect, height: visible.height)
        }
        window.contentAspectRatio = CGSize(width: aspect, height: 1)
        window.setContentSize(size)
        window.center()
    }
}

/// Hosts the display layer and a status line, and turns clicks into screen fractions.
private final class VideoView: NSView {
    let displayLayer = AVSampleBufferDisplayLayer()
    private let label = NSTextField(labelWithString: "")
    var onClick: ((CGPoint) -> Void)?
    var onDrop: (([URL]) -> Void)?
    /// Size of the stream, to locate the aspect-fit picture inside the view.
    var videoSize = CGSize.zero { didSet { needsLayout = true } }
    /// One-to-one (full screen): draw the peer's display at one of its pixels per pixel
    /// here, never larger, centered. `sourcePixels` is that display's native size, so the
    /// picture keeps its size when the stream's resolution adapts.
    var oneToOne = false { didSet { needsLayout = true } }
    var sourcePixels: CGSize? { didSet { needsLayout = true } }

    /// Where the picture sits inside the view.
    var videoRect: CGRect {
        let backing = window?.backingScaleFactor ?? 2
        if oneToOne, let source = sourcePixels, source.width > 0, source.height > 0 {
            let points = CGSize(width: source.width / backing, height: source.height / backing)
            if points.width <= bounds.width, points.height <= bounds.height {
                // Whole points keep pixels aligned.
                let x = ((bounds.width - points.width) / 2).rounded(.down)
                let y = ((bounds.height - points.height) / 2).rounded(.down)
                return CGRect(x: x, y: y, width: points.width, height: points.height)
            }
            return Self.fit(source, in: bounds) // bigger than this screen: scale down
        }
        guard videoSize.width > 0, videoSize.height > 0 else { return bounds }
        return Self.fit(videoSize, in: bounds)
    }

    private static func fit(_ size: CGSize, in bounds: CGRect) -> CGRect {
        let scale = min(bounds.width / size.width, bounds.height / size.height)
        let w = size.width * scale, h = size.height * scale
        return CGRect(x: bounds.midX - w / 2, y: bounds.midY - h / 2, width: w, height: h)
    }

    var status: String? {
        didSet {
            label.stringValue = status ?? ""
            label.isHidden = status == nil
        }
    }

    override init(frame: NSRect) {
        super.init(frame: frame)
        wantsLayer = true
        layer?.backgroundColor = NSColor.black.cgColor
        displayLayer.videoGravity = .resizeAspect
        layer?.addSublayer(displayLayer)
        label.textColor = .secondaryLabelColor
        label.alignment = .center
        label.translatesAutoresizingMaskIntoConstraints = false
        addSubview(label)
        registerForDraggedTypes([.fileURL])
        NSLayoutConstraint.activate([
            label.centerXAnchor.constraint(equalTo: centerXAnchor),
            label.centerYAnchor.constraint(equalTo: centerYAnchor),
        ])
    }

    required init?(coder: NSCoder) { fatalError() }

    override func layout() {
        super.layout()
        CATransaction.begin()
        CATransaction.setDisableActions(true)
        displayLayer.frame = oneToOne ? videoRect : bounds
        CATransaction.commit()
    }

    override func acceptsFirstMouse(for event: NSEvent?) -> Bool { true }

    override func draggingEntered(_ sender: NSDraggingInfo) -> NSDragOperation { .copy }

    override func performDragOperation(_ sender: NSDraggingInfo) -> Bool {
        guard let urls = sender.draggingPasteboard.readObjects(
            forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL], !urls.isEmpty else { return false }
        onDrop?(urls)
        return true
    }

    override func mouseDown(with event: NSEvent) {
        let video = videoRect
        guard video.width > 0, video.height > 0 else { return }
        let p = convert(event.locationInWindow, from: nil)
        guard video.contains(p) else { return }
        // AppKit views have a bottom-left origin; screens are addressed from the top left.
        onClick?(CGPoint(x: (p.x - video.minX) / video.width, y: 1 - (p.y - video.minY) / video.height))
    }
}
