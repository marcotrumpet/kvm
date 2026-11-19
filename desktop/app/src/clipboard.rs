//! Clipboard sharing, in the apps' payload format (docs/protocol.md): `count: u16 BE`, then
//! per entry `type_len: u16 BE | MIME type | len: u32 BE | bytes`. Used here: `text/plain`,
//! and copied files (`application/x-kvm-files`: JSON `{"transfer", "names"}`, the files
//! themselves sent ahead into the receiver's clipboard cache). The Mac's rich text and
//! images are skipped.

pub const FILES: &str = "application/x-kvm-files";
pub const FILES_TOO_LARGE: &str = "application/x-kvm-files-too-large";

pub fn encode(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = (entries.len() as u16).to_be_bytes().to_vec();
    for (mime, bytes) in entries {
        out.extend_from_slice(&(mime.len() as u16).to_be_bytes());
        out.extend_from_slice(mime.as_bytes());
        out.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        out.extend_from_slice(bytes);
    }
    out
}

pub fn encode_text(text: &str) -> Vec<u8> {
    encode(&[("text/plain", text.as_bytes())])
}

/// Copied files: the transfer carrying them and their top-level names.
pub fn encode_files(transfer: u64, names: &[String]) -> Vec<u8> {
    let json = serde_json::json!({ "transfer": transfer.to_string(), "names": names }).to_string();
    let text = names.join("\n");
    encode(&[(FILES, json.as_bytes()), ("text/plain", text.as_bytes())])
}

/// Copied files over the size limit: named, not sent.
pub fn encode_files_too_large(names: &[String], bytes: u64) -> Vec<u8> {
    let json = serde_json::json!({ "names": names, "bytes": bytes }).to_string();
    encode(&[(FILES_TOO_LARGE, json.as_bytes())])
}

/// Files copied on the peer, announced in a payload.
pub fn decode_files(payload: &[u8]) -> Option<(u64, Vec<String>)> {
    let json = entry(payload, FILES)?;
    let value: serde_json::Value = serde_json::from_slice(&json).ok()?;
    let transfer = value.get("transfer")?.as_str()?.parse().ok()?;
    let names = value.get("names")?.as_array()?.iter().filter_map(|n| n.as_str().map(String::from)).collect();
    Some((transfer, names))
}

/// The `text/plain` entry, if there is one.
pub fn decode_text(payload: &[u8]) -> Option<String> {
    String::from_utf8(entry(payload, "text/plain")?).ok()
}

/// The bytes of the first entry of type `wanted`.
fn entry(payload: &[u8], wanted: &str) -> Option<Vec<u8>> {
    let mut rest = payload;
    let mut take = |n: usize| -> Option<&[u8]> {
        if rest.len() < n {
            return None;
        }
        let (head, tail) = rest.split_at(n);
        rest = tail;
        Some(head)
    };
    let count = u16::from_be_bytes(take(2)?.try_into().ok()?);
    for _ in 0..count {
        let type_len = u16::from_be_bytes(take(2)?.try_into().ok()?) as usize;
        let mime = take(type_len)?.to_vec();
        let len = u32::from_be_bytes(take(4)?.try_into().ok()?) as usize;
        let bytes = take(len)?;
        if mime == wanted.as_bytes() {
            return Some(bytes.to_vec());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        assert_eq!(decode_text(&encode_text("héllo")).as_deref(), Some("héllo"));
    }

    #[test]
    fn finds_text_after_other_entries() {
        let mut payload = 2u16.to_be_bytes().to_vec();
        for (mime, bytes) in [(&b"text/html"[..], &b"<b>x</b>"[..]), (b"text/plain", b"x")] {
            payload.extend_from_slice(&(mime.len() as u16).to_be_bytes());
            payload.extend_from_slice(mime);
            payload.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
            payload.extend_from_slice(bytes);
        }
        assert_eq!(decode_text(&payload).as_deref(), Some("x"));
    }

    #[test]
    fn files_round_trip() {
        let names = vec!["a.txt".to_string(), "Folder".to_string()];
        let payload = encode_files(42, &names);
        assert_eq!(decode_files(&payload), Some((42, names)));
        assert_eq!(decode_text(&payload).as_deref(), Some("a.txt\nFolder"));
    }

    #[test]
    fn reads_the_macs_files_entry() {
        let json = br#"{"transfer":"7","names":["Photo.jpg"]}"#;
        let payload = encode(&[(FILES, json), ("text/plain", b"Photo.jpg")]);
        assert_eq!(decode_files(&payload), Some((7, vec!["Photo.jpg".to_string()])));
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(decode_text(&encode_text("hello")[..10]), None);
    }
}
