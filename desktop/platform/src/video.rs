//! Encoded frames in the viewers' payload format (docs/protocol.md, VideoFrames.swift):
//! `codec << 7 | nalLengthSize: u8 | parameterSetCount: u8 | (len: u16 BE, bytes)* | AVCC`.
//! Encoders here produce H.264 in Annex B (start codes); this repackages it, carrying the
//! parameter sets (SPS, PPS) on every keyframe.

/// What a viewer asks for (from the core's VideoParams).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScreenParams {
    pub max_width: u16,
    pub max_height: u16,
    pub fps: u8,
    pub bitrate_kbps: u32,
    /// An id from the display list; 0 is the main display.
    pub display: u32,
}

/// One of this machine's displays, as the display list describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Display {
    /// Never 0 (0 means "the main one" on the wire).
    pub id: u32,
    pub name: String,
    pub left: i32,
    pub top: i32,
    pub width: u32,
    pub height: u32,
    pub main: bool,
}

/// The display list's JSON, left to right: `[{"id", "name", "width", "height", "main"}]`.
pub fn display_list_json(displays: &[Display]) -> Vec<u8> {
    let mut sorted: Vec<&Display> = displays.iter().collect();
    sorted.sort_by_key(|d| (d.left, d.top));
    let items: Vec<String> = sorted
        .iter()
        .map(|d| {
            format!(
                r#"{{"id":{},"name":"{}","width":{},"height":{},"main":{}}}"#,
                d.id,
                d.name.replace('\\', "\\\\").replace('"', "\\\""),
                d.width,
                d.height,
                d.main
            )
        })
        .collect();
    format!("[{}]", items.join(",")).into_bytes()
}

