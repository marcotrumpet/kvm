import CoreMedia
import os
import ScreenCaptureKit
import VideoToolbox

/// Shares this Mac's main display with the peer: ScreenCaptureKit capture, hardware H.264
/// encoding in low-latency mode, frames handed to the core.
///
/// Frame payload: `nalLengthSize: u8 | parameterSetCount: u8 | (len: u16 BE, bytes)* | AVCC`.
/// Keyframes carry the SPS/PPS, so the viewer can (re)start decoding at any keyframe.
/// State is confined to `queue` or the main thread, as marked; hence `@unchecked Sendable`.
final class ScreenHost: NSObject, SCStreamOutput, SCStreamDelegate, @unchecked Sendable {
    private let core: Core
    private let queue = DispatchQueue(label: "kvm.screen-host", qos: .userInteractive)
    private var stream: SCStream?
    /// The display being captured, and what the viewer last asked for. Main thread.
    private var capturedDisplay: CGDirectDisplayID = 0
    private var lastParams: VideoParams?
    private var encoder: VTCompressionSession?
    /// The newest captured frame, re-encoded when a keyframe is needed while the screen is
    /// idle (ScreenCaptureKit only delivers frames when something changes).
    private var lastFrame: CVPixelBuffer?
    private let forceKeyframe = OSAllocatedUnfairLock(initialState: true)
    private var starting = false
    // Adaptive quality, confined to `queue`. `fullSize` is the largest size the viewer
    // accepts; `level` the rung of the quality ladder currently streamed.
    private struct Level: Equatable {
        let scale: Double
        let fps: Int
    }
    /// The captured display's native pixel size.
    private var nativeSize = CGSize.zero
    private var fullSize = CGSize.zero
    private var ladder: [Level] = []
    private var level = 0
    private var targetKbps = 12_000
    private var encoderSize = CGSize.zero
    private var fps: Int { ladder.isEmpty ? 60 : ladder[level].fps }
    /// Bits per pixel per frame below which a rung looks bad (step down), and the headroom
    /// needed to step back up. Low, because a desktop is mostly static and H.264 spends
    /// almost nothing on unchanged areas.
    private static let minBitsPerPixel = 0.012
    private static let upBitsPerPixel = 0.02
    /// Main thread: whether the screen is being shared.
    var onSharingChanged: ((Bool) -> Void)?

    init(core: Core) {
        self.core = core
    }

    var isSharing: Bool { stream != nil }

    /// Main thread.
    func start(_ params: VideoParams) {
        lastParams = params
        if stream != nil {
            // Already streaming: the viewer changed its settings, window size or display.
            let display = target(for: params)
            if display != capturedDisplay {
                switchDisplay(to: display, params: params)
            } else {
                let fitted = ScreenHost.fittedSize(of: display)
                queue.async { [self] in
                    if let fitted { nativeSize = fitted }
                    reconfigure(params)
                }
            }
            return
        }
        guard !starting else { return }
        guard Permissions.screenRecording else {
            Log.warn("screen sharing requested but Screen Recording is not granted")
            Permissions.requestScreenRecording()
            return
        }
        starting = true
        Task { @MainActor in
            defer { starting = false }
            do {
                try await begin(params)
                onSharingChanged?(true)
            } catch {
                Log.warn("starting screen capture: \(error)")
                stop()
            }
        }
    }

    /// Main thread.
    func stop() {
        let stream = self.stream
        self.stream = nil
        capturedDisplay = 0
        VirtualDisplay.shared.remove(owner: self)
        stream?.stopCapture { _ in }
        queue.async { [self] in
            if let encoder { VTCompressionSessionInvalidate(encoder) }
            encoder = nil
            lastFrame = nil
            fullSize = .zero
            nativeSize = .zero
            level = 0
        }
        if stream != nil {
            Log.info("screen sharing stopped")
            onSharingChanged?(false)
        }
    }

    func requestKeyframe() {
        forceKeyframe.withLock { $0 = true }
        queue.async { [self] in
            if let frame = lastFrame { encode(frame, at: CMClockGetTime(CMClockGetHostTimeClock())) }
        }
    }

    /// Main thread: the viewer picked another display; capture it without restarting.
    private func switchDisplay(to id: CGDirectDisplayID, params: VideoParams) {
        Task { @MainActor in
            do {
                let display = try await ScreenHost.scDisplay(id)
                try await stream?.updateContentFilter(SCContentFilter(display: display, excludingWindows: []))
                capturedDisplay = display.displayID
                // Capture has moved off a virtual display we let go: unplug it now.
                if VirtualDisplay.shared.released, display.displayID != VirtualDisplay.shared.id {
                    VirtualDisplay.shared.remove(owner: self)
                }
                let native = ScreenHost.nativeSize(of: display)
                queue.async { [self] in
                    nativeSize = native
                    reconfigure(params)
                }
                Log.info("screen sharing display -> \(display.displayID)")
            } catch {
                Log.warn("switching display: \(error)")
            }
        }
    }

