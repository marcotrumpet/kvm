import CoreMedia
import VideoToolbox

/// Turns ScreenHost's frame payloads into sample buffers for AVSampleBufferDisplayLayer
/// (shared by the Mac viewer and the iPhone/iPad app).
///
/// Payload: `codec << 7 | nalLengthSize: u8 | parameterSetCount: u8 | (len: u16 BE, bytes)* |
/// AVCC`, codec 0 H.264 (SPS, PPS) or 1 HEVC (VPS, SPS, PPS).
struct VideoFrames {
    /// Whether this device decodes HEVC in hardware (worth asking the host for it).
    static var decodesHEVC: Bool { VTIsHardwareDecodeSupported(kCMVideoCodecType_HEVC) }

    /// From the latest parameter sets; nil until the first keyframe.
    private(set) var format: CMVideoFormatDescription?

    /// Drops the format: nothing decodes until the next keyframe.
    mutating func reset() {
        format = nil
    }

    /// The sample for one frame, nil if it can't be decoded yet (or is malformed).
    /// `newFormat`: the frame carried new parameter sets (possibly a new size).
    mutating func sample(from frame: Data) -> (sample: CMSampleBuffer, newFormat: Bool)? {
        let bytes = [UInt8](frame)
        guard bytes.count >= 2 else { return nil }
        let hevc = bytes[0] & 0x80 != 0
        let nalLength = Int32(bytes[0] & 0x7F)
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
        var newFormat = false
        if !sets.isEmpty {
            var described: CMVideoFormatDescription?
            let status = VideoFrames.withPointers(sets) { pointers, sizes in
                hevc
                    ? CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                        allocator: nil, parameterSetCount: sets.count, parameterSetPointers: pointers,
                        parameterSetSizes: sizes, nalUnitHeaderLength: nalLength, extensions: nil,
                        formatDescriptionOut: &described)
                    : CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: nil, parameterSetCount: sets.count, parameterSetPointers: pointers,
                        parameterSetSizes: sizes, nalUnitHeaderLength: nalLength, formatDescriptionOut: &described)
            }
            if status == noErr, let described {
                format = described
                newFormat = true
            }
        }
        guard let format, let sample = VideoFrames.makeSample(Data(bytes[offset...]), format: format) else { return nil }
        return (sample, newFormat)
    }

    private static func withPointers<R>(_ sets: [[UInt8]], _ body: (UnsafePointer<UnsafePointer<UInt8>>, UnsafePointer<Int>) -> R) -> R {
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

    private static func makeSample(_ avcc: Data, format: CMVideoFormatDescription) -> CMSampleBuffer? {
        guard !avcc.isEmpty else { return nil }
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
}
