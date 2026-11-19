import AVFoundation
import Combine
import GameController
import UIKit

/// Full-screen control of a computer: its screen (optional), touches as a trackpad or as
/// direct touch on the picture, the on-screen or a hardware keyboard, and a mouse or
/// trackpad attached to an iPad (pointer locked, raw deltas).
///
/// Trackpad gestures: drag moves the pointer, tap clicks, two-finger tap right-clicks,
/// two-finger drag scrolls (with momentum), touch and hold then drag drags.
/// Touch gestures: tap clicks where you tap, drag drags from there, touch and hold
/// right-clicks, two-finger drag scrolls.
/// Both, with the screen shown: pinch zooms the picture, and two-finger drag pans it while
/// zoomed. Without the screen, and on an iPad trackpad, pinch zooms and two fingers turning
/// rotate (real trackpad gestures on the computer). On the screen, three fingers swiping up
/// show Mission Control, down App Exposé, left and right switch Spaces.
/// An iPad trackpad or mouse moves the computer's pointer by raw deltas while iPadOS locks
/// the pointer to the app; otherwise the iPad's pointer points at the picture directly.
///
/// With several displays, the pointer stays on the one shown; pushing it past the left or
/// right edge switches to the display next to it.
final class RemoteViewController: UIViewController {
    enum TouchMode: String {
        case trackpad, touch
    }

    private let model: AppModel
    private var core: Core? { model.core }
    private let videoView = VideoView()
    private let status = UILabel()
    private let info = UILabel()
    private let keyboard = KeyboardInputView()
    private let bar = UIStackView()
    private let barBackground = UIVisualEffectView(effect: UIBlurEffect(style: .systemThinMaterialDark))
    private var barButtons: [UIView] = []
    private let displayButton = UIButton(type: .system)
    private var cancellables: Set<AnyCancellable> = []
    private var frames = VideoFrames()
    private var requestedSize: CGSize?

    private var screenOn = Prefs.screenOn
    /// The computer lays out its main display at this screen's size and shape.
    private var fitHost = Prefs.fitHost
    private var mode = Prefs.touchMode
    private var barCollapsed = false

    // Gestures
    private var last: (point: CGPoint, time: CFTimeInterval)?
    private var motionRemainder = CGPoint.zero
    private var scrollRemainder = CGPoint.zero
    private var dragging = false
    private var momentum: CADisplayLink?
    private var momentumVelocity = CGPoint.zero
    /// Pinch and rotation, sent once past a threshold so scrolling doesn't zoom.
    private var magnifying = false
    private var rotating = false
    private var lastScale: CGFloat = 1
    private var lastRotation: CGFloat = 0
    /// A pinch or rotation happened during this two-finger touch: no scroll momentum after.
    private var transformed = false
    private var together: [UIGestureRecognizer] = []
    /// The picture zoomed in on this device: scale and offset (view points) from centered.
    private var zoom: CGFloat = 1
    private var zoomOffset = CGPoint.zero
    /// A finger pinch is zooming the picture rather than the computer.
    private var zooming = false
    private var lastCentroid = CGPoint.zero
    private var pinchTouches = 0
    /// How much of the view the on-screen keyboard covers, from the bottom.
    private var keyboardHeight: CGFloat = 0
    private var laidOutSize = CGSize.zero
    /// The zoom the current stream's resolution was asked for.
    private var streamZoom: CGFloat = 1

    // Hardware keyboard and mouse
    private var heldKeys: Set<UInt16> = []
    private var repeatTimer: Timer?
    private var mouseRemainder = CGPoint.zero
    private var mouseObservers: [NSObjectProtocol] = []

    init(model: AppModel) {
        self.model = model
        super.init(nibName: nil, bundle: nil)
        modalPresentationStyle = .fullScreen
    }

    required init?(coder: NSCoder) { fatalError() }

    override var prefersStatusBarHidden: Bool { true }
    override var prefersHomeIndicatorAutoHidden: Bool { true }
    override var preferredScreenEdgesDeferringSystemGestures: UIRectEdge { .all }
    override var prefersPointerLocked: Bool { true }
    override var canBecomeFirstResponder: Bool { true }
    override var editingInteractionConfiguration: UIEditingInteractionConfiguration { .none }

    // MARK: Lifecycle

