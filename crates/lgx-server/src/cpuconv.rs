//! CPU XRGB/ARGB -> full-range BT.709 NV12.
//!
//! Used by the linear-capture path: the compositor blits into a dumb dma-buf,
//! the CPU converts it, and the VAAPI helper uploads the NV12. This keeps the
//! iGPU's 3D engine free for the compositor while the video engine encodes.

#[inline]
fn clamp(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// `src` is a linear BGRX/BGRA buffer (`stride` bytes per row); `out` must be
/// resized to `w*h*3/2` by the caller or this function will.
pub fn xrgb_to_nv12(src: &[u8], w: usize, h: usize, stride: usize, out: &mut Vec<u8>) {
    let need = w * h * 3 / 2;
    if out.len() != need {
        out.resize(need, 0);
    }
    let (ys, uvs) = out.split_at_mut(w * h);
    let h2 = h / 2;
    let w2 = w / 2;
    for y in 0..h2 {
        let r0 = &src[(2 * y) * stride..];
        let r1 = &src[(2 * y + 1) * stride..];
        let uv_row = &mut uvs[y * w..y * w + w];
        let (y0_row, y1_row) = ys[(2 * y) * w..(2 * y + 2) * w].split_at_mut(w);
        for x in 0..w2 {
            let i = x * 4;
            let b0 = r0[i] as i32;
            let g0 = r0[i + 1] as i32;
            let rr0 = r0[i + 2] as i32;
            let b1 = r0[i + 4] as i32;
            let g1 = r0[i + 5] as i32;
            let rr1 = r0[i + 6] as i32;
            let b2 = r1[i] as i32;
            let g2 = r1[i + 1] as i32;
            let rr2 = r1[i + 2] as i32;
            let b3 = r1[i + 4] as i32;
            let g3 = r1[i + 5] as i32;
            let rr3 = r1[i + 6] as i32;

            let yv = |r: i32, g: i32, b: i32| -> u8 {
                clamp((13933 * r + 46871 * g + 4732 * b + 32768) >> 16)
            };
            y0_row[x * 2] = yv(rr0, g0, b0);
            y0_row[x * 2 + 1] = yv(rr1, g1, b1);
            y1_row[x * 2] = yv(rr2, g2, b2);
            y1_row[x * 2 + 1] = yv(rr3, g3, b3);

            let rs = rr0 + rr1 + rr2 + rr3;
            let gs = g0 + g1 + g2 + g3;
            let bs = b0 + b1 + b2 + b3;
            let u = clamp(((-7508 * rs - 25259 * gs + 32768 * bs + 131072) >> 18) + 128);
            let v = clamp(((32768 * rs - 29767 * gs - 3005 * bs + 131072) >> 18) + 128);
            uv_row[x * 2] = u;
            uv_row[x * 2 + 1] = v;
        }
    }
}

/// Rows of one worker: `src` starts at the chunk's first row and `ys`/`uvs`
/// are the matching output planes.
fn chunk(src: &[u8], w: usize, h: usize, stride: usize, ys: &mut [u8], uvs: &mut [u8]) {
    let w2 = w / 2;
    for y in 0..h / 2 {
        let r0 = &src[(2 * y) * stride..];
        let r1 = &src[(2 * y + 1) * stride..];
        let uv_row = &mut uvs[y * w..y * w + w];
        let (y0_row, y1_row) = ys[(2 * y) * w..(2 * y + 2) * w].split_at_mut(w);
        for x in 0..w2 {
            let i = x * 4;
            let b0 = r0[i] as i32;
            let g0 = r0[i + 1] as i32;
            let rr0 = r0[i + 2] as i32;
            let b1 = r0[i + 4] as i32;
            let g1 = r0[i + 5] as i32;
            let rr1 = r0[i + 6] as i32;
            let b2 = r1[i] as i32;
            let g2 = r1[i + 1] as i32;
            let rr2 = r1[i + 2] as i32;
            let b3 = r1[i + 4] as i32;
            let g3 = r1[i + 5] as i32;
            let rr3 = r1[i + 6] as i32;

            let yv = |r: i32, g: i32, b: i32| -> u8 {
                clamp((13933 * r + 46871 * g + 4732 * b + 32768) >> 16)
            };
            y0_row[x * 2] = yv(rr0, g0, b0);
            y0_row[x * 2 + 1] = yv(rr1, g1, b1);
            y1_row[x * 2] = yv(rr2, g2, b2);
            y1_row[x * 2 + 1] = yv(rr3, g3, b3);

            let rs = rr0 + rr1 + rr2 + rr3;
            let gs = g0 + g1 + g2 + g3;
            let bs = b0 + b1 + b2 + b3;
            let u = clamp(((-7508 * rs - 25259 * gs + 32768 * bs + 131072) >> 18) + 128);
            let v = clamp(((32768 * rs - 29767 * gs - 3005 * bs + 131072) >> 18) + 128);
            uv_row[x * 2] = u;
            uv_row[x * 2 + 1] = v;
        }
    }
}

/// Multi-threaded variant for wide screens. The conversion is the only CPU
/// work left in the portal path, and a 3440x1440 desktop at 160 Hz otherwise
/// saturates one core and limits the stream to ~110 fps.
pub fn xrgb_to_nv12_parallel(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    out: &mut Vec<u8>,
    threads: usize,
) {
    let need = w * h * 3 / 2;
    if out.len() != need {
        out.resize(need, 0);
    }
    let threads = threads.min((h / 2).max(1));
    if threads <= 1 || w * h < 512 * 512 {
        xrgb_to_nv12(src, w, h, stride, out);
        return;
    }
    let rows_per = (h / 2).div_ceil(threads) * 2;
    let (ys, uvs) = out.split_at_mut(w * h);
    let mut y_rest = ys;
    let mut uv_rest = uvs;
    let mut row = 0;
    std::thread::scope(|scope| {
        while row < h {
            let nrows = rows_per.min(h - row);
            if nrows < 2 {
                break;
            }
            let (y_chunk, y_tail) = y_rest.split_at_mut(nrows * w);
            let (uv_chunk, uv_tail) = uv_rest.split_at_mut((nrows / 2) * w);
            let src_chunk = &src[row * stride..];
            scope.spawn(move || chunk(src_chunk, w, nrows, stride, y_chunk, uv_chunk));
            y_rest = y_tail;
            uv_rest = uv_tail;
            row += nrows;
        }
    });
}