    /// Main thread: displays changed; if ours went away, fall back to the main display.
    func displaysChanged() {
        guard stream != nil, let params = lastParams else { return }
        let wanted = target(for: params)
        if wanted != capturedDisplay {
            switchDisplay(to: wanted, params: params)
        }
    }

    /// Main thread: the display to capture for `params`. With `fitHost`, a virtual display
    /// at the viewer's size or shape that the main display mirrors. Otherwise ours (if any)
    /// goes: right away, or once capture has moved off it (removing a display while it is
    /// captured stops the capture).
    private func target(for params: VideoParams) -> CGDirectDisplayID {
        let virtual = VirtualDisplay.shared
        if params.fitHost != .off, params.display == 0,
           let id = virtual.fit(to: CGSize(width: params.maxWidth, height: params.maxHeight),
                                shapeOnly: params.fitHost == .shape, owner: self) {
            return id
        }
        guard stream != nil, capturedDisplay == virtual.id, let physical = virtual.release(owner: self) else {
            virtual.remove(owner: self)
            return Displays.resolve(params.display)
        }
        return params.display == 0 ? physical : Displays.resolve(params.display)
    }

    /// Main thread: the size the virtual display was set to, if `id` is it. Its display
    /// mode can lag behind a resize.
    private static func fittedSize(of id: CGDirectDisplayID) -> CGSize? {
        id == VirtualDisplay.shared.id ? VirtualDisplay.shared.pixelSize : nil
    }

    private static func scDisplay(_ id: CGDirectDisplayID) async throws -> SCDisplay {
        let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        guard let display = content.displays.first(where: { $0.displayID == id })
            ?? content.displays.first(where: { $0.displayID == CGMainDisplayID() })
            ?? content.displays.first else {
            throw CoreError(description: "no display to capture")
        }
        return display
    }

    private static func nativeSize(of display: SCDisplay) -> CGSize {
        if let fitted = fittedSize(of: display.displayID) { return fitted }
        let mode = CGDisplayCopyDisplayMode(display.displayID)
        return CGSize(width: Double(mode?.pixelWidth ?? display.width), height: Double(mode?.pixelHeight ?? display.height))
    }

    @MainActor
    private func begin(_ params: VideoParams) async throws {
        let display = try await ScreenHost.scDisplay(target(for: params))
        capturedDisplay = display.displayID
        // Native pixel size, scaled down to fit what the viewer asked for.
        let native = ScreenHost.nativeSize(of: display)
        let full = ScreenHost.fit(native, params)
        let start = queue.sync { () -> Level in
            nativeSize = native
            fullSize = full
            ladder = ScreenHost.ladder(maxFps: max(params.fps, 1), prefer: params.prefer)
            level = ScreenHost.pick(kbps: targetKbps, full: full, ladder: ladder, current: 0)
            return ladder[level]
        }
        let width = Int(full.width * start.scale) & ~1
        let height = Int(full.height * start.scale) & ~1
        let config = ScreenHost.configuration(width: width, height: height, fps: start.fps)

        let encoder = try queue.sync { try makeEncoder(width: width, height: height) }
        queue.sync {
            self.encoder = encoder
            self.encoderSize = CGSize(width: width, height: height)
        }
        forceKeyframe.withLock { $0 = true }

        let stream = SCStream(filter: SCContentFilter(display: display, excludingWindows: []),
                              configuration: config, delegate: self)
        try stream.addStreamOutput(self, type: .screen, sampleHandlerQueue: queue)
        try await stream.startCapture()
        self.stream = stream
        Log.info("screen sharing started: \(width)x\(height) @ \(start.fps) fps, \(targetKbps) kbit/s (up to \(params.bitrateKbps))")
    }

    /// Any thread: the core adapted the bitrate to the link.
    func setBitrate(_ kbps: Int) {
        queue.async { [self] in
            targetKbps = kbps
            if let encoder { applyBitrate(encoder) }
            guard fullSize != .zero else { return } // not started yet: used when it starts
            let next = ScreenHost.pick(kbps: kbps, full: fullSize, ladder: ladder, current: level)
            if next != level { change(to: next) }
        }
    }

    /// The display's size scaled down to fit the viewer's maximum, never up.
    private static func fit(_ native: CGSize, _ params: VideoParams) -> CGSize {
        let scale = min(1, Double(params.maxWidth) / native.width, Double(params.maxHeight) / native.height)
        return CGSize(width: max(native.width * scale, 320), height: max(native.height * scale, 200))
    }

