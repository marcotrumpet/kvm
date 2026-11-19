import AVFoundation
import os

/// The host's sound, sent next to its screen as AAC-ELD (low delay, built into every Apple
/// device), one packet per datagram (shared by the Mac and the iPhone/iPad app).
///
/// Payload: `kind: u8 | bytes`. Kind 0 carries the encoder's magic cookie (sent before the
/// first packet and every couple of seconds, so a viewer can start or recover at any
/// time); kind 1 one encoded packet. Lost packets are just skipped.
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

    static let cookieKind: UInt8 = 0
    static let packetKind: UInt8 = 1
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
        guard let converter else { return }
        let frames = AVAudioFrameCount(CMSampleBufferGetNumSamples(sample))
        guard frames > 0, let pcm = AVAudioPCMBuffer(pcmFormat: format, frameCapacity: frames) else { return }
        pcm.frameLength = frames
        guard CMSampleBufferCopyPCMDataIntoAudioBufferList(
            sample, at: 0, frameCount: Int32(frames), into: pcm.mutableAudioBufferList) == noErr else { return }

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
final class AudioPlayer: @unchecked Sendable {
    private let queue = DispatchQueue(label: "kvm.audio-player", qos: .userInteractive)
    // Confined to `queue`.
    private let engine = AVAudioEngine()
    private let node = AVAudioPlayerNode()
    private var decoder: AVAudioConverter?
    private var cookie: Data?
    private var playing = false
    /// Frames scheduled and not yet played.
    private let queued = OSAllocatedUnfairLock(initialState: 0)
    /// Start once this much is queued; drop packets beyond the maximum.
    private static let startFrames = Int(AudioStream.sampleRate * 0.04)
    private static let maxFrames = Int(AudioStream.sampleRate * 0.15)
    private var observer: NSObjectProtocol?

    init() {
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
        guard kind == AudioStream.packetKind, let decoder, !body.isEmpty else { return }
        guard queued.withLock({ $0 }) < AudioPlayer.maxFrames else { return } // running late: skip
        guard let pcm = decode(Data(body), with: decoder) else { return }
        schedule(pcm)
    }

    private func decode(_ packet: Data, with decoder: AVAudioConverter) -> AVAudioPCMBuffer? {
        let input = AVAudioCompressedBuffer(format: AudioStream.compressed, packetCapacity: 1,
                                            maximumPacketSize: packet.count)
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
            #if os(iOS)
            try? AVAudioSession.sharedInstance().setCategory(.playback, options: [.mixWithOthers])
            try? AVAudioSession.sharedInstance().setActive(true)
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