    override func viewDidLoad() {
        super.viewDidLoad()
        view.backgroundColor = .black
        view.addSubview(videoView)

        status.textColor = UIColor(white: 0.7, alpha: 1)
        status.font = .preferredFont(forTextStyle: .headline)
        status.textAlignment = .center
        status.numberOfLines = 0
        status.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(status)

        keyboard.frame = CGRect(x: 0, y: 0, width: 1, height: 1)
        keyboard.onText = { [weak self] text in self?.core?.sendText(text) }
        keyboard.onKey = { [weak self] usage, modifiers in self?.press(usage, modifiers: modifiers) }
        keyboard.onHardwareKey = { [weak self] press, down in self?.hardwareKey(press, down: down) ?? false }
        // Back to this controller, which takes hardware keys while the keyboard is away.
        keyboard.onResign = { [weak self] in DispatchQueue.main.async { self?.becomeFirstResponder() } }
        view.addSubview(keyboard)

        setUpBar()
        setUpGestures()
        NSLayoutConstraint.activate([
            status.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            status.centerYAnchor.constraint(equalTo: view.centerYAnchor),
            status.widthAnchor.constraint(lessThanOrEqualTo: view.widthAnchor, constant: -48),
        ])

        model.$link.combineLatest(model.$active).receive(on: RunLoop.main).sink { [weak self] _, active in
            guard let self else { return }
            if active == nil {
                dismiss(animated: true)
            } else {
                updateStatus()
            }
        }.store(in: &cancellables)
        model.$latencyMs.combineLatest(model.$direct).receive(on: RunLoop.main).sink { [weak self] ms, direct in
            var parts: [String] = []
            if let ms { parts.append(String(format: "%.0f ms", ms)) }
            if let direct, !direct { parts.append("relayed") }
            self?.info.text = parts.joined(separator: " · ")
        }.store(in: &cancellables)
        model.$displays.receive(on: RunLoop.main).sink { [weak self] _ in self?.updateDisplayMenu() }
            .store(in: &cancellables)
        NotificationCenter.default.publisher(for: UIResponder.keyboardWillChangeFrameNotification)
            .merge(with: NotificationCenter.default.publisher(for: UIResponder.keyboardWillHideNotification))
            .sink { [weak self] note in self?.keyboardMoved(note) }
            .store(in: &cancellables)
    }

    /// The picture moves up out of the keyboard's way, along with it.
    private func keyboardMoved(_ note: Notification) {
        guard let end = note.userInfo?[UIResponder.keyboardFrameEndUserInfoKey] as? CGRect,
              let screen = view.window?.screen else { return }
        let frame = view.convert(end, from: screen.coordinateSpace)
        // Hidden, or floating on an iPad (not across the bottom): nothing to avoid.
        let docked = note.name != UIResponder.keyboardWillHideNotification && frame.maxY >= view.bounds.maxY - 1
        let height = docked ? max(view.bounds.maxY - frame.minY, 0) : 0
        guard height != keyboardHeight else { return }
        keyboardHeight = height
        let duration = note.userInfo?[UIResponder.keyboardAnimationDurationUserInfoKey] as? Double ?? 0.25
        view.setNeedsLayout()
        UIView.animate(withDuration: duration) { self.view.layoutIfNeeded() }
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        UIApplication.shared.isIdleTimerDisabled = true
        becomeFirstResponder()
        setNeedsUpdateOfPrefersPointerLocked()
        startMice()
        if screenOn { startVideo() }
        updateStatus()
    }

    override func viewWillDisappear(_ animated: Bool) {
        super.viewWillDisappear(animated)
        UIApplication.shared.isIdleTimerDisabled = false
        stopMice()
        releaseKeys()
        stopMomentum()
        stopVideo()
    }

    override func viewDidLayoutSubviews() {
        super.viewDidLayoutSubviews()
        // Laid out by bounds and center, which a zoom transform leaves alone; above the
        // on-screen keyboard, so it doesn't cover the picture.
        let area = CGRect(x: 0, y: 0, width: view.bounds.width, height: max(view.bounds.height - keyboardHeight, 1))
        if videoView.bounds.size != area.size {
            videoView.bounds = CGRect(origin: .zero, size: area.size)
            if view.bounds.size != laidOutSize { resetZoom() } else { setZoom(zoom, offset: zoomOffset) }
            laidOutSize = view.bounds.size
        }
        videoView.center = CGPoint(x: area.midX, y: area.midY)
        // Ask for a stream matching the new shape after a rotation, not for every pixel.
        guard screenOn, let last = requestedSize else { return }
        let now = pixelSize
        if abs(now.width - last.width) / last.width > 0.1 || abs(now.height - last.height) / last.height > 0.1 {
            startVideo()
        }
    }

    private var pixelSize: CGSize {
        let scale = view.window?.screen.scale ?? UIScreen.main.scale
        return CGSize(width: view.bounds.width * scale, height: view.bounds.height * scale)
    }

    private func updateStatus() {
        let name = model.active?.name ?? "the computer"
        let text: String? = switch model.link {
        case .idle, .connecting: "Connecting to \(name)…"
        case .stalled: "Connection to \(name) lost, waiting…"
        case .connected: screenOn ? (videoView.videoSize == .zero ? "Waiting for the screen…" : nil)
            : "Use this screen as a trackpad for \(name)"
        }
        status.text = text
        status.isHidden = text == nil
    }

    // MARK: Screen

    private func startVideo() {
        requestedSize = pixelSize
        // Zoomed in, more pixels than the screen has keep the picture sharp (the computer
        // sends at most its display's own resolution). A fitted computer takes only this
        // screen's shape, so zooming still finds more pixels there.
        let size = CGSize(width: pixelSize.width * streamZoom, height: pixelSize.height * streamZoom)
        var params = VideoQuality.auto.params(maxSize: size)
        params.fitHost = fitHost ? .shape : .off
        model.view(params) { [weak self] data in
            DispatchQueue.main.async { self?.enqueue(data) }
        }
    }

    private func stopVideo() {
        requestedSize = nil
        model.stopViewing()
        frames.reset()
        videoView.renderer.flush(removingDisplayedImage: true, completionHandler: nil)
        videoView.videoSize = .zero
        resetZoom()
    }