    /// Rungs from best to most economical. Balanced halves the frame rate before giving up
    /// resolution, so text stays sharp; the other preferences protect one side further.
    private static func ladder(maxFps: Int, prefer: VideoPreference) -> [Level] {
        let low = min(maxFps, 30)
        let rungs: [Level] = switch prefer {
        case .balanced:
            [Level(scale: 1, fps: maxFps), Level(scale: 1, fps: low), Level(scale: 0.75, fps: low), Level(scale: 0.5, fps: low)]
        case .resolution:
            [Level(scale: 1, fps: maxFps), Level(scale: 1, fps: min(maxFps, 20)), Level(scale: 1, fps: min(maxFps, 15)),
             Level(scale: 0.75, fps: min(maxFps, 15))]
        case .frameRate:
            [Level(scale: 1, fps: maxFps), Level(scale: 0.75, fps: maxFps), Level(scale: 0.5, fps: maxFps), Level(scale: 0.5, fps: low)]
        }
        return rungs.reduce(into: []) { out, rung in if out.last != rung { out.append(rung) } }
    }

    /// On `queue`: new viewer settings while streaming.
    private func reconfigure(_ params: VideoParams) {
        guard nativeSize != .zero else { return }
        fullSize = ScreenHost.fit(nativeSize, params)
        ladder = ScreenHost.ladder(maxFps: max(params.fps, 1), prefer: params.prefer)
        let next = ScreenHost.pick(kbps: targetKbps, full: fullSize, ladder: ladder, current: 0)
        Log.info("viewer settings: up to \(params.maxWidth)x\(params.maxHeight) @ \(params.fps) fps, \(params.bitrateKbps) kbit/s, prefer \(params.prefer)")
        change(to: next)
        forceKeyframe.withLock { $0 = true }
    }

    /// The best rung the bitrate can sustain, with hysteresis between rungs.
    private static func pick(kbps: Int, full: CGSize, ladder: [Level], current: Int) -> Int {
        for (i, rung) in ladder.enumerated() {
            let bpp = Double(kbps) * 1000 / Double(rung.fps) / (full.width * rung.scale * full.height * rung.scale)
            let needed = i < current ? upBitsPerPixel : minBitsPerPixel
            if bpp >= needed || i == ladder.count - 1 { return i }
        }
        return current
    }

    private static func configuration(width: Int, height: Int, fps: Int) -> SCStreamConfiguration {
        let config = SCStreamConfiguration()
        config.width = width
        config.height = height
        config.minimumFrameInterval = CMTime(value: 1, timescale: CMTimeScale(fps))
        config.pixelFormat = kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
        config.showsCursor = true
        config.queueDepth = 4
        return config
    }

    /// On `queue`: switch rungs. A new size needs a new encoder (and a keyframe); a new
    /// frame rate only a capture and encoder setting.
    private func change(to next: Int) {
        let rung = ladder[next]
        let width = Int(fullSize.width * rung.scale) & ~1
        let height = Int(fullSize.height * rung.scale) & ~1
        let resize = CGSize(width: width, height: height) != encoderSize
        level = next
        if resize {
            guard let newEncoder = try? makeEncoder(width: width, height: height) else { return }
            if let encoder { VTCompressionSessionInvalidate(encoder) }
            encoder = newEncoder
            encoderSize = CGSize(width: width, height: height)
            lastFrame = nil
            forceKeyframe.withLock { $0 = true }
        } else if let encoder {
            VTSessionSetProperty(encoder, key: kVTCompressionPropertyKey_ExpectedFrameRate, value: rung.fps as CFNumber)
        }
        Log.info("screen sharing quality -> \(width)x\(height) @ \(rung.fps) fps for \(targetKbps) kbit/s")
        let config = ScreenHost.configuration(width: width, height: height, fps: rung.fps)
        DispatchQueue.main.async { [self] in
            stream?.updateConfiguration(config) { error in
                if let error { Log.warn("changing capture settings: \(error)") }
            }
        }
    }

