//! CPU blur / pixelate of the whole screenshot (computed once per strength and
//! then drawn clipped to each region, so dragging a blur box is real time).

fn threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

/// Horizontal box blur of every row, radius r, edges clamped.
fn box_rows(src: &[u8], dst: &mut [u8], w: usize, h: usize, r: usize) {
    let rows_per = h.div_ceil(threads()).max(1);
    std::thread::scope(|s| {
        for (ci, chunk) in dst.chunks_mut(rows_per * w * 4).enumerate() {
            let y0 = ci * rows_per;
            s.spawn(move || {
                let n = (2 * r + 1) as u32;
                for (dy, out) in chunk.chunks_mut(w * 4).enumerate() {
                    let row = &src[(y0 + dy) * w * 4..(y0 + dy + 1) * w * 4];
                    let at = |x: isize, c: usize| {
                        row[(x.clamp(0, w as isize - 1) as usize) * 4 + c] as u32
                    };
                    let mut sum = [0u32; 3];
                    for c in 0..3 {
                        for x in -(r as isize)..=(r as isize) {
                            sum[c] += at(x, c);
                        }
                    }
                    for x in 0..w {
                        for c in 0..3 {
                            out[x * 4 + c] = ((sum[c] + n / 2) / n) as u8;
                            sum[c] += at(x as isize + r as isize + 1, c);
                            sum[c] -= at(x as isize - r as isize, c);
                        }
                        out[x * 4 + 3] = 255;
                    }
                }
            });
        }
    });
}

fn transpose(src: &[u8], dst: &mut [u8], w: usize, h: usize) {
    // dst is h x w.
    let cols_per = w.div_ceil(threads()).max(1);
    std::thread::scope(|s| {
        for (ci, chunk) in dst.chunks_mut(cols_per * h * 4).enumerate() {
            let x0 = ci * cols_per;
            s.spawn(move || {
                for (dx, out) in chunk.chunks_mut(h * 4).enumerate() {
                    let x = x0 + dx;
                    for y in 0..h {
                        let si = (y * w + x) * 4;
                        out[y * 4..y * 4 + 4].copy_from_slice(&src[si..si + 4]);
                    }
                }
            });
        }
    });
}

/// Gaussian-like blur (three box passes each way). `radius` in pixels.
pub fn blur(src: &[u8], w: usize, h: usize, radius: u32) -> Vec<u8> {
    let r = ((radius as f32) * 0.58).round().max(1.0) as usize;
    let mut a = src.to_vec();
    let mut b = vec![0u8; a.len()];
    for _ in 0..3 {
        box_rows(&a, &mut b, w, h, r);
        std::mem::swap(&mut a, &mut b);
    }
    transpose(&a, &mut b, w, h);
    std::mem::swap(&mut a, &mut b);
    for _ in 0..3 {
        box_rows(&a, &mut b, h, w, r);
        std::mem::swap(&mut a, &mut b);
    }
    transpose(&a, &mut b, h, w);
    b
}

/// Mosaic with square blocks aligned to the image grid.
pub fn pixelate(src: &[u8], w: usize, h: usize, block: u32) -> Vec<u8> {
    let bs = block.max(2) as usize;
    let mut out = vec![0u8; src.len()];
    let bands = h.div_ceil(bs);
    let per = bands.div_ceil(threads()).max(1);
    std::thread::scope(|s| {
        for (ci, chunk) in out.chunks_mut(per * bs * w * 4).enumerate() {
            s.spawn(move || {
                let y_start = ci * per * bs;
                let rows = chunk.len() / (w * 4);
                let mut by = 0;
                while by < rows {
                    let bh = bs.min(rows - by);
                    let mut bx = 0;
                    while bx < w {
                        let bw = bs.min(w - bx);
                        let mut sum = [0u64; 3];
                        for y in 0..bh {
                            let row = (y_start + by + y) * w * 4;
                            for x in 0..bw {
                                let i = row + (bx + x) * 4;
                                sum[0] += src[i] as u64;
                                sum[1] += src[i + 1] as u64;
                                sum[2] += src[i + 2] as u64;
                            }
                        }
                        let n = (bw * bh) as u64;
                        let c = [
                            (sum[0] / n) as u8,
                            (sum[1] / n) as u8,
                            (sum[2] / n) as u8,
                            255,
                        ];
                        for y in 0..bh {
                            let row = (by + y) * w * 4;
                            for x in 0..bw {
                                let i = row + (bx + x) * 4;
                                chunk[i..i + 4].copy_from_slice(&c);
                            }
                        }
                        bx += bs;
                    }
                    by += bs;
                }
            });
        }
    });
    out
}
