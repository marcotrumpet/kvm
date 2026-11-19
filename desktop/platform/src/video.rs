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

/// The other way, for viewing: a payload back to Annex B, the parameter sets (if any) first.
/// Returns (HEVC, keyframe-capable, stream); None if malformed.
pub fn unpack(payload: &[u8]) -> Option<(bool, bool, Vec<u8>)> {
    let (&head, rest) = payload.split_first()?;
    let (hevc, nal_len) = (head & 0x80 != 0, (head & 0x7F) as usize);
    if !(1..=4).contains(&nal_len) {
        return None;
    }
    let (&count, mut rest) = rest.split_first()?;
    let mut out = Vec::with_capacity(payload.len() + 16);
    for _ in 0..count {
        let len = u16::from_be_bytes(rest.get(..2)?.try_into().ok()?) as usize;
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(rest.get(2..2 + len)?);
        rest = &rest[2 + len..];
    }
    while !rest.is_empty() {
        let len = rest.get(..nal_len)?.iter().fold(0usize, |n, &b| n << 8 | b as usize);
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(rest.get(nal_len..nal_len + len)?);
        rest = &rest[nal_len + len..];
    }
    Some((hevc, count > 0, out))
}

/// NV12 (BT.709 limited range; `stride` bytes per row, chroma after `stride * plane_rows`)
/// to BGRA, `width` x `height`.
pub fn nv12_to_bgra(src: &[u8], width: usize, height: usize, stride: usize, plane_rows: usize, out: &mut Vec<u8>) {
    out.resize(width * height * 4, 0);
    let chroma = stride * plane_rows;
    for (y, dst) in out.chunks_exact_mut(width * 4).enumerate() {
        let uv = &src[chroma + (y / 2) * stride..][..width.div_ceil(2) * 2];
        yuv_row(&src[y * stride..][..width], dst, |i| (uv[2 * i], uv[2 * i + 1]));
    }
}

/// I420 (separate U and V planes, as the decoders give it) to BGRA, `width` x `height`; same
/// colors as [`nv12_to_bgra`].
#[allow(clippy::too_many_arguments)]
pub fn i420_to_bgra(
    y_plane: &[u8],
    y_stride: usize,
    u_plane: &[u8],
    v_plane: &[u8],
    uv_stride: usize,
    width: usize,
    height: usize,
    out: &mut Vec<u8>,
) {
    out.resize(width * height * 4, 0);
    let half = width.div_ceil(2);
    for (y, dst) in out.chunks_exact_mut(width * 4).enumerate() {
        let (u, v) = (&u_plane[(y / 2) * uv_stride..][..half], &v_plane[(y / 2) * uv_stride..][..half]);
        yuv_row(&y_plane[y * y_stride..][..width], dst, |i| (u[i], v[i]));
    }
}

/// One row of 4:2:0 pixels (BT.709 limited range) to BGRA; `uv(i)` is the chroma of pixels
/// 2i and 2i + 1, worked out once for both.
#[inline(always)]
fn yuv_row(y: &[u8], dst: &mut [u8], uv: impl Fn(usize) -> (u8, u8)) {
    let width = y.len();
    for (i, (luma, px)) in y.as_chunks::<2>().0.iter().zip(dst.as_chunks_mut::<8>().0.iter_mut()).enumerate() {
        let (u, v) = uv(i);
        let (u, v) = (u as i32 - 128, v as i32 - 128);
        let (r, g, b) = (459 * v + 128, -55 * u - 136 * v + 128, 541 * u + 128);
        let c0 = (luma[0] as i32 - 16) * 298;
        let c1 = (luma[1] as i32 - 16) * 298;
        px.copy_from_slice(&[
            clamp8((c0 + b) >> 8),
            clamp8((c0 + g) >> 8),
            clamp8((c0 + r) >> 8),
            255,
            clamp8((c1 + b) >> 8),
            clamp8((c1 + g) >> 8),
            clamp8((c1 + r) >> 8),
            255,
        ]);
    }
    if width % 2 == 1 {
        let (u, v) = uv(width / 2);
        dst[width * 4 - 4..].copy_from_slice(&bt709(y[width - 1], u, v));
    }
}