    /// Zoom the picture to `scale`, shifted by `offset`, keeping the screen covered.
    private func setZoom(_ scale: CGFloat, offset: CGPoint) {
        let z = min(max(scale, 1), 5)
        let r = videoView.pictureRect
        let maxX = max(0, (r.width * z - videoView.bounds.width) / 2)
        let maxY = max(0, (r.height * z - videoView.bounds.height) / 2)
        zoom = z
        zoomOffset = CGPoint(x: min(max(offset.x, -maxX), maxX), y: min(max(offset.y, -maxY), maxY))
        videoView.transform = CGAffineTransform(translationX: zoomOffset.x, y: zoomOffset.y).scaledBy(x: z, y: z)
    }

    /// Back to the whole picture; the next stream request is for the screen's size again.
    private func resetZoom() {
        zooming = false
        streamZoom = 1
        setZoom(1, offset: .zero)
    }

    /// After a pinch: ask for a stream to match the zoom, when it changed enough to matter.
    private func updateStreamZoom() {
        guard abs(zoom / streamZoom - 1) > 0.15 || (zoom == 1 && streamZoom != 1) else { return }
        streamZoom = zoom
        if screenOn, requestedSize != nil { startVideo() }
    }

    private func enqueue(_ frame: Data) {
        guard screenOn else { return }
        let renderer = videoView.renderer
        if renderer.status == .failed {
            renderer.flush()
            frames.reset()
            model.restartVideo()
            return
        }
        guard let (sample, newFormat) = frames.sample(from: frame), let format = frames.format else { return }
        if newFormat {
            let dims = CMVideoFormatDescriptionGetDimensions(format)
            let size = CGSize(width: CGFloat(dims.width), height: CGFloat(dims.height))
            // Another display or shape, not just a sharper stream: start from the whole picture.
            let old = videoView.videoSize
            if old.height == 0 || abs(size.width / size.height - old.width / old.height) > 0.01 {
                let wasZoomed = streamZoom != 1
                resetZoom()
                if wasZoomed { startVideo() }
            }
            videoView.videoSize = size
            updateStatus()
        }
        renderer.enqueue(sample)
    }

    /// `p` (view coordinates) as a fraction of the picture, or nil without one.
    private func fraction(at p: CGPoint) -> CGPoint? {
        let r = videoView.pictureRect
        guard screenOn, r.width > 0, r.height > 0 else { return nil }
        return CGPoint(x: min(max((p.x - r.minX) / r.width, 0), 1), y: min(max((p.y - r.minY) / r.height, 0), 1))
    }

    private var touchMode: Bool { mode == .touch && videoView.videoSize != .zero && screenOn }

    // MARK: Bar

    private func setUpBar() {
        func button(_ symbol: String, _ label: String, action: @escaping () -> Void) -> UIButton {
            var config = UIButton.Configuration.plain()
            config.image = UIImage(systemName: symbol)
            config.baseForegroundColor = .white
            config.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 8, bottom: 8, trailing: 8)
            let b = UIButton(configuration: config, primaryAction: UIAction { _ in action() })
            b.accessibilityLabel = label
            return b
        }
        let close = button("xmark", "Disconnect") { [weak self] in self?.model.disconnect() }
        let keys = button("keyboard", "Keyboard") { [weak self] in self?.toggleKeyboard() }
        let modeButton = button(mode == .touch ? "hand.point.up.left" : "rectangle.and.hand.point.up.left", "Touch mode") {}
        modeButton.addAction(UIAction { [weak self, weak modeButton] _ in
            guard let self else { return }
            mode = mode == .touch ? .trackpad : .touch
            Prefs.touchMode = mode
            modeButton?.configuration?.image = UIImage(systemName: mode == .touch ? "hand.point.up.left" : "rectangle.and.hand.point.up.left")
            showHint(mode == .touch ? "Touch: tap where you want to click" : "Trackpad: drag to move the pointer")
        }, for: .primaryActionTriggered)
        let screen = button(screenOn ? "display" : "rectangle.dashed", "Show screen") {}
        screen.addAction(UIAction { [weak self, weak screen] _ in
            guard let self else { return }
            screenOn.toggle()
            Prefs.screenOn = screenOn
            screen?.configuration?.image = UIImage(systemName: screenOn ? "display" : "rectangle.dashed")
            if screenOn { startVideo() } else { stopVideo() }
            updateDisplayMenu()
            updateStatus()
        }, for: .primaryActionTriggered)
        let fit = button(fitHost ? "aspectratio.fill" : "aspectratio", "Fit computer to this screen") {}
        fit.addAction(UIAction { [weak self, weak fit] _ in
            guard let self else { return }
            fitHost.toggle()
            Prefs.fitHost = fitHost
            fit?.configuration?.image = UIImage(systemName: fitHost ? "aspectratio.fill" : "aspectratio")
            showHint(fitHost ? "The computer's resolution follows this screen" : "The computer keeps its own resolution")
            if screenOn {
                resetZoom()
                startVideo()
            }
        }, for: .primaryActionTriggered)
        let clipboard = button("doc.on.clipboard", "Send clipboard") { [weak self] in
            self?.model.sendClipboard()
            self?.showHint("Clipboard sent: paste it on the computer")
        }
        var displayConfig = UIButton.Configuration.plain()
        displayConfig.image = UIImage(systemName: "rectangle.on.rectangle")
        displayConfig.baseForegroundColor = .white
        displayConfig.contentInsets = NSDirectionalEdgeInsets(top: 8, leading: 8, bottom: 8, trailing: 8)
        displayButton.configuration = displayConfig
        displayButton.showsMenuAsPrimaryAction = true
        displayButton.accessibilityLabel = "Display"
        displayButton.isHidden = true
        info.font = .monospacedDigitSystemFont(ofSize: 12, weight: .regular)
        info.textColor = .lightGray
        let collapse = button("chevron.right", "Hide controls") {}
        collapse.addAction(UIAction { [weak self, weak collapse] _ in
            guard let self else { return }
            barCollapsed.toggle()
            UIView.animate(withDuration: 0.2) {
                self.barButtons.forEach { $0.isHidden = self.barCollapsed }
                if !self.barCollapsed { self.updateDisplayMenu() }
            }
            collapse?.configuration?.image = UIImage(systemName: barCollapsed ? "chevron.left" : "chevron.right")
        }, for: .primaryActionTriggered)

