//! Text clipboard sharing, in the apps' payload format (docs/protocol.md): `count: u16 BE`,
//! then per entry `type_len: u16 BE | MIME type | len: u32 BE | bytes`. Only `text/plain`
//! here; the Mac's other entries (rich text, images, files) are skipped.

pub fn encode_text(text: &str) -> Vec<u8> {
    let mime = b"text/plain";
    let mut out = Vec::with_capacity(2 + 2 + mime.len() + 4 + text.len());
    out.extend_from_slice(&1u16.to_be_bytes());
    out.extend_from_slice(&(mime.len() as u16).to_be_bytes());
    out.extend_from_slice(mime);
    out.extend_from_slice(&(text.len() as u32).to_be_bytes());
    out.extend_from_slice(text.as_bytes());
    out
}

/// The `text/plain` entry, if there is one.
pub fn decode_text(payload: &[u8]) -> Option<String> {
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
        if mime == b"text/plain" {
            return String::from_utf8(bytes.to_vec()).ok();
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
    fn rejects_truncated() {
        assert_eq!(decode_text(&encode_text("hello")[..10]), None);
    }
}
