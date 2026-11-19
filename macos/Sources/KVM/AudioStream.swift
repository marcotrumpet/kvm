import AVFoundation
import os

/// The host's sound, sent next to its screen as AAC-ELD (low delay, built into every Apple
/// device), one packet per datagram (shared by the Mac and the iPhone/iPad app).
///
/// Payload: `kind: u8 | bytes`. Kind 0 carries the encoder's magic cookie (sent before the
/// first packet and every couple of seconds, so a viewer can start or recover at any
/// time); kind 1 one encoded packet. Lost packets are just skipped. Kind 2 is one Opus
/// packet (48 kHz stereo, 10 ms), from senders without AAC-ELD: the Windows and Linux apps'
/// microphones.
enum AudioStream {
    static let sampleRate = 48_000.0
    static let channels: AVAudioChannelCount = 2
    static let bitrate = 128_000

    static let pcm = AVAudioFormat(standardFormatWithSampleRate: sampleRate, channels: channels)!

    static let compressed: AVAudioFormat = {
        var asbd = AudioStreamBasicDescription()
        asbd.mSampleRate = sampleRate
        asbd.mFormatID = kAudioFormatMPEG4AAC_ELD
        asbd.mChannelsPerFrame = channels
        var size = UInt32(MemoryLayout<AudioStreamBasicDescription>.size)
        AudioFormatGetProperty(kAudioFormatProperty_FormatInfo, 0, nil, &size, &asbd) // fills in the rest
        return AVAudioFormat(streamDescription: &asbd)!
    }()

    static let opus: AVAudioFormat = {
        var asbd = AudioStreamBasicDescription()
        asbd.mSampleRate = sampleRate
        asbd.mFormatID = kAudioFormatOpus
        asbd.mChannelsPerFrame = channels
        asbd.mFramesPerPacket = 480
        return AVAudioFormat(streamDescription: &asbd)!
    }()

    static let cookieKind: UInt8 = 0
    static let packetKind: UInt8 = 1
    static let opusKind: UInt8 = 2
}

/// Host side: captured sample buffers in, payloads out. Use from one queue.
final class AudioEncoder {
    var onPayload: ((Data) -> Void)?
    private var converter: AVAudioConverter?
    private var inputFormat: AVAudioFormat?
    private var packets = 0
    /// How often the cookie is repeated, in packets (about two seconds).
    private static let cookieEvery = 200

    func reset() {
        converter = nil
        inputFormat = nil
        packets = 0
    }

    func encode(_ sample: CMSampleBuffer) {
        guard let description = sample.formatDescription else { return }
        let format = AVAudioFormat(cmAudioFormatDescription: description)
        let frames = AVAudioFrameCount(CMSampleBufferGetNumSamples(sample))
        guard frames > 0, let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { return }
        pcm.frameLength = frames
        guard CMSampleBufferCopyPCMDataIntoAudioBufferList(
            sample, at: 0, frameCount: Int32(frames), into: pcm.mutableAudioBufferList) == noErr else { return }
        encode(pcm)
    }

    func encode(_ pcm: AVAudioPCMBuffer) {
        let format = pcm.format
        if converter == nil || format != inputFormat {
            guard let made = AVAudioConverter(from: format, to: AudioStream.compressed) else {
                Log.warn("creating the audio encoder failed")
                return
            }
            made.bitRate = AudioStream.bitrate
            converter = made
            inputFormat = format
            packets = 0
        }
        guard let converter, pcm.frameLength > 0 else { return }

        var fed = false
        while true {
            let out = AVAudioCompressedBuffer(format: AudioStream.compressed, packetCapacity: 8,
                                              maximumPacketSize: max(converter.maximumOutputPacketSize, 1))
            var error: NSError?
            let status = converter.convert(to: out, error: &error) { _, input in
                if fed {
                    input.pointee = .noDataNow // keep the converter's leftovers for the next call
                    return nil
                }
                fed = true
                input.pointee = .haveData
                return pcm
            }
            emit(out, cookie: converter.magicCookie)
            guard status == .haveData else { break }
        }
    }

