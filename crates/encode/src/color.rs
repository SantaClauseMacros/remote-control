//! Scalar BGRA ↔ NV12 colour conversion (BT.601, limited range — what the
//! Media Foundation H.264 encoder/decoder expect by default).
//!
//! These run on the CPU. At 1080p they cost a few milliseconds per frame in a
//! release build — acceptable for milestone 3. Milestone 6 moves the encode
//! side onto the GPU (capture texture → Video Processor MFT → encoder, no CPU
//! touch); the decode side stays here (the client is not the constrained end).

/// Bytes needed for an NV12 image of `w × h` (both must be even).
pub fn nv12_len(w: usize, h: usize) -> usize {
    w * h + (w * h) / 2
}

/// Convert top-down BGRA (`w*h*4` bytes) to NV12 in `out` (`nv12_len` bytes).
pub fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, out: &mut [u8]) {
    debug_assert_eq!(w % 2, 0);
    debug_assert_eq!(h % 2, 0);
    debug_assert!(bgra.len() >= w * h * 4);
    debug_assert!(out.len() >= nv12_len(w, h));

    let (y_plane, uv_plane) = out.split_at_mut(w * h);

    // Luma: every pixel. BT.601 studio-swing coefficients (sum ≈ 219), matched
    // to the 298-gain inverse in `nv12_to_bgra`.
    for y in 0..h {
        let row = &bgra[y * w * 4..(y + 1) * w * 4];
        let yrow = &mut y_plane[y * w..(y + 1) * w];
        for x in 0..w {
            let b = row[x * 4] as i32;
            let g = row[x * 4 + 1] as i32;
            let r = row[x * 4 + 2] as i32;
            yrow[x] = clamp8(((66 * r + 129 * g + 25 * b + 128) >> 8) + 16);
        }
    }

    // Chroma: one sample per 2×2 block, averaging the block's four pixels.
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let mut sr = 0i32;
            let mut sg = 0i32;
            let mut sb = 0i32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let px = ((by * 2 + dy) * w + (bx * 2 + dx)) * 4;
                    sb += bgra[px] as i32;
                    sg += bgra[px + 1] as i32;
                    sr += bgra[px + 2] as i32;
                }
            }
            let (r, g, b) = (sr / 4, sg / 4, sb / 4);
            let u = ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128;
            let v = ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128;
            let idx = (by * (w / 2) + bx) * 2;
            uv_plane[idx] = clamp8(u);
            uv_plane[idx + 1] = clamp8(v);
        }
    }
}

/// Convert NV12 (`nv12_len` bytes) to top-down BGRA in `out` (`w*h*4` bytes),
/// with `src_stride` / `src_uv_stride` allowing the decoder's padded row pitch.
pub fn nv12_to_bgra(
    y_plane: &[u8],
    uv_plane: &[u8],
    w: usize,
    h: usize,
    y_stride: usize,
    uv_stride: usize,
    out: &mut [u8],
) {
    debug_assert!(out.len() >= w * h * 4);
    for y in 0..h {
        for x in 0..w {
            let yy = y_plane[y * y_stride + x] as i32 - 16;
            let uv_i = (y / 2) * uv_stride + (x / 2) * 2;
            let u = uv_plane[uv_i] as i32 - 128;
            let v = uv_plane[uv_i + 1] as i32 - 128;

            let c = 298 * yy;
            let r = (c + 409 * v + 128) >> 8;
            let g = (c - 100 * u - 208 * v + 128) >> 8;
            let b = (c + 516 * u + 128) >> 8;

            let o = (y * w + x) * 4;
            out[o] = clamp8(b);
            out[o + 1] = clamp8(g);
            out[o + 2] = clamp8(r);
            out[o + 3] = 255;
        }
    }
}

#[inline]
fn clamp8(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grey_roundtrips_within_tolerance() {
        let (w, h) = (8usize, 8usize);
        let bgra = vec![128u8; w * h * 4];
        let mut nv12 = vec![0u8; nv12_len(w, h)];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        let (yp, uvp) = nv12.split_at(w * h);
        let mut back = vec![0u8; w * h * 4];
        nv12_to_bgra(yp, uvp, w, h, w, w, &mut back);
        for (i, (&a, &b)) in bgra.iter().zip(back.iter()).enumerate() {
            if i % 4 != 3 {
                assert!((a as i32 - b as i32).abs() <= 4, "channel {i}: {a} vs {b}");
            }
        }
    }

    #[test]
    fn primary_red_maps_to_reddish() {
        let (w, h) = (2usize, 2usize);
        let mut bgra = vec![0u8; w * h * 4];
        for p in bgra.chunks_mut(4) {
            p[2] = 255; // R
            p[3] = 255;
        }
        let mut nv12 = vec![0u8; nv12_len(w, h)];
        bgra_to_nv12(&bgra, w, h, &mut nv12);
        let (yp, uvp) = nv12.split_at(w * h);
        let mut back = vec![0u8; w * h * 4];
        nv12_to_bgra(yp, uvp, w, h, w, w, &mut back);
        assert!(back[2] > 200, "R should survive: {}", back[2]);
        assert!(back[0] < 60 && back[1] < 60, "B/G should stay low");
    }
}