    /// On `queue`. The cap keeps bursts (keyframes) from overshooting the link by much.
    private func applyBitrate(_ session: VTCompressionSession) {
        let bps = targetKbps * 1000
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_AverageBitRate, value: bps as CFNumber)
        let limits = [bps / 8 * 3 / 2, 1] as CFArray // bytes per second, measured over 1 s
        VTSessionSetProperty(session, key: kVTCompressionPropertyKey_DataRateLimits, value: limits)
    }

    /// On `queue`.
    private func makeEncoder(width: Int, height: Int) throws -> VTCompressionSession {
        let spec = [kVTVideoEncoderSpecification_EnableLowLatencyRateControl: kCFBooleanTrue] as CFDictionary
        var session: VTCompressionSession?
        let status = VTCompressionSessionCreate(
            allocator: nil, width: Int32(width), height: Int32(height),
            codecType: kCMVideoCodecType_H264, encoderSpecification: spec,
            imageBufferAttributes: nil, compressedDataAllocator: nil,
            outputCallback: nil, refcon: nil, compressionSessionOut: &session)
        guard status == noErr, let session else {
            throw CoreError(description: "creating the H.264 encoder failed (\(status))")
        }
        let props: [CFString: Any] = [
            kVTCompressionPropertyKey_RealTime: true,
            kVTCompressionPropertyKey_AllowFrameReordering: false,
            kVTCompressionPropertyKey_ExpectedFrameRate: fps,
            // Keyframes are sent on demand (start, loss, backpressure), so allow long gaps.
            kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: 30,
        ]
        for (key, value) in props {
            VTSessionSetProperty(session, key: key, value: value as CFTypeRef)
        }
        applyBitrate(session)
        VTCompressionSessionPrepareToEncodeFrames(session)
        return session
    }

    // MARK: Capture (on `queue`)

    func stream(_ stream: SCStream, didOutputSampleBuffer sampleBuffer: CMSampleBuffer, of type: SCStreamOutputType) {
        guard type == .screen, sampleBuffer.isValid, let frame = sampleBuffer.imageBuffer else { return }
        // Skip "idle"/"blank" status updates that carry no new content.
        if let info = (CMSampleBufferGetSampleAttachmentsArray(sampleBuffer, createIfNecessary: false) as? [[SCStreamFrameInfo: Any]])?.first,
           let raw = info[.status] as? Int, SCFrameStatus(rawValue: raw) != .complete {
            return
        }
        lastFrame = frame
        encode(frame, at: sampleBuffer.presentationTimeStamp)
    }

    func stream(_ stream: SCStream, didStopWithError error: Error) {
        Log.warn("screen capture stopped: \(error)")
        DispatchQueue.main.async { self.stop() }
    }

    private func encode(_ frame: CVPixelBuffer, at time: CMTime) {
        guard let encoder else { return }
        // Link busy: skip this frame before encoding, so the next can still build on the
        // last one sent (an encoded frame refused later would force a large keyframe).
        guard core.videoCanSend() else { return }
        // During a resolution change, frames of the old size are still in flight.
        guard CVPixelBufferGetWidth(frame) == Int(encoderSize.width),
              CVPixelBufferGetHeight(frame) == Int(encoderSize.height) else { return }
        let key = forceKeyframe.withLock { k -> Bool in
            defer { k = false }
            return k
        }
        let options = key ? [kVTEncodeFrameOptionKey_ForceKeyFrame: kCFBooleanTrue] as CFDictionary : nil
        VTCompressionSessionEncodeFrame(
            encoder, imageBuffer: frame, presentationTimeStamp: time, duration: .invalid,
            frameProperties: options, infoFlagsOut: nil
        ) { [weak self] status, _, sample in
            guard status == noErr, let sample, let self else { return }
            self.send(sample)
        }
    }

    private func send(_ sample: CMSampleBuffer) {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, createIfNecessary: false) as? [[CFString: Any]]
        let keyframe = !(attachments?.first?[kCMSampleAttachmentKey_NotSync] as? Bool ?? false)
        guard let block = sample.dataBuffer, let format = sample.formatDescription else { return }

        var payload = Data()
        var nalLength: Int32 = 4
        var parameterSets: [Data] = []
        if keyframe {
            var count = 0
            CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                format, parameterSetIndex: 0, parameterSetPointerOut: nil, parameterSetSizeOut: nil,
                parameterSetCountOut: &count, nalUnitHeaderLengthOut: &nalLength)
            for i in 0..<count {
                var pointer: UnsafePointer<UInt8>?
                var size = 0
                if CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
                    format, parameterSetIndex: i, parameterSetPointerOut: &pointer, parameterSetSizeOut: &size,
                    parameterSetCountOut: nil, nalUnitHeaderLengthOut: nil) == noErr, let pointer {
                    parameterSets.append(Data(bytes: pointer, count: size))
                }
            }
        }
        payload.append(UInt8(nalLength))
        payload.append(UInt8(parameterSets.count))
        for set in parameterSets {
            payload.append(UInt8(set.count >> 8))
            payload.append(UInt8(set.count & 0xFF))
            payload.append(set)
        }
        let length = CMBlockBufferGetDataLength(block)
        var bytes = Data(count: length)
        bytes.withUnsafeMutableBytes { raw in
            _ = CMBlockBufferCopyDataBytes(block, atOffset: 0, dataLength: length, destination: raw.baseAddress!)
        }
        payload.append(bytes)

        if !core.sendVideoFrame(payload, keyframe: keyframe) {
            // Refused (link behind): the next frame must not depend on this one.
            forceKeyframe.withLock { $0 = true }
        }
    }
}