    private func emit(_ buffer: AVAudioCompressedBuffer, cookie: Data?) {
        guard let descriptions = buffer.packetDescriptions else { return }
        for i in 0..<Int(buffer.packetCount) {
            if packets % AudioEncoder.cookieEvery == 0, let cookie {
                onPayload?(Data([AudioStream.cookieKind]) + cookie)
            }
            packets += 1
            let d = descriptions[i]
            var payload = Data([AudioStream.packetKind])
            payload.append(buffer.data.advanced(by: Int(d.mStartOffset)).assumingMemoryBound(to: UInt8.self),
                           count: Int(d.mDataByteSize))
            onPayload?(payload)
        }
    }
}

/// Viewer side: payloads in, sound out. A small jitter buffer; packets that arrive while
/// too much is queued are dropped, so delay can't build up. Any thread.
/// `outputDevice` (macOS): play there instead of the default output, and not at all while
/// it returns nil (the microphone must never come out of the speakers).
final class AudioPlayer: @unchecked Sendable {
    private let queue = DispatchQueue(label: "kvm.audio-player", qos: .userInteractive)
    // Confined to `queue`.
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private var decoder: AVAudioConverter?
    private var cookie: Data?
    private var opusDecoder: AVAudioConverter?
    private var playing = false
    /// Frames scheduled and not yet played.
    private let queued = OSAllocatedUnfairLock(initialState: 0)
    /// Start once this much is queued; drop packets beyond the maximum.
    private static let startFrames = Int(AudioStream.sampleRate * 0.04)
    private static let maxFrames = Int(AudioStream.sampleRate * 0.15)
    private var observer: NSObjectProtocol?
    private let outputDevice: (() -> UInt32?)?

    init(outputDevice: (() -> UInt32?)? = nil) {
        self.outputDevice = outputDevice
        engine.attach(node)
        engine.connect(node, to: engine.mainMixerNode, format: AudioStream.pcm)
        // Another output device (headphones, AirPlay) stops the engine: start over.
        observer = NotificationCenter.default.addObserver(
            forName: .AVAudioEngineConfigurationChange, object: engine, queue: nil
        ) { [weak self] _ in self?.queue.async { self?.halt() } }
    }

    deinit {
        if let observer { NotificationCenter.default.removeObserver(observer) }
    }

    func handle(_ payload: Data) {
        queue.async { [self] in receive(payload) }
    }

    /// Silence until the next payload.
    func stop() {
        queue.async { [self] in
            halt()
            decoder = nil
            cookie = nil
            opusDecoder = nil
        }
    }

    private func halt() {
        node.stop()
        engine.stop()
        playing = false
        queued.withLock { $0 = 0 }
    }

    private func receive(_ payload: Data) {
        guard let kind = payload.first else { return }
        let body = payload.dropFirst()
        if kind == AudioStream.cookieKind {
            guard body != cookie else { return }
            guard let made = AVAudioConverter(from: AudioStream.compressed, to: AudioStream.pcm) else { return }
            made.magicCookie = Data(body)
            decoder = made
            cookie = Data(body)
            return
        }
        if kind == AudioStream.opusKind, opusDecoder == nil {
            opusDecoder = AVAudioConverter(from: AudioStream.opus, to: AudioStream.pcm)
        }
        let (format, converter) = kind == AudioStream.opusKind
            ? (AudioStream.opus, opusDecoder)
            : (AudioStream.compressed, kind == AudioStream.packetKind ? decoder : nil)
        guard let converter, !body.isEmpty else { return }
        guard queued.withLock({ $0 }) < AudioPlayer.maxFrames else { return } // running late: skip
        guard let pcm = decode(Data(body), format: format, with: converter) else { return }
        schedule(pcm)
    }

    private func decode(_ packet: Data, format: AVAudioFormat, with decoder: AVAudioConverter) -> AVAudioPCMBuffer? {
        let input = AVAudioCompressedBuffer(format: format, packetCapacity: 1, maximumPacketSize: packet.count)
        packet.withUnsafeBytes { raw in input.data.copyMemory(from: raw.baseAddress!, byteCount: packet.count) }
        input.byteLength = UInt32(packet.count)
        input.packetCount = 1
        input.packetDescriptions?[0] = AudioStreamPacketDescription(
            mStartOffset: 0, mVariableFramesInPacket: 0, mDataByteSize: UInt32(packet.count))
        guard let out = AVAudioPCMBuffer(pcmFormat: AudioStream.pcm, frameCapacity: 2048) else { return nil }
        var fed = false
        var error: NSError?
        _ = decoder.convert(to: out, error: &error) { _, status in
            if fed {
                status.pointee = .noDataNow
                return nil
            }
            fed = true
            status.pointee = .haveData
            return input
        }
        return out.frameLength > 0 ? out : nil
    }