/// The NAL units of an Annex B stream, without their start codes.
pub fn nal_units(annex_b: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= annex_b.len() {
        if annex_b[i] == 0 && annex_b[i + 1] == 0 && annex_b[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut units = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let mut end = starts.get(n + 1).map_or(annex_b.len(), |&next| next - 3);
        // A four-byte start code leaves its leading zero on the previous unit.
        while end > start && annex_b[end - 1] == 0 {
            end -= 1;
        }
        if end > start {
            units.push(&annex_b[start..end]);
        }
    }
    units
}

/// Repackages H.264 Annex B output, remembering the latest SPS and PPS for keyframes.
#[derive(Default)]
pub struct H264Packager {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl H264Packager {
    /// The payload for one encoded frame; `keyframe` (an IDR) carries the parameter sets.
    /// None while there is nothing to send (only parameter sets, or no SPS/PPS seen yet
    /// for a keyframe).
    pub fn package(&mut self, annex_b: &[u8], keyframe: bool) -> Option<Vec<u8>> {
        let mut avcc = Vec::with_capacity(annex_b.len() + 16);
        for unit in nal_units(annex_b) {
            match unit[0] & 0x1F {
                7 => self.sps = Some(unit.to_vec()),
                8 => self.pps = Some(unit.to_vec()),
                9 => {} // access unit delimiter
                _ => {
                    avcc.extend_from_slice(&(unit.len() as u32).to_be_bytes());
                    avcc.extend_from_slice(unit);
                }
            }
        }
        if avcc.is_empty() {
            return None;
        }
        let mut out = Vec::with_capacity(avcc.len() + 64);
        out.push(4); // codec 0 (H.264) << 7 | 4-byte NAL lengths
        if keyframe {
            let (sps, pps) = (self.sps.as_ref()?, self.pps.as_ref()?);
            out.push(2);
            for set in [sps, pps] {
                out.extend_from_slice(&(set.len() as u16).to_be_bytes());
                out.extend_from_slice(set);
            }
        } else {
            out.push(0);
        }
        out.extend_from_slice(&avcc);
        Some(out)
    }
}

/// Fits `width` x `height` within the viewer's maximum, keeping the aspect ratio, in even
/// numbers (the encoder's 4:2:0 needs them).
pub fn fit(width: u32, height: u32, max_width: u16, max_height: u16) -> (u32, u32) {
    let (mw, mh) = (max_width.max(64) as f64, max_height.max(64) as f64);
    let scale = (mw / width.max(1) as f64).min(mh / height.max(1) as f64).min(1.0);
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(width as f64 * scale), even(height as f64 * scale))
}

/// Scales a BGRA picture (rows of `stride` bytes) to `width` x `height` NV12 (BT.709,
/// limited range), averaging the source pixels each target pixel covers.
pub fn bgra_to_nv12(src: &[u8], src_w: u32, src_h: u32, stride: usize, width: u32, height: u32, out: &mut Vec<u8>) {
    let (w, h) = (width as usize, height as usize);
    out.resize(w * h + w * h / 2, 0);
    let (y_plane, uv_plane) = out.split_at_mut(w * h);
    let xs: Vec<(usize, usize)> = (0..w)
        .map(|x| {
            let a = x * src_w as usize / w;
            let b = ((x + 1) * src_w as usize / w).max(a + 1).min(src_w as usize);
            (a, b)
        })
        .collect();
    let ys: Vec<(usize, usize)> = (0..h)
        .map(|y| {
            let a = y * src_h as usize / h;
            let b = ((y + 1) * src_h as usize / h).max(a + 1).min(src_h as usize);
            (a, b)
        })
        .collect();
    // Average up to a 2x2 sample of each covered block: enough for screen content, and
    // quick (the full box average costs too much at 60 fps).
    let sample = |x0: usize, x1: usize, y0: usize, y1: usize| -> (i32, i32, i32) {
        let (xa, xb) = (x0, if x1 - x0 > 1 { x0 + (x1 - x0) / 2 } else { x0 });
        let (ya, yb) = (y0, if y1 - y0 > 1 { y0 + (y1 - y0) / 2 } else { y0 });
        let mut sum = (0, 0, 0);
        for (sx, sy) in [(xa, ya), (xb, ya), (xa, yb), (xb, yb)] {
            let p = sy * stride + sx * 4;
            sum.0 += src[p + 2] as i32;
            sum.1 += src[p + 1] as i32;
            sum.2 += src[p] as i32;
        }
        (sum.0 / 4, sum.1 / 4, sum.2 / 4)
    };
    let mut rgb_row = vec![(0i32, 0i32, 0i32); w];
    let mut prev_row = vec![(0i32, 0i32, 0i32); w];
    for (y, &(y0, y1)) in ys.iter().enumerate() {
        for (x, &(x0, x1)) in xs.iter().enumerate() {
            let (r, g, b) = sample(x0, x1, y0, y1);
            rgb_row[x] = (r, g, b);
            y_plane[y * w + x] = (16 + ((47 * r + 157 * g + 16 * b) >> 8)).clamp(16, 235) as u8;
        }
        if y % 2 == 1 {
            let row = &mut uv_plane[(y / 2) * w..(y / 2) * w + w];
            for x in (0..w).step_by(2) {
                let mut s = (0, 0, 0);
                for p in [prev_row[x], prev_row[x + 1], rgb_row[x], rgb_row[x + 1]] {
                    s.0 += p.0;
                    s.1 += p.1;
                    s.2 += p.2;
                }
                let (r, g, b) = (s.0 / 4, s.1 / 4, s.2 / 4);
                row[x] = (128 + ((-26 * r - 87 * g + 112 * b) >> 8)).clamp(16, 240) as u8;
                row[x + 1] = (128 + ((112 * r - 102 * g - 10 * b) >> 8)).clamp(16, 240) as u8;
            }
        }
        std::mem::swap(&mut rgb_row, &mut prev_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_annex_b() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4, 5, 0];
        let units = nal_units(&stream);
        assert_eq!(units, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4, 5][..]]);
    }

    #[test]
    fn keyframes_carry_parameter_sets() {
        let mut p = H264Packager::default();
        let key = p.package(&[0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 7, 7], true).unwrap();
        assert_eq!(key, vec![4, 2, 0, 2, 0x67, 9, 0, 2, 0x68, 8, 0, 0, 0, 3, 0x65, 7, 7]);
        let delta = p.package(&[0, 0, 0, 1, 0x41, 6], false).unwrap();
        assert_eq!(delta, vec![4, 0, 0, 0, 0, 2, 0x41, 6]);
        // A later keyframe without in-band SPS/PPS reuses the remembered ones.
        let again = p.package(&[0, 0, 0, 1, 0x65, 1], true).unwrap();
        assert_eq!(&again[..2], &[4, 2]);
    }

    #[test]
    fn keyframe_needs_parameter_sets() {
        assert_eq!(H264Packager::default().package(&[0, 0, 1, 0x65, 1], true), None);
    }

    #[test]
    fn fits_even_and_within() {
        assert_eq!(fit(3840, 2160, 1920, 1200), (1920, 1080));
        assert_eq!(fit(1366, 768, 4000, 4000), (1366, 768));
        assert_eq!(fit(1365, 767, 4000, 4000), (1364, 766));
    }

    #[test]
    fn converts_white_and_black() {
        let (w, h) = (4u32, 2u32);
        let mut src = vec![255u8; (w * h * 4) as usize];
        for px in src.chunks_mut(4).skip(4) {
            px[..3].fill(0); // bottom row black
        }
        let mut out = Vec::new();
        bgra_to_nv12(&src, w, h, (w * 4) as usize, w, h, &mut out);
        assert_eq!(out.len(), 12);
        assert!(out[..4].iter().all(|&y| y == 235));
        assert!(out[4..8].iter().all(|&y| y == 16));
        assert!(out[8..].iter().all(|&c| (126..=130).contains(&c)), "{:?}", &out[8..]);
    }

    #[test]
    fn display_list_is_sorted_json() {
        let d = |id, left, main| Display { id, name: format!("D{id}"), left, top: 0, width: 10, height: 5, main };
        let json = String::from_utf8(display_list_json(&[d(2, 1920, false), d(1, 0, true)])).unwrap();
        assert_eq!(
            json,
            r#"[{"id":1,"name":"D1","width":10,"height":5,"main":true},{"id":2,"name":"D2","width":10,"height":5,"main":false}]"#
        );
    }
}