        // Mac actions that need three trackpad fingers, which iPadOS keeps for itself.
        let actions: [(title: String, symbol: String, run: () -> Void)] = [
            ("Previous Space", "arrow.left.square", { [weak self] in self?.switchSpace(left: true) }),
            ("Mission Control", "rectangle.3.group", { [weak self] in self?.missionControl() }),
            ("App Exposé", "rectangle.stack", { [weak self] in self?.appExpose() }),
            ("Next Space", "arrow.right.square", { [weak self] in self?.switchSpace(left: false) }),
        ]
        var macButtons: [UIView]
        if traitCollection.userInterfaceIdiom == .pad {
            macButtons = actions.map { button($0.symbol, $0.title, action: $0.run) }
        } else { // narrow: one menu
            let menu = button("square.grid.2x2", "Mac actions") {}
            menu.menu = UIMenu(children: actions.map { a in
                UIAction(title: a.title, image: UIImage(systemName: a.symbol)) { _ in a.run() }
            })
            menu.showsMenuAsPrimaryAction = true
            macButtons = [menu]
        }

        barButtons = [close, keys, modeButton, screen, fit, displayButton, clipboard] + macButtons + [info]
        bar.addArrangedSubviews(barButtons + [collapse])
        bar.axis = .horizontal
        bar.alignment = .center
        bar.spacing = 2
        bar.isLayoutMarginsRelativeArrangement = true
        bar.directionalLayoutMargins = NSDirectionalEdgeInsets(top: 0, leading: 6, bottom: 0, trailing: 2)
        bar.translatesAutoresizingMaskIntoConstraints = false
        barBackground.translatesAutoresizingMaskIntoConstraints = false
        barBackground.layer.cornerRadius = 22
        barBackground.clipsToBounds = true
        barBackground.contentView.addSubview(bar)
        view.addSubview(barBackground)
        NSLayoutConstraint.activate([
            bar.leadingAnchor.constraint(equalTo: barBackground.contentView.leadingAnchor),
            bar.trailingAnchor.constraint(equalTo: barBackground.contentView.trailingAnchor),
            bar.topAnchor.constraint(equalTo: barBackground.contentView.topAnchor),
            bar.bottomAnchor.constraint(equalTo: barBackground.contentView.bottomAnchor),
            bar.heightAnchor.constraint(equalToConstant: 44),
            barBackground.trailingAnchor.constraint(equalTo: view.safeAreaLayoutGuide.trailingAnchor, constant: -8),
            barBackground.topAnchor.constraint(equalTo: view.safeAreaLayoutGuide.topAnchor, constant: 8),
        ])
    }

    private func updateDisplayMenu() {
        let list = model.displays
        displayButton.isHidden = barCollapsed || !screenOn || list.count < 2
        displayButton.menu = UIMenu(title: "Display", children: list.map { d in
            let selected = model.selectedDisplay == 0 ? d.main : d.id == model.selectedDisplay
            return UIAction(title: d.name + (d.main ? " (main)" : ""), state: selected ? .on : .off) { [weak self] _ in
                self?.model.selectedDisplay = d.main ? 0 : d.id
                self?.updateDisplayMenu()
            }
        })
    }

    private func showHint(_ text: String) {
        let hint = UILabel()
        hint.text = "  \(text)  "
        hint.textColor = .white
        hint.font = .preferredFont(forTextStyle: .subheadline)
        hint.backgroundColor = UIColor(white: 0.15, alpha: 0.9)
        hint.layer.cornerRadius = 12
        hint.clipsToBounds = true
        hint.translatesAutoresizingMaskIntoConstraints = false
        view.addSubview(hint)
        NSLayoutConstraint.activate([
            hint.centerXAnchor.constraint(equalTo: view.centerXAnchor),
            hint.bottomAnchor.constraint(equalTo: view.safeAreaLayoutGuide.bottomAnchor, constant: -24),
            hint.heightAnchor.constraint(equalToConstant: 34),
        ])
        UIView.animate(withDuration: 0.3, delay: 1.8, options: []) { hint.alpha = 0 } completion: { _ in hint.removeFromSuperview() }
    }

    private func toggleKeyboard() {
        if keyboard.isFirstResponder {
            _ = keyboard.resignFirstResponder()
        } else {
            keyboard.becomeFirstResponder()
        }
    }

    // MARK: Gestures

    private func setUpGestures() {
        let direct = [NSNumber(value: UITouch.TouchType.direct.rawValue)] // a mouse goes through GCMouse
        let move = UIPanGestureRecognizer(target: self, action: #selector(pan(_:)))
        move.maximumNumberOfTouches = 1
        let scroll = UIPanGestureRecognizer(target: self, action: #selector(scroll(_:)))
        scroll.minimumNumberOfTouches = 2
        scroll.maximumNumberOfTouches = 2
        let tap = UITapGestureRecognizer(target: self, action: #selector(tap(_:)))
        let twoFingerTap = UITapGestureRecognizer(target: self, action: #selector(twoFingerTap(_:)))
        twoFingerTap.numberOfTouchesRequired = 2
        let hold = UILongPressGestureRecognizer(target: self, action: #selector(hold(_:)))
        hold.minimumPressDuration = 0.4
        for g in [move, scroll, tap, twoFingerTap, hold] as [UIGestureRecognizer] {
            g.allowedTouchTypes = direct
            videoView.addGestureRecognizer(g)
        }

        // The iPad's own pointer, when it isn't locked to the app.
        let pointer = [NSNumber(value: UITouch.TouchType.indirectPointer.rawValue)]
        let hover = UIHoverGestureRecognizer(target: self, action: #selector(pointerHover(_:)))
        let pointerDrag = UIPanGestureRecognizer(target: self, action: #selector(pointerDrag(_:)))
        pointerDrag.maximumNumberOfTouches = 1
        let pointerClick = UITapGestureRecognizer(target: self, action: #selector(pointerClick(_:)))
        let pointerRightClick = UITapGestureRecognizer(target: self, action: #selector(pointerRightClick(_:)))
        pointerRightClick.buttonMaskRequired = .secondary
        for g in [pointerDrag, pointerClick, pointerRightClick] as [UIGestureRecognizer] {
            g.allowedTouchTypes = pointer
        }
        // Two-finger scrolling on a trackpad, or a mouse wheel: scroll events, no touches.
        let pointerScroll = UIPanGestureRecognizer(target: self, action: #selector(scroll(_:)))
        pointerScroll.allowedScrollTypesMask = .all
        pointerScroll.allowedTouchTypes = []
        for g in [hover, pointerDrag, pointerClick, pointerRightClick, pointerScroll] as [UIGestureRecognizer] {
            videoView.addGestureRecognizer(g)
        }

        // Fingers or trackpad: pinch and rotation, alongside two-finger scrolling.
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(pinch(_:)))
        let rotation = UIRotationGestureRecognizer(target: self, action: #selector(rotation(_:)))
        let swipe = UIPanGestureRecognizer(target: self, action: #selector(threeFingerSwipe(_:)))
        swipe.minimumNumberOfTouches = 3
        swipe.maximumNumberOfTouches = 3
        swipe.allowedTouchTypes = direct
        together = [scroll, pointerScroll, pinch, rotation]
        for g in together { g.delegate = self }
        for g in [pinch, rotation, swipe] as [UIGestureRecognizer] {
            videoView.addGestureRecognizer(g)
        }
        videoView.addInteraction(UIPointerInteraction(delegate: self))
    }

    /// iPadOS locked the pointer to the app: a mouse or trackpad goes through GCMouse.
    private var pointerLocked: Bool { view.window?.windowScene?.pointerLockState?.isLocked ?? false }

    @objc private func pointerHover(_ g: UIHoverGestureRecognizer) {
        guard !pointerLocked, g.state == .began || g.state == .changed else { return }
        point(at: g.location(in: videoView))
    }

    @objc private func pointerDrag(_ g: UIPanGestureRecognizer) {
        guard !pointerLocked else { return }
        point(at: g.location(in: videoView))
        switch g.state {
        case .began: core?.sendButton(0, down: true)
        case .changed: break
        default: core?.sendButton(0, down: false)
        }
    }

    @objc private func pointerClick(_ g: UITapGestureRecognizer) {
        guard !pointerLocked else { return }
        point(at: g.location(in: videoView))
        click(0)
    }

    @objc private func pointerRightClick(_ g: UITapGestureRecognizer) {
        guard !pointerLocked else { return }
        point(at: g.location(in: videoView))
        click(1)
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        stopMomentum()
        super.touchesBegan(touches, with: event)
    }

    @objc private func pan(_ g: UIPanGestureRecognizer) {
        let p = g.location(in: videoView)
        let now = CACurrentMediaTime()
        switch g.state {
        case .began:
            last = (p, now)
            if touchMode {
                point(at: p)
                core?.sendButton(0, down: true)
                dragging = true
            }
        case .changed:
            guard let previous = last else { return }
            last = (p, now)
            if dragging {
                point(at: p)
            } else {
                move(by: CGPoint(x: p.x - previous.point.x, y: p.y - previous.point.y), over: now - previous.time)
            }
        default:
            last = nil
            if dragging {
                core?.sendButton(0, down: false)
                dragging = false
            }
        }
    }

    /// Relative pointer motion with acceleration: precise when slow, far when fast.
    private func move(by d: CGPoint, over dt: CFTimeInterval, accelerate: Bool = true) {
        let speed = hypot(d.x, d.y) / max(dt, 1.0 / 240)
        let gain = Prefs.pointerSpeed * (accelerate ? 1 + min(speed / 500, 3) : 1.5)
        let x = d.x * gain + motionRemainder.x, y = d.y * gain + motionRemainder.y
        let (ix, iy) = (x.rounded(.towardZero), y.rounded(.towardZero))
        motionRemainder = CGPoint(x: x - ix, y: y - iy)
        if ix != 0 || iy != 0 { core?.sendMotion(dx: Int64(ix), dy: Int64(iy)) }
    }

    @objc private func scroll(_ g: UIPanGestureRecognizer) {
        if g.state == .began { transformed = false }
        let fingers = !g.allowedTouchTypes.isEmpty
        // Zoomed in, two fingers pan the picture instead of scrolling the computer.
        if fingers, zooming || zoom > 1 {
            transformed = true
            if !zooming {
                let t = g.translation(in: view)
                setZoom(zoom, offset: CGPoint(x: zoomOffset.x + t.x, y: zoomOffset.y + t.y))
            }
            g.setTranslation(.zero, in: view)
            return
        }
        switch g.state {
        case .began, .changed:
            if !magnifying, !rotating { scroll(by: g.translation(in: videoView)) }
            g.setTranslation(.zero, in: videoView)
        case .ended:
            // Trackpads and wheels bring their own momentum.
            if fingers, !transformed { startMomentum(g.velocity(in: videoView)) }
        default:
            break
        }
    }

    /// Content follows the fingers, like on a trackpad.
    private func scroll(by d: CGPoint) {
        let x = d.x * 2 + scrollRemainder.x, y = d.y * 2 + scrollRemainder.y
        let (ix, iy) = (x.rounded(.towardZero), y.rounded(.towardZero))
        scrollRemainder = CGPoint(x: x - ix, y: y - iy)
        if ix != 0 || iy != 0 { core?.sendMotion(dx: 0, dy: 0, scrollX: Int64(ix), scrollY: Int64(iy)) }
    }

    private func startMomentum(_ velocity: CGPoint) {
        stopMomentum()
        guard hypot(velocity.x, velocity.y) > 200 else { return }
        momentumVelocity = velocity
        let link = CADisplayLink(target: self, selector: #selector(momentumTick(_:)))
        link.add(to: .main, forMode: .common)
        momentum = link
    }

    @objc private func momentumTick(_ link: CADisplayLink) {
        let dt = link.targetTimestamp - link.timestamp
        scroll(by: CGPoint(x: momentumVelocity.x * dt, y: momentumVelocity.y * dt))
        let decay = pow(0.05, dt) // keeps 5% of the speed per second
        momentumVelocity = CGPoint(x: momentumVelocity.x * decay, y: momentumVelocity.y * decay)
        if hypot(momentumVelocity.x, momentumVelocity.y) < 30 { stopMomentum() }
    }

    private func stopMomentum() {
        momentum?.invalidate()
        momentum = nil
    }

    @objc private func pinch(_ g: UIPinchGestureRecognizer) {
        // Fingers on the picture zoom it here; a trackpad pinch (no touches) goes to the computer.
        if g.state == .began, screenOn, videoView.videoSize != .zero, g.numberOfTouches >= 2 {
            zooming = true
            transformed = true
            stopMomentum()
            lastScale = g.scale
            lastCentroid = g.location(in: view)
            pinchTouches = g.numberOfTouches
            return
        }
        if zooming {
            zoomPicture(g)
            return
        }
        switch g.state {
        case .began:
            lastScale = g.scale
        case .changed:
            if !magnifying {
                guard abs(g.scale / lastScale - 1) > 0.08 else { return }
                magnifying = true
                transformed = true
                stopMomentum()
                lastScale = g.scale
                core?.sendGesture(Gesture(kind: .magnify, phase: .began, value: 0))
                return
            }
            // macOS apps scale by 1 + magnification per event.
            core?.sendGesture(Gesture(kind: .magnify, phase: .changed, value: g.scale / lastScale - 1))
            lastScale = g.scale
        default:
            if magnifying { core?.sendGesture(Gesture(kind: .magnify, phase: .ended, value: 0)) }
            magnifying = false
        }
    }

    /// Zoom about the fingers, which also pan the picture as they move.
    private func zoomPicture(_ g: UIPinchGestureRecognizer) {
        guard g.state == .changed else {
            zooming = false
            if zoom < 1.05 { UIView.animate(withDuration: 0.2) { self.setZoom(1, offset: .zero) } }
            updateStreamZoom()
            return
        }
        let c = g.location(in: view)
        // A finger lifted or came back: the centroid jumps, so start over from here.
        guard g.numberOfTouches == pinchTouches else {
            pinchTouches = g.numberOfTouches
            lastScale = g.scale
            lastCentroid = c
            return
        }
        let k = min(max(zoom * g.scale / lastScale, 1), 5) / zoom
        // Keep the point under the fingers in place, then follow them.
        let d = CGPoint(x: lastCentroid.x - videoView.center.x, y: lastCentroid.y - videoView.center.y)
        let offset = CGPoint(x: d.x - (d.x - zoomOffset.x) * k + c.x - lastCentroid.x,
                             y: d.y - (d.y - zoomOffset.y) * k + c.y - lastCentroid.y)
        setZoom(zoom * k, offset: offset)
        lastScale = g.scale
        lastCentroid = c
    }

    @objc private func rotation(_ g: UIRotationGestureRecognizer) {
        if zooming, !rotating { return }
        switch g.state {
        case .began:
            lastRotation = g.rotation
        case .changed:
            if !rotating {
                guard abs(g.rotation - lastRotation) > .pi / 15 else { return } // 12°
                rotating = true
                transformed = true
                stopMomentum()
                lastRotation = g.rotation
                core?.sendGesture(Gesture(kind: .rotate, phase: .began, value: 0))
                return
            }
            // UIKit turns clockwise, macOS counterclockwise.
            core?.sendGesture(Gesture(kind: .rotate, phase: .changed, value: -(g.rotation - lastRotation) * 180 / .pi))
            lastRotation = g.rotation
        default:
            if rotating { core?.sendGesture(Gesture(kind: .rotate, phase: .ended, value: 0)) }
            rotating = false
        }
    }

    /// Three fingers: up Mission Control, down App Exposé, left and right the next Space
    /// (the content follows the fingers, like on a Mac trackpad).
    @objc private func threeFingerSwipe(_ g: UIPanGestureRecognizer) {
        guard g.state == .ended else { return }
        let t = g.translation(in: videoView)
        guard max(abs(t.x), abs(t.y)) > 50 else { return }
        if abs(t.y) > abs(t.x) {
            if t.y < 0 { missionControl() } else { appExpose() }
        } else {
            switchSpace(left: t.x > 0)
        }
        UIImpactFeedbackGenerator(style: .light).impactOccurred()
    }

    private func missionControl() {
        core?.sendConsumer(usage: 0x29F, down: true) // AC Desktop Show All Windows
        core?.sendConsumer(usage: 0x29F, down: false)
    }

    private func appExpose() {
        press(HID.down, modifiers: [HID.control])
    }

    /// The Space to the left or right (⌃← / ⌃→, the Mac's default shortcuts).
    private func switchSpace(left: Bool) {
        press(left ? HID.left : HID.right, modifiers: [HID.control])
    }

    @objc private func tap(_ g: UITapGestureRecognizer) {
        if touchMode { point(at: g.location(in: videoView)) }
        click(0)
    }

    @objc private func twoFingerTap(_ g: UITapGestureRecognizer) {
        if touchMode { point(at: g.location(in: videoView)) }
        click(1)
    }

    @objc private func hold(_ g: UILongPressGestureRecognizer) {
        let p = g.location(in: videoView)
        if touchMode {
            if g.state == .began {
                point(at: p)
                click(1)
            }
            return
        }
        switch g.state {
        case .began:
            last = (p, CACurrentMediaTime())
            core?.sendButton(0, down: true)
            UIImpactFeedbackGenerator(style: .light).impactOccurred()
        case .changed:
            guard let previous = last else { return }
            last = (p, CACurrentMediaTime())
            move(by: CGPoint(x: p.x - previous.point.x, y: p.y - previous.point.y), over: 0, accelerate: false)
        default:
            last = nil
            core?.sendButton(0, down: false)
        }
    }

    private func click(_ button: Int) {
        core?.sendButton(button, down: true)
        core?.sendButton(button, down: false)
    }

    private func point(at p: CGPoint) {
        guard let f = fraction(at: p) else { return }
        core?.pointerAbs(x: f.x, y: f.y, display: model.selectedDisplay)
    }

    // MARK: Keys

    private func press(_ usage: UInt16, modifiers: [UInt16]) {
        for m in modifiers { core?.sendKey(hid: m, down: true) }
        core?.sendKey(hid: usage, down: true)
        core?.sendKey(hid: usage, down: false)
        for m in modifiers.reversed() { core?.sendKey(hid: m, down: false) }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !hardwareKey($0, down: true) }
        if !unhandled.isEmpty { super.pressesBegan(unhandled, with: event) }
    }

    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !hardwareKey($0, down: false) }
        if !unhandled.isEmpty { super.pressesEnded(unhandled, with: event) }
    }

    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        let unhandled = presses.filter { !hardwareKey($0, down: false) }
        if !unhandled.isEmpty { super.pressesCancelled(unhandled, with: event) }
    }

    /// A hardware keyboard key: UIKit already reports HID usages. UIKit doesn't repeat held
    /// keys, so repeat them here (the computer marks repeated downs as autorepeat).
    private func hardwareKey(_ press: UIPress, down: Bool) -> Bool {
        guard let key = press.key else { return false }
        let usage = UInt16(clamping: key.keyCode.rawValue)
        guard usage > 3 else { return false }
        if down {
            heldKeys.insert(usage)
            core?.sendKey(hid: usage, down: true)
            repeatTimer?.invalidate()
            repeatTimer = nil
            if !HID.isModifier(usage) {
                repeatTimer = Timer.scheduledTimer(withTimeInterval: 0.45, repeats: false) { [weak self] _ in
                    self?.repeatTimer = Timer.scheduledTimer(withTimeInterval: 1.0 / 30, repeats: true) { [weak self] _ in
                        guard let self, heldKeys.contains(usage) else { return }
                        core?.sendKey(hid: usage, down: true)
                    }
                }
            }
        } else {
            heldKeys.remove(usage)
            core?.sendKey(hid: usage, down: false)
            if !HID.isModifier(usage) {
                repeatTimer?.invalidate()
                repeatTimer = nil
            }
        }
        return true
    }

    private func releaseKeys() {
        repeatTimer?.invalidate()
        repeatTimer = nil
        for usage in heldKeys { core?.sendKey(hid: usage, down: false) }
        heldKeys.removeAll()
    }

    // MARK: Mouse (iPad)

    private func startMice() {
        GCMouse.mice().forEach(attach)
        mouseObservers = [
            NotificationCenter.default.addObserver(forName: .GCMouseDidConnect, object: nil, queue: .main) { [weak self] note in
                if let mouse = note.object as? GCMouse { self?.attach(mouse) }
            },
        ]
    }

    private func stopMice() {
        mouseObservers.forEach(NotificationCenter.default.removeObserver)
        mouseObservers = []
        for mouse in GCMouse.mice() {
            guard let input = mouse.mouseInput else { continue }
            input.mouseMovedHandler = nil
            input.leftButton.pressedChangedHandler = nil
            input.rightButton?.pressedChangedHandler = nil
            input.middleButton?.pressedChangedHandler = nil
            input.scroll.valueChangedHandler = nil
        }
    }

    private func attach(_ mouse: GCMouse) {
        guard let input = mouse.mouseInput else { return }
        input.mouseMovedHandler = { [weak self] _, dx, dy in
            guard let self, pointerLocked else { return }
            // Game controller deltas point up; screen coordinates point down.
            let x = CGFloat(dx) + mouseRemainder.x, y = -CGFloat(dy) + mouseRemainder.y
            let (ix, iy) = (x.rounded(.towardZero), y.rounded(.towardZero))
            mouseRemainder = CGPoint(x: x - ix, y: y - iy)
            if ix != 0 || iy != 0 { core?.sendMotion(dx: Int64(ix), dy: Int64(iy)) }
        }
        let button = { [weak self] (b: Int, pressed: Bool) in
            // A release always goes through, so nothing stays held if the lock ends mid-click.
            guard let self, pointerLocked || !pressed else { return }
            core?.sendButton(b, down: pressed)
        }
        input.leftButton.pressedChangedHandler = { _, _, pressed in button(0, pressed) }
        input.rightButton?.pressedChangedHandler = { _, _, pressed in button(1, pressed) }
        input.middleButton?.pressedChangedHandler = { _, _, pressed in button(2, pressed) }
        input.scroll.valueChangedHandler = { [weak self] _, x, y in
            guard let self, pointerLocked else { return }
            core?.sendMotion(dx: 0, dy: 0, scrollX: Int64((x * 4).rounded()), scrollY: Int64((y * 4).rounded()))
        }
    }
}

extension RemoteViewController: UIGestureRecognizerDelegate {
    func gestureRecognizer(_ g: UIGestureRecognizer, shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        together.contains(g) && together.contains(other)
    }
}

extension RemoteViewController: UIPointerInteractionDelegate {
    /// Over the picture, the computer's own cursor (in the stream) is the pointer.
    func pointerInteraction(_ interaction: UIPointerInteraction, styleFor region: UIPointerRegion) -> UIPointerStyle? {
        videoView.pictureRect.contains(region.rect.origin) ? .hidden() : nil
    }
}

/// The computer's screen, decoded on the GPU.
private final class VideoView: UIView {
    override class var layerClass: AnyClass { AVSampleBufferDisplayLayer.self }
    var displayLayer: AVSampleBufferDisplayLayer { layer as! AVSampleBufferDisplayLayer }
    var renderer: AVSampleBufferVideoRenderer { displayLayer.sampleBufferRenderer }
    /// The stream's size, to locate the aspect-fit picture.
    var videoSize = CGSize.zero

    override init(frame: CGRect) {
        super.init(frame: frame)
        displayLayer.videoGravity = .resizeAspect
        backgroundColor = .black
    }

    required init?(coder: NSCoder) { fatalError() }

    var pictureRect: CGRect {
        guard videoSize.width > 0, videoSize.height > 0 else { return .zero }
        return AVMakeRect(aspectRatio: videoSize, insideRect: bounds)
    }
}

private extension UIStackView {
    func addArrangedSubviews(_ views: [UIView]) {
        views.forEach(addArrangedSubview)
    }
}

/// Per-device preferences.
enum Prefs {
    private static let defaults = UserDefaults.standard

    static var screenOn: Bool {
        get { defaults.object(forKey: "screenOn") as? Bool ?? true }
        set { defaults.set(newValue, forKey: "screenOn") }
    }

    static var fitHost: Bool {
        get { defaults.bool(forKey: "fitHost") }
        set { defaults.set(newValue, forKey: "fitHost") }
    }

    static var touchMode: RemoteViewController.TouchMode {
        get { defaults.string(forKey: "touchMode").flatMap(RemoteViewController.TouchMode.init) ?? .trackpad }
        set { defaults.set(newValue.rawValue, forKey: "touchMode") }
    }

    /// Trackpad pointer speed multiplier.
    static var pointerSpeed: CGFloat {
        get { defaults.object(forKey: "pointerSpeed") as? CGFloat ?? 1.2 }
        set { defaults.set(newValue, forKey: "pointerSpeed") }
    }
}
