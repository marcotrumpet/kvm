import AppKit
import AVFoundation
import CoreMedia

/// Window showing the peer's screen. Decoding and display happen on the GPU through
/// AVSampleBufferDisplayLayer. Clicking into it takes control of the peer (the hotkey
/// gives it back), with the peer's pointer jumping to the clicked point.
final class ScreenViewer: NSObject, NSWindowDelegate {
    private var window: NSWindow?
    private let view = VideoView()
    private var frames = VideoFrames()
    private var format: CMVideoFormatDescription? { frames.format }
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
    private let resizeHost = NSButton(checkboxWithTitle: "Resize host", target: nil, action: nil)
    private let sound = NSButton(checkboxWithTitle: "Sound", target: nil, action: nil)
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

    /// Whether the peer lays out its desktop at this window's size (its main display only);
    /// the window then has no fixed shape.
    var fitsHost: Bool { Settings.viewerResizesHost && selectedDisplay == 0 }

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
            frames.reset()
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
        qualityMenu.toolTip = """
            What to keep when the network is slow. Auto: balance both. Sharp text: resolution, \
            at a lower frame rate. Smooth motion: frame rate, at a lower resolution. Data saver: \
            less data overall.
            """
        matchWindow.toolTip = "Stream at this window's size, using less bandwidth. Off: always the full resolution"
        matchWindow.state = Settings.viewerMatchesWindow ? .on : .off
        matchWindow.target = self
        matchWindow.action = #selector(matchWindowChanged)
        matchWindow.controlSize = .small
        matchWindow.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        resizeHost.state = Settings.viewerResizesHost ? .on : .off
        resizeHost.target = self
        resizeHost.action = #selector(resizeHostChanged)
        resizeHost.controlSize = .small
        resizeHost.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        resizeHost.toolTip = "Change the other computer's resolution and shape to fit this window, while viewing it"
        sound.state = Settings.viewerAudio ? .on : .off
        sound.target = self
        sound.action = #selector(soundChanged)
        sound.controlSize = .small
        sound.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        sound.toolTip = "Play the other computer's sound here"
        hoverControl.toolTip = "Take control as soon as the pointer enters the picture. Off: click into it"
        hoverControl.state = Settings.viewerHoverControl ? .on : .off
        hoverControl.target = self
        hoverControl.action = #selector(hoverControlChanged)
        hoverControl.controlSize = .small
        hoverControl.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        stats.font = .monospacedDigitSystemFont(ofSize: NSFont.smallSystemFontSize, weight: .regular)
        stats.textColor = .secondaryLabelColor
        stats.toolTip = "The stream's resolution, frames per second and data rate"
        let label = NSTextField(labelWithString: "Quality:")
        label.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        displayMenu.target = self
        displayMenu.action = #selector(displayChanged)
        displayMenu.controlSize = .small
        displayMenu.font = .systemFont(ofSize: NSFont.smallSystemFontSize)
        displayMenu.isHidden = true
        displayMenu.toolTip = "Which of the other computer's displays to show"
        let spacer = NSView()
        spacer.setContentHuggingPriority(.init(1), for: .horizontal)
        fullScreen.bezelStyle = .accessoryBarAction
        fullScreen.isBordered = false
        fullScreen.toolTip = "Full screen, one-to-one"
        let bar = NSStackView(views: [displayMenu, label, qualityMenu, matchWindow, resizeHost, sound, hoverControl, spacer, stats, fullScreen])
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
        updateAspectLock()
        onStreamSettingsChanged?()
        onPictureMoved?()
    }

    /// Locks the window to the stream's shape, or frees it while the host fits the window.
    private func updateAspectLock() {
        guard let window, !window.styleMask.contains(.fullScreen) else { return }
        if fitsHost {
            window.contentResizeIncrements = NSSize(width: 1, height: 1)
        } else if let format {
            let dims = CMVideoFormatDescriptionGetDimensions(format)
            window.contentAspectRatio = CGSize(width: CGFloat(dims.width) / CGFloat(max(dims.height, 1)), height: 1)
        }
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

    @objc private func soundChanged() {
        Settings.viewerAudio = sound.state == .on
        onStreamSettingsChanged?()
    }

    @objc private func resizeHostChanged() {
        Settings.viewerResizesHost = resizeHost.state == .on
        updateAspectLock()
        onStreamSettingsChanged?()
    }

    func windowDidResize(_ notification: Notification) {
        onPictureMoved?()
        guard Settings.viewerMatchesWindow || fitsHost else { return }
        resizeDebounce?.cancel()
        let work = DispatchWorkItem { [weak self] in
            guard let self, let window, let last = requestedSize else { return }
            let scale = window.backingScaleFactor
            let now = CGSize(width: view.bounds.width * scale, height: view.bounds.height * scale)
            // Ask again only for a real change, not a few pixels. A fitted host follows closely
            // (it re-lays out its desktop, so keep that rare too).
            let threshold = fitsHost ? 0.02 : 0.1
            if abs(now.width - last.width) / last.width > threshold || abs(now.height - last.height) / last.height > threshold {
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
            frames.reset()
            onDecodeFailure?()
            return
        }
        guard let (sample, newFormat) = frames.sample(from: frame), let format else { return }
        if newFormat {
            // A new size (the host adapting quality) keeps the window as it is.
            let dims = CMVideoFormatDescriptionGetDimensions(format)
            view.videoSize = CGSize(width: CGFloat(dims.width), height: CGFloat(dims.height))
        }
        layer.enqueue(sample)
        if view.status != nil { view.status = nil }
        if !sized {
            sizeWindow(to: format)
            onPictureMoved?() // the picture now exists (and has its size)
            showFirstTip()
        }
    }

    /// Once ever, on the first picture: how to take control and give it back.
    private func showFirstTip() {
        guard !Settings.viewerTipShown else { return }
        Settings.viewerTipShown = true
        let enter = Settings.viewerHoverControl ? "Move the pointer into the picture" : "Click into the picture"
        view.showTip("""
            \(enter) to control the other computer. To come back, push the pointer out past the \
            picture's edge or press \(Settings.hotkey.display). Drop files here to send them.
            """)
    }

    /// First frame: size the window to the stream's aspect ratio, within the screen.
    private func sizeWindow(to format: CMVideoFormatDescription) {
        guard let window, let screen = window.screen ?? NSScreen.main else { return }
        sized = true
        guard !window.styleMask.contains(.fullScreen) else { return }
        guard !fitsHost else { // the stream already has the window's shape
            updateAspectLock()
            return
        }
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

/// Hosts the display layer, a status line and tips, and turns clicks into screen fractions.
private final class VideoView: NSView {
    let displayLayer = AVSampleBufferDisplayLayer()
    private let label = NSTextField(labelWithString: "")
    private weak var tip: NSView?
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

    /// A bubble at the top of the picture, gone after a while or when clicked.
    func showTip(_ text: String) {
        tip?.removeFromSuperview()
        let label = NSTextField(wrappingLabelWithString: text)
        label.font = .systemFont(ofSize: NSFont.systemFontSize)
        label.textColor = .labelColor
        label.translatesAutoresizingMaskIntoConstraints = false
        let bubble = NSVisualEffectView()
        bubble.material = .hudWindow
        bubble.state = .active
        bubble.wantsLayer = true
        bubble.layer?.cornerRadius = 10
        bubble.translatesAutoresizingMaskIntoConstraints = false
        bubble.addSubview(label)
        addSubview(bubble)
        NSLayoutConstraint.activate([
            label.leadingAnchor.constraint(equalTo: bubble.leadingAnchor, constant: 14),
            label.trailingAnchor.constraint(equalTo: bubble.trailingAnchor, constant: -14),
            label.topAnchor.constraint(equalTo: bubble.topAnchor, constant: 10),
            label.bottomAnchor.constraint(equalTo: bubble.bottomAnchor, constant: -10),
            bubble.centerXAnchor.constraint(equalTo: centerXAnchor),
            bubble.topAnchor.constraint(equalTo: topAnchor, constant: 16),
            bubble.widthAnchor.constraint(lessThanOrEqualToConstant: 420),
            bubble.widthAnchor.constraint(lessThanOrEqualTo: widthAnchor, constant: -32),
        ])
        tip = bubble
        DispatchQueue.main.asyncAfter(deadline: .now() + 12) { [weak self, weak bubble] in
            guard let bubble, bubble === self?.tip else { return }
            self?.hideTip()
        }
    }

    private func hideTip() {
        guard let tip else { return }
        self.tip = nil
        NSAnimationContext.runAnimationGroup { $0.duration = 0.3; tip.animator().alphaValue = 0 } completionHandler: {
            tip.removeFromSuperview()
        }
    }

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
        hideTip() // read, or in the way
        let video = videoRect
        guard video.width > 0, video.height > 0 else { return }
        let p = convert(event.locationInWindow, from: nil)
        guard video.contains(p) else { return }
        // AppKit views have a bottom-left origin; screens are addressed from the top left.
        onClick?(CGPoint(x: (p.x - video.minX) / video.width, y: 1 - (p.y - video.minY) / video.height))
    }
}