    private func schedule(_ pcm: AVAudioPCMBuffer) {
        if !engine.isRunning {
            #if os(macOS)
            if let outputDevice {
                guard var device = outputDevice(), let unit = engine.outputNode.audioUnit,
                      AudioUnitSetProperty(unit, kAudioOutputUnitProperty_CurrentDevice, kAudioUnitScope_Global, 0,
                                           &device, UInt32(MemoryLayout<UInt32>.size)) == noErr else { return }
            }
            #endif
            #if os(iOS)
            let session = AVAudioSession.sharedInstance()
            if session.category != .playAndRecord { // the microphone set it up for both
                try? session.setCategory(.playback, options: [.mixWithOthers])
            }
            try? session.setActive(true)
            #endif
            do {
                try engine.start()
            } catch {
                Log.warn("starting audio output: \(error)")
                return
            }
        }
        let frames = Int(pcm.frameLength)
        let total = queued.withLock { q -> Int in
            q += frames
            return q
        }
        node.scheduleBuffer(pcm) { [queued] in queued.withLock { $0 = max($0 - frames, 0) } }
        if !playing, total >= AudioPlayer.startFrames {
            node.play()
            playing = true
        }
    }
}

/// Controller side: this device's microphone in, payloads out (same format as the host's
/// sound), while `start`ed. The microphone is mono; both channels carry it. Main thread.
final class MicrophoneSender {
    var onPayload: ((Data) -> Void)?
    private var engine: AVAudioEngine?
    private let queue = DispatchQueue(label: "kvm.microphone", qos: .userInteractive)
    private let encoder = AudioEncoder()

    var isRunning: Bool { engine != nil }

    /// Asks for microphone access the first time; false if it is denied or fails.
    func start(completion: @escaping (Bool) -> Void) {
        guard engine == nil else { return completion(true) }
        Self.requestAccess { [weak self] granted in
            guard let self, granted else { return completion(false) }
            completion(begin())
        }
    }

    func stop() {
        guard let engine else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        self.engine = nil
        queue.async { [encoder] in encoder.reset() }
        Log.info("microphone off")
    }

    private func begin() -> Bool {
        #if os(iOS)
        let session = AVAudioSession.sharedInstance()
        do {
            try session.setCategory(.playAndRecord, mode: .default,
                                    options: [.mixWithOthers, .defaultToSpeaker, .allowBluetoothHFP])
            try session.setActive(true)
        } catch {
            Log.warn("microphone audio session: \(error)")
            return false
        }
        #endif
        let engine = AVAudioEngine()
        let input = engine.inputNode
        let format = input.outputFormat(forBus: 0)
        guard format.sampleRate > 0, format.channelCount > 0,
              let stereo = AVAudioFormat(standardFormatWithSampleRate: format.sampleRate, channels: 2) else {
            Log.warn("no microphone input")
            return false
        }
        encoder.onPayload = { [weak self] in self?.onPayload?($0) }
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [queue, encoder] buffer, _ in
            guard let both = MicrophoneSender.stereo(buffer, as: stereo) else { return }
            queue.async { encoder.encode(both) }
        }
        do {
            try engine.start()
        } catch {
            input.removeTap(onBus: 0)
            Log.warn("starting the microphone: \(error)")
            return false
        }
        self.engine = engine
        Log.info("microphone on (\(Int(format.sampleRate)) Hz)")
        return true
    }

    /// The first input channel, on both channels of a new buffer.
    private static func stereo(_ buffer: AVAudioPCMBuffer, as format: AVAudioFormat) -> AVAudioPCMBuffer? {
        guard let source = buffer.floatChannelData, buffer.frameLength > 0,
              let out = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: buffer.frameLength),
              let channels = out.floatChannelData else { return nil }
        out.frameLength = buffer.frameLength
        let count = Int(buffer.frameLength)
        channels[0].update(from: source[0], count: count)
        channels[1].update(from: source[0], count: count)
        return out
    }

    private static func requestAccess(_ done: @escaping (Bool) -> Void) {
        switch AVCaptureDevice.authorizationStatus(for: .audio) {
        case .authorized: done(true)
        case .notDetermined:
            AVCaptureDevice.requestAccess(for: .audio) { granted in DispatchQueue.main.async { done(granted) } }
        default: done(false)
        }
    }
}