#[inline(always)]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// One BT.709 limited-range pixel as BGRA.
fn bt709(y: u8, u: u8, v: u8) -> [u8; 4] {
    let c = (y as i32 - 16) * 298;
    let (u, v) = (u as i32 - 128, v as i32 - 128);
    let r = (c + 459 * v + 128) >> 8;
    let g = (c - 55 * u - 136 * v + 128) >> 8;
    let b = (c + 541 * u + 128) >> 8;
    [clamp8(b), clamp8(g), clamp8(r), 255]
}

/// Scales a BGRA picture to `width` x `height` (nearest pixel: the stream is asked for at
/// the window's size, so this rarely does much).
pub fn scale_bgra(src: &[u8], src_w: usize, src_h: usize, width: usize, height: usize, out: &mut Vec<u8>) {
    out.resize(width * height * 4, 0);
    if (src_w, src_h) == (width, height) {
        out.copy_from_slice(&src[..width * height * 4]);
        return;
    }
    let xs: Vec<usize> = (0..width).map(|x| (x * src_w / width.max(1)).min(src_w - 1) * 4).collect();
    for y in 0..height {
        let row = &src[(y * src_h / height.max(1)).min(src_h - 1) * src_w * 4..];
        let dst = &mut out[y * width * 4..(y + 1) * width * 4];
        for (x, &sx) in xs.iter().enumerate() {
            dst[x * 4..x * 4 + 4].copy_from_slice(&row[sx..sx + 4]);
        }
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
    // Average up to a 2x2 sample of each covered block: enough for screen content, and
    // quick (the full box average costs too much at 60 fps). As byte offsets per column
    // and row offsets per row; at the same size, each sample is just the pixel.
    let taps = |n: usize, src_n: usize| -> Vec<(usize, usize)> {
        (0..n)
            .map(|i| {
                let a = i * src_n / n;
                let b = ((i + 1) * src_n / n).max(a + 1).min(src_n);
                (a, if b - a > 1 { a + (b - a) / 2 } else { a })
            })
            .collect()
    };
    let xs: Vec<(usize, usize)> = taps(w, src_w as usize).into_iter().map(|(a, b)| (a * 4, b * 4)).collect();
    let ys = taps(h, src_h as usize);
    let same_size = (src_w as usize, src_h as usize) == (w, h);
    let mut rgb_row = vec![[0i32; 3]; w];
    let mut prev_row = vec![[0i32; 3]; w];
    for (y, &(ya, yb)) in ys.iter().enumerate() {
        let (top, bottom) = (&src[ya * stride..], &src[yb * stride..]);
        let luma = &mut y_plane[y * w..(y + 1) * w];
        if same_size {
            for ((rgb, l), px) in rgb_row.iter_mut().zip(luma.iter_mut()).zip(top.as_chunks::<4>().0) {
                let (r, g, b) = (px[2] as i32, px[1] as i32, px[0] as i32);
                *rgb = [r, g, b];
                *l = (16 + ((47 * r + 157 * g + 16 * b) >> 8)).clamp(16, 235) as u8;
            }
        } else {
            let px = |row: &[u8], at: usize| u32::from_le_bytes(row[at..at + 4].try_into().unwrap());
            for ((rgb, l), &(xa, xb)) in rgb_row.iter_mut().zip(luma.iter_mut()).zip(&xs) {
                let p = [px(top, xa), px(top, xb), px(bottom, xa), px(bottom, xb)];
                let sum = |shift: u32| p.iter().map(|&v| (v >> shift & 0xFF) as i32).sum::<i32>();
                let (r, g, b) = (sum(16) / 4, sum(8) / 4, sum(0) / 4);
                *rgb = [r, g, b];
                *l = (16 + ((47 * r + 157 * g + 16 * b) >> 8)).clamp(16, 235) as u8;
            }
        }
        if y % 2 == 1 {
            let row = &mut uv_plane[(y / 2) * w..(y / 2) * w + w];
            for ((uv, above), below) in row.as_chunks_mut::<2>().0.iter_mut().zip(prev_row.as_chunks::<2>().0).zip(rgb_row.as_chunks::<2>().0) {
                let s = |c: usize| (above[0][c] + above[1][c] + below[0][c] + below[1][c]) / 4;
                let (r, g, b) = (s(0), s(1), s(2));
                uv[0] = (128 + ((-26 * r - 87 * g + 112 * b) >> 8)).clamp(16, 240) as u8;
                uv[1] = (128 + ((112 * r - 102 * g - 10 * b) >> 8)).clamp(16, 240) as u8;
            }
        }
        std::mem::swap(&mut rgb_row, &mut prev_row);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The straightforward versions these replaced, to check them against.
    mod reference {
        /// NV12 (BT.709 limited range; `stride` bytes per row, chroma after `stride * plane_rows`)
        /// to BGRA, `width` x `height`.
        pub fn nv12_to_bgra(src: &[u8], width: usize, height: usize, stride: usize, plane_rows: usize, out: &mut Vec<u8>) {
            out.resize(width * height * 4, 0);
            let chroma = stride * plane_rows;
            for y in 0..height {
                let row = &src[y * stride..];
                let uv = &src[chroma + (y / 2) * stride..];
                let dst = &mut out[y * width * 4..(y + 1) * width * 4];
                for x in 0..width {
                    dst[x * 4..x * 4 + 4].copy_from_slice(&bt709(row[x], uv[x & !1], uv[(x & !1) + 1]));
                }
            }
        }

        /// I420 (separate U and V planes, as OpenH264 decodes) to BGRA, `width` x `height`; same
        /// colors as [`nv12_to_bgra`].
        #[allow(clippy::too_many_arguments)]
        pub fn i420_to_bgra(
            y_plane: &[u8],
            y_stride: usize,
            u_plane: &[u8],
            v_plane: &[u8],
            uv_stride: usize,
            width: usize,
            height: usize,
            out: &mut Vec<u8>,
        ) {
            out.resize(width * height * 4, 0);
            for y in 0..height {
                let row = &y_plane[y * y_stride..];
                let (u, v) = (&u_plane[(y / 2) * uv_stride..], &v_plane[(y / 2) * uv_stride..]);
                let dst = &mut out[y * width * 4..(y + 1) * width * 4];
                for x in 0..width {
                    dst[x * 4..x * 4 + 4].copy_from_slice(&bt709(row[x], u[x / 2], v[x / 2]));
                }
            }
        }

        /// One BT.709 limited-range pixel as BGRA.
        fn bt709(y: u8, u: u8, v: u8) -> [u8; 4] {
            let c = (y as i32 - 16) * 298;
            let (u, v) = (u as i32 - 128, v as i32 - 128);
            let r = (c + 459 * v + 128) >> 8;
            let g = (c - 55 * u - 136 * v + 128) >> 8;
            let b = (c + 541 * u + 128) >> 8;
            [b.clamp(0, 255) as u8, g.clamp(0, 255) as u8, r.clamp(0, 255) as u8, 255]
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

    }

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
    fn unpacks_what_it_packs() {
        let stream = [0, 0, 0, 1, 0x67, 9, 9, 0, 0, 0, 1, 0x68, 8, 0, 0, 0, 1, 0x65, 7, 7, 7];
        let payload = H264Packager::default().package(&stream, true).unwrap();
        assert_eq!(unpack(&payload), Some((false, true, stream.to_vec())));
        assert_eq!(unpack(&payload[..payload.len() - 1]), None);
    }

    #[test]
    fn colors_survive_nv12_both_ways() {
        let (w, h) = (4u32, 2u32);
        let mut src = Vec::new();
        for _ in 0..(w * h) {
            src.extend_from_slice(&[40, 160, 220, 255]); // BGRA: an orange
        }
        let (mut nv12, mut back) = (Vec::new(), Vec::new());
        bgra_to_nv12(&src, w, h, (w * 4) as usize, w, h, &mut nv12);
        nv12_to_bgra(&nv12, w as usize, h as usize, w as usize, h as usize, &mut back);
        for px in back.chunks(4) {
            for (got, want) in px[..3].iter().zip([40u8, 160, 220]) {
                assert!((*got as i32 - want as i32).abs() <= 6, "{px:?}");
            }
        }
    }

    /// Pseudo-random bytes, the same every run.
    fn noise(n: usize, seed: u32) -> Vec<u8> {
        let mut x = seed.wrapping_mul(2_654_435_761).max(1);
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                (x >> 24) as u8
            })
            .collect()
    }

    /// The faster conversions give exactly the old ones' bytes: odd sizes, padded rows,
    /// scaling down, up and not at all.
    #[test]
    fn matches_the_reference_conversions() {
        for (w, h) in [(2usize, 2usize), (7, 5), (64, 36), (101, 51)] {
            let (stride, rows) = (w + 6, h + 2);
            let src = noise(stride * rows * 3 / 2 + stride, (w * h) as u32);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            nv12_to_bgra(&src, w, h, stride, rows, &mut a);
            reference::nv12_to_bgra(&src, w, h, stride, rows, &mut b);
            assert_eq!(a, b, "nv12 {w}x{h}");
            let (u, v) = (noise(stride * rows, 1), noise(stride * rows, 2));
            i420_to_bgra(&src, stride, &u, &v, stride / 2, w, h, &mut a);
            reference::i420_to_bgra(&src, stride, &u, &v, stride / 2, w, h, &mut b);
            assert_eq!(a, b, "i420 {w}x{h}");
        }
        for ((sw, sh), (w, h)) in [((64u32, 36u32), (64u32, 36u32)), ((100, 60), (64, 36)), ((33, 21), (64, 40)), ((1920, 1080), (1280, 720))] {
            let stride = sw as usize * 4 + 12;
            let src = noise(stride * sh as usize, sw * sh);
            let (mut a, mut b) = (Vec::new(), Vec::new());
            bgra_to_nv12(&src, sw, sh, stride, w, h, &mut a);
            reference::bgra_to_nv12(&src, sw, sh, stride, w, h, &mut b);
            assert_eq!(a, b, "bgra_to_nv12 {sw}x{sh} -> {w}x{h}");
        }
    }

    /// Speeds at 1080p: cargo test --release -p kvm-platform conversion_speed -- --ignored --nocapture
    #[test]
    #[ignore]
    fn conversion_speed() {
        let (w, h) = (1920usize, 1080usize);
        let src: Vec<u8> = (0..w * h * 4).map(|i| (i * 7 % 251) as u8).collect();
        let (mut nv12, mut bgra, mut scaled) = (Vec::new(), Vec::new(), Vec::new());
        let time = |name: &str, f: &mut dyn FnMut()| {
            f();
            let start = std::time::Instant::now();
            for _ in 0..20 {
                f();
            }
            println!("{name}: {:.2} ms", start.elapsed().as_secs_f64() * 1000.0 / 20.0);
        };
        time("bgra_to_nv12", &mut || bgra_to_nv12(&src, w as u32, h as u32, w * 4, w as u32, h as u32, &mut nv12));
        time("bgra_to_nv12 to 720p", &mut || bgra_to_nv12(&src, w as u32, h as u32, w * 4, 1280, 720, &mut scaled));
        time("(old) bgra_to_nv12 to 720p", &mut || reference::bgra_to_nv12(&src, w as u32, h as u32, w * 4, 1280, 720, &mut scaled));
        time("nv12_to_bgra", &mut || nv12_to_bgra(&nv12, w, h, w, h, &mut bgra));
        let (y, uv) = nv12.split_at(w * h);
        let u: Vec<u8> = uv.iter().step_by(2).copied().collect();
        let v: Vec<u8> = uv.iter().skip(1).step_by(2).copied().collect();
        time("i420_to_bgra", &mut || i420_to_bgra(y, w, &u, &v, w / 2, w, h, &mut bgra));
        time("scale_bgra 1080p->900p", &mut || scale_bgra(&src, w, h, 1600, 900, &mut scaled));
    }

    #[test]
    fn i420_matches_nv12() {
        // 4x1: four luma samples, then two U/V pairs (interleaved in NV12, planes in I420).
        let nv12 = [16, 235, 81, 145, 90, 240, 54, 34];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        nv12_to_bgra(&nv12, 4, 1, 4, 1, &mut a);
        i420_to_bgra(&nv12[..4], 4, &[90, 54], &[240, 34], 2, 4, 1, &mut b);
        assert_eq!(a, b);
        let mut gray = Vec::new();
        i420_to_bgra(&[16, 235], 2, &[128], &[128], 1, 2, 1, &mut gray);
        assert_eq!(gray, vec![0, 0, 0, 255, 255, 255, 255, 255]);
    }

    #[test]
    fn scales_by_nearest_pixel() {
        let px = |v: u8| [v, v, v, 255];
        let src: Vec<u8> = [1, 2, 3, 4].into_iter().flat_map(px).collect(); // 2x2
        let mut out = Vec::new();
        scale_bgra(&src, 2, 2, 4, 2, &mut out);
        let got: Vec<u8> = out.chunks(4).map(|p| p[0]).collect();
        assert_eq!(got, vec![1, 1, 2, 2, 3, 3, 4, 4]);
        scale_bgra(&src, 2, 2, 1, 1, &mut out);
        assert_eq!(out, px(1));
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
