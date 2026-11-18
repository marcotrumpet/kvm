import UIKit

/// The clipboard payload shared with the computer (docs/protocol.md): `count: u16 BE`, then
/// per entry `type_len: u16 BE | MIME type | len: u32 BE | bytes`. Text and images only.
enum Clipboard {
    static func encodeGeneral() -> Data? {
        let board = UIPasteboard.general
        var entries: [(String, Data)] = []
        if board.hasStrings, let text = board.string {
            entries.append(("text/plain", Data(text.utf8)))
        }
        if board.hasImages, let png = board.image?.pngData() {
            entries.append(("image/png", png))
        }
        return entries.isEmpty ? nil : encode(entries)
    }

    /// Contents copied on the computer.
    static func write(_ payload: Data) {
        guard let entries = decode(payload) else { return }
        var item: [String: Any] = [:]
        for (mime, data) in entries {
            switch mime {
            case "text/plain": item["public.utf8-plain-text"] = String(decoding: data, as: UTF8.self)
            case "text/html": item["public.html"] = data
            case "image/png": item["public.png"] = data
            default: break
            }
        }
        if !item.isEmpty { UIPasteboard.general.items = [item] }
    }

    private static func encode(_ entries: [(String, Data)]) -> Data {
        var out = Data()
        func put16(_ v: Int) { out.append(contentsOf: [UInt8(v >> 8 & 0xFF), UInt8(v & 0xFF)]) }
        func put32(_ v: Int) { out.append(contentsOf: [24, 16, 8, 0].map { UInt8(v >> $0 & 0xFF) }) }
        put16(entries.count)
        for (mime, data) in entries {
            let type = Data(mime.utf8)
            put16(type.count)
            out.append(type)
            put32(data.count)
            out.append(data)
        }
        return out
    }

    private static func decode(_ payload: Data) -> [(String, Data)]? {
        let bytes = [UInt8](payload)
        var offset = 0
        func take(_ n: Int) -> ArraySlice<UInt8>? {
            guard offset + n <= bytes.count else { return nil }
            defer { offset += n }
            return bytes[offset..<offset + n]
        }
        func int(_ n: Int) -> Int? { take(n).map { $0.reduce(0) { $0 << 8 | Int($1) } } }
        guard let count = int(2) else { return nil }
        var entries: [(String, Data)] = []
        for _ in 0..<count {
            guard let typeLen = int(2), let type = take(typeLen), let len = int(4), let data = take(len) else { return nil }
            entries.append((String(decoding: type, as: UTF8.self), Data(data)))
        }
        return entries
    }
}
