//! Scrolling capture: the user selects an area (or clicks a window) and scrolls
//! it as usual, with the wheel, the scroll bar or the keyboard. The app watches
//! the area and stitches every new part into one tall image.
//!
//! - The area is read about 30 times a second; a frame is used as soon as the
//!   picture changed, so the overlap with the last one stays large.
//! - Frames are matched by row hashes: the shift with the most matching rows wins.
//!   Rows that stay in place at the top and bottom (sticky headers, footers,
//!   cookie bars) are left out of the match and appear once in the result.
//!   When nothing matches exactly (a translucent window, sub-pixel scrolling),
//!   brightness edges are compared instead.
//! - Only the columns that changed take part: side parts that stay in place (a
//!   site menu, a panel, another window) would make every row differ. The result
//!   is cut to the part that scrolled.
//! - Stitching is streamed: the last frame and the result are kept.
//! - Scrolling up is ignored; the capture continues when the page comes back down.
//! - Ends on Esc or Done, or at MAX_H pixels. A panel beside the area shows the
//!   picture growing; it is excluded from screen capture.

use crate::capture::{Monitor, Shot};
use crate::config::Config;
use crate::gdi::*;
use crate::geom::RectI;
use crate::win::*;
use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Height limit of a scrolling capture, in pixels.
pub const MAX_H: usize = 30_000;

// ---------------------------------------------------------------- stitching

/// Column ranges `[from, to)` of a frame that take part in matching.
pub type Runs = Vec<(usize, usize)>;

/// Row hashes and "flat" flags (every pixel of the row the same) of a BGRA
/// frame, over the columns in `runs` (not empty).
pub fn row_hashes(px: &[u8], w: usize, h: usize, runs: &[(usize, usize)]) -> (Vec<u64>, Vec<bool>) {
    let total: usize = runs.iter().map(|r| r.1 - r.0).sum();
    let mut hs = Vec::with_capacity(h);
    let mut flat = Vec::with_capacity(h);
    for y in 0..h {
        let line = &px[y * w * 4..(y + 1) * w * 4];
        let first = &line[runs[0].0 * 4..runs[0].0 * 4 + 4];
        let mut x: u64 = 0xCBF2_9CE4_8422_2325 ^ total as u64;
        let mut same = true;
        for &(a, b) in runs {
            let row = &line[a * 4..b * 4];
            let mut chunks = row.chunks_exact(8);
            for c in &mut chunks {
                let v = u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]]);
                x = (x ^ v).wrapping_mul(0x9E37_79B9_7F4A_7C15).rotate_left(29);
            }
            for &b in chunks.remainder() {
                x = (x ^ b as u64)
                    .wrapping_mul(0x9E37_79B9_7F4A_7C15)
                    .rotate_left(29);
            }
            same = same && row.chunks_exact(4).all(|p| p == first);
        }
        hs.push(x);
        flat.push(same);
    }
    (hs, flat)
}

/// For each column, in how many rows frames `a` and `b` differ.
fn column_changes(a: &[u8], b: &[u8], w: usize, h: usize) -> Vec<u32> {
    let mut n = vec![0u32; w];
    let row = w * 4;
    for y in 0..h {
        let (ra, rb) = (&a[y * row..(y + 1) * row], &b[y * row..(y + 1) * row]);
        if ra == rb {
            continue;
        }
        for (c, (p, q)) in n.iter_mut().zip(ra.chunks_exact(4).zip(rb.chunks_exact(4))) {
            *c += (p[..3] != q[..3]) as u32;
        }
    }
    n
}

/// Runs of columns that changed in at least one row. Side parts that stay in
/// place (a site menu, a panel, another window) change in none.
fn changed_runs(changes: &[u32]) -> Runs {
    let mut runs = Vec::new();
    let mut x = 0;
    while x < changes.len() {
        if changes[x] > 0 {
            let a = x;
            while x < changes.len() && changes[x] > 0 {
                x += 1;
            }
            runs.push((a, x));
        } else {
            x += 1;
        }
    }
    runs
}

/// `runs` without their rightmost columns, for the tolerant match over all of
/// them: a moving scroll bar thumb there would make every row differ.
fn trim_right(runs: &[(usize, usize)]) -> Runs {
    let count: usize = runs.iter().map(|r| r.1 - r.0).sum();
    let mut trim = (count / 16).min(20);
    let mut out = runs.to_vec();
    while trim > 0 {
        let Some(last) = out.last_mut() else { break };
        let cut = trim.min(last.1 - last.0);
        last.1 -= cut;
        trim -= cut;
        if last.0 == last.1 {
            out.pop();
        }
    }
    out
}

/// `runs` cut into strips of about `width` columns.
fn strips(runs: &[(usize, usize)], width: usize) -> Runs {
    let mut out = Vec::new();
    for &(a, b) in runs {
        let k = (b - a).div_ceil(width);
        for i in 0..k {
            out.push((a + (b - a) * i / k, a + (b - a) * (i + 1) / k));
        }
    }
    out
}

/// The shift that the most strip width agrees on; ties go to `expect`.
fn vote(votes: &[((usize, usize), usize)], expect: Option<usize>) -> Option<usize> {
    let mut by_d: Vec<(usize, usize)> = Vec::new(); // d, width
    for &((a, b), d) in votes {
        match by_d.iter_mut().find(|v| v.0 == d) {
            Some(v) => v.1 += b - a,
            None => by_d.push((d, b - a)),
        }
    }
    by_d.into_iter()
        .max_by(|x, y| {
            x.1.cmp(&y.1).then_with(|| match expect {
                Some(e) => y.0.abs_diff(e).cmp(&x.0.abs_diff(e)),
                None => std::cmp::Ordering::Equal,
            })
        })
        .map(|v| v.0)
}

/// Neighbouring strips joined back into runs.
fn join(mut strips: Vec<(usize, usize)>) -> Runs {
    strips.sort_unstable();
    let mut out: Runs = Vec::new();
    for s in strips {
        match out.last_mut() {
            Some(l) if l.1 == s.0 => l.1 = s.1,
            _ => out.push(s),
        }
    }
    out
}

/// Edge signature of a frame for tolerant matching: for each row, the columns
/// in `runs` in bins of `bin` pixels, each bin the sum of the brightness
/// steps between neighbouring pixels. Text and lines give strong steps; a smooth
/// background (an acrylic window showing what is behind it, a gradient) gives
/// almost none, so it hardly counts when it does not scroll with the content.
pub struct Sig {
    pub v: Vec<u16>,
    /// Bins per row.
    pub nb: usize,
    pub bin: usize,
}

impl Sig {
    pub fn new(px: &[u8], w: usize, h: usize, runs: &[(usize, usize)]) -> Sig {
        let total: usize = runs.iter().map(|r| r.1 - r.0).sum();
        let bin = (total / 160).max(2);
        let bins = |r: &(usize, usize)| (r.1 - r.0).saturating_sub(1) / bin;
        let nb: usize = runs.iter().map(bins).sum();
        let mut v = vec![0u16; nb * h];
        for y in 0..h {
            let row = &px[y * w * 4..(y + 1) * w * 4];
            let gray = |x: usize| {
                let p = &row[x * 4..x * 4 + 3];
                (p[0] as i32 + 5 * p[1] as i32 + 2 * p[2] as i32) >> 3
            };
            let mut out = v[y * nb..(y + 1) * nb].iter_mut();
            for r in runs {
                let x0 = r.0;
                let mut prev = gray(x0);
                for (b, o) in (0..bins(r)).zip(&mut out) {
                    let mut sum = 0i32;
                    for x in x0 + b * bin + 1..=x0 + (b + 1) * bin {
                        let g = gray(x);
                        sum += (g - prev).abs();
                        prev = g;
                    }
                    *o = sum.min(u16::MAX as i32) as u16;
                }
            }
        }
        Sig { v, nb, bin }
    }
    fn row(&self, y: usize) -> &[u16] {
        &self.v[y * self.nb..(y + 1) * self.nb]
    }
    /// Sum of differences between row `a` of self and row `b` of `o`.
    fn dist(&self, a: usize, o: &Sig, b: usize) -> u32 {
        self.row(a)
            .iter()
            .zip(o.row(b))
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs())
            .sum()
    }
    /// Rows this far apart (per bin, on average) count as the same picture:
    /// noise, a blinking caret, an animation behind a translucent window.
    fn tolerance(&self) -> f32 {
        2.0 * self.bin as f32
    }
    /// Mean difference per bin between two frames.
    pub fn diff(&self, o: &Sig) -> f32 {
        let total: u64 = self
            .v
            .iter()
            .zip(&o.v)
            .map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64)
            .sum();
        total as f32 / self.v.len().max(1) as f32
    }
    /// Frames that differ less than this are the same picture.
    pub fn still(&self) -> f32 {
        0.25 * self.bin as f32
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Push {
    /// The content moved up by this many rows; new rows were added.
    Moved(usize),
    /// Nothing moved.
    Same,
    /// The frames could not be matched (moved too far, or the content changed).
    NoMatch,
    /// MAX height reached; the result was cut to it.
    Full,
}

pub struct Stitcher {
    w: usize,
    h: usize,
    max_h: usize,
    /// Result, BGRA rows. Invariant: rows == y + h (the last frame fills the end).
    out: Vec<u8>,
    rows: usize,
    /// Page offset of the last frame.
    y: usize,
    /// The last frame that was added (one frame of memory, reused).
    prev: Vec<u8>,
    /// Columns the hashes and the signature of `prev` are made over.
    runs: Runs,
    prev_hash: Vec<u64>,
    /// Made only when an exact match fails.
    prev_sig: Option<Sig>,
    /// Columns that scrolled so far, `[from, to)`; the result is cut to them.
    span: Option<(usize, usize)>,
    /// Why the last frame did not match, for the log.
    pub note: String,
    /// Rows that stayed in place at the top/bottom in the last step.
    pub header: usize,
    pub footer: usize,
    /// Rows from here down changed in the last push (for the live preview).
    changed: usize,
}

fn opaque_rows(dst: &mut Vec<u8>, src: &[u8]) {
    let at = dst.len();
    dst.extend_from_slice(src);
    for a in dst[at..].iter_mut().skip(3).step_by(4) {
        *a = 255;
    }
}

impl Stitcher {
    pub fn new(first: &[u8], w: usize, h: usize, max_h: usize) -> Stitcher {
        let runs = vec![(0, w - (w / 16).min(20))];
        let prev = first[..w * h * 4].to_vec();
        let (prev_hash, _) = row_hashes(&prev, w, h, &runs);
        let mut out = Vec::with_capacity(w * h * 4 * 3);
        opaque_rows(&mut out, &prev);
        Stitcher {
            w,
            h,
            max_h: max_h.max(h),
            out,
            rows: h,
            y: 0,
            prev,
            runs,
            prev_hash,
            prev_sig: None,
            span: None,
            note: String::new(),
            header: 0,
            footer: 0,
            changed: 0,
        }
    }

    pub fn height(&self) -> usize {
        self.rows
    }

    /// The picture so far (BGRA rows of `width` pixels).
    pub fn pixels(&self) -> &[u8] {
        &self.out[..self.rows * self.w * 4]
    }

    /// First row that changed in the last push.
    pub fn changed(&self) -> usize {
        self.changed
    }

    /// Rows of the result that the last frame shows.
    pub fn view(&self) -> (usize, usize) {
        (self.y, self.y + self.h)
    }

    /// Finds how far `cur` moved up against the previous frame.
    /// `expect`: the shift the scroll step should give, to choose between equally
    /// good matches on repeating content.
    fn find(
        &self,
        ph: &[u64],
        ch: &[u64],
        cf: &[bool],
        expect: Option<usize>,
    ) -> Result<(usize, usize, usize), Push> {
        let h = self.h;
        let top = (0..h).take_while(|&i| ph[i] == ch[i]).count();
        if top == h {
            return Err(Push::Same);
        }
        let bot = (0..h)
            .take_while(|&i| ph[h - 1 - i] == ch[h - 1 - i])
            .count();
        let band = h - top - bot;
        if band < (h / 10).max(12) {
            // Only a small spot changed (a caret, an animation): not a scroll.
            return Err(Push::Same);
        }
        let min_overlap = (band / 6).max(8);
        let mut best: Option<(usize, f32, usize)> = None; // d, score, evidence
        for d in 1..=band - min_overlap {
            let n = band - d;
            let mut info = 0usize;
            let mut ok_info = 0usize;
            let mut bad = 0usize;
            let allowed = n / 10;
            for r in 0..n {
                let (a, b) = (top + r, top + r + d);
                let same = ch[a] == ph[b];
                if !cf[a] {
                    info += 1;
                    ok_info += same as usize;
                }
                if !same {
                    bad += 1;
                    if bad > allowed {
                        break;
                    }
                }
            }
            if bad > allowed || info < 4 {
                continue;
            }
            let score = ok_info as f32 / info as f32;
            if score < 0.9 {
                continue;
            }
            let better = match (best, expect) {
                (None, _) => true,
                (Some((bd, bs, _)), Some(e)) => {
                    let (da, db) = (d.abs_diff(e), bd.abs_diff(e));
                    if (score - bs).abs() < 0.02 {
                        da < db
                    } else {
                        score > bs
                    }
                }
                (Some((_, bs, be)), None) => {
                    score > bs + 0.02 || ((score - bs).abs() <= 0.02 && ok_info > be)
                }
            };
            if better {
                best = Some((d, score, ok_info));
            }
        }
        match best {
            Some((d, ..)) => Ok((d, top, bot)),
            None => Err(Push::NoMatch),
        }
    }

    /// Tolerant match on edge signatures, for pictures that never repeat exactly:
    /// a translucent window over a changing background, sub-pixel scrolling,
    /// dithered video. The shift with the smallest difference wins when it is
    /// clearly smaller than the difference between neighbouring rows.
    fn find_fuzzy(
        &self,
        ps: &Sig,
        cs: &Sig,
        expect: Option<usize>,
    ) -> Result<(usize, usize, usize), Push> {
        let h = self.h;
        if cs.nb == 0 {
            return Err(Push::NoMatch);
        }
        let nb = cs.nb as f32;
        let tol = cs.tolerance() * nb;
        let top = (0..h)
            .take_while(|&i| cs.dist(i, ps, i) as f32 <= tol)
            .count();
        if top == h {
            return Err(Push::Same);
        }
        let bot = (0..h)
            .take_while(|&i| cs.dist(h - 1 - i, ps, h - 1 - i) as f32 <= tol)
            .count();
        let band = h - top - bot;
        if band < (h / 10).max(12) {
            return Err(Push::Same);
        }
        // How different neighbouring rows are: the scale a match is judged on.
        let texture = (top..top + band - 1)
            .map(|r| cs.dist(r, cs, r + 1) as f32)
            .sum::<f32>()
            / ((band - 1) as f32 * nb);
        if texture < 0.5 {
            return Err(Push::NoMatch);
        }
        let min_overlap = (band / 6).max(8);
        let step = (band / 500).max(1);
        let mut costs = vec![f32::INFINITY; band + 1];
        let mut best = f32::INFINITY;
        for d in 1..=band - min_overlap {
            let n = band - d;
            let rows = n.div_ceil(step) as f32;
            let limit = best * 1.3 * rows * nb;
            let mut sum = 0f32;
            let mut r = 0;
            while r < n {
                sum += cs.dist(top + r, ps, top + r + d) as f32;
                if sum > limit {
                    break;
                }
                r += step;
            }
            if r >= n {
                let c = sum / (rows * nb);
                costs[d] = c;
                best = best.min(c);
            }
        }
        if !best.is_finite() || best > 0.35 * texture {
            return Err(Push::NoMatch);
        }
        // Several shifts about as good (repeating content): the expected one.
        let near = best * 1.15 + 0.05;
        let d = (1..=band)
            .filter(|&d| costs[d] <= near)
            .min_by(|&a, &b| match expect {
                Some(e) => a.abs_diff(e).cmp(&b.abs_diff(e)),
                None => costs[a].total_cmp(&costs[b]),
            })
            .ok_or(Push::NoMatch)?;
        Ok((d, top, bot))
    }

    pub fn push(&mut self, cur: &[u8], expect: Option<usize>) -> Push {
        let (w, h) = (self.w, self.h);
        let cur = &cur[..w * h * 4];
        // Only the columns that changed can be the scrolling part.
        let changed = changed_runs(&column_changes(&self.prev, cur, w, h));
        let total: usize = changed.iter().map(|r| r.1 - r.0).sum();
        if total < 8 {
            // Nothing, or only a thin strip changed: not a scroll.
            return Push::Same;
        }
        if self.span.is_some()
            && !changed
                .iter()
                .any(|c| self.runs.iter().any(|r| c.0 < r.1 && r.0 < c.1))
        {
            // Something changed beside the page (a video, another window); the
            // page itself did not move.
            return Push::Same;
        }
        // Each strip of changed columns looks for its own shift. The page agrees
        // on one; a video, an animation or a scroll bar thumb beside it matches
        // none and has no say.
        let width = (total / 32).max(96);
        let mut votes = Vec::new();
        let (mut missed, mut matched) = (0, 0);
        for s in strips(&changed, width) {
            let ph = row_hashes(&self.prev, w, h, &[s]).0;
            let (ch, cf) = row_hashes(cur, w, h, &[s]);
            match self.find(&ph, &ch, &cf, expect) {
                Ok((d, ..)) => {
                    votes.push((s, d));
                    matched += 1;
                }
                Err(Push::NoMatch) => missed += 1,
                Err(_) => {}
            }
        }
        let runs = match vote(&votes, expect) {
            Some(d) => join(votes.iter().filter(|v| v.1 == d).map(|v| v.0).collect()),
            // No exact match anywhere: the tolerant one over all changed columns.
            None if missed > 0 => trim_right(&changed),
            None => return Push::Same,
        };
        if runs.is_empty() {
            return Push::Same;
        }
        if runs != self.runs {
            self.prev_hash = row_hashes(&self.prev, w, h, &runs).0;
            self.prev_sig = None;
            self.runs = runs;
        }
        let (ch, cf) = row_hashes(cur, w, h, &self.runs);
        let mut cs = None;
        let found = match self.find(&self.prev_hash, &ch, &cf, expect) {
            Err(Push::NoMatch) => {
                if self.prev_sig.is_none() {
                    self.prev_sig = Some(Sig::new(&self.prev, w, h, &self.runs));
                }
                let c = Sig::new(cur, w, h, &self.runs);
                let r = match &self.prev_sig {
                    Some(ps) => self.find_fuzzy(ps, &c, expect),
                    None => Err(Push::NoMatch),
                };
                cs = Some(c);
                r
            }
            other => other,
        };
        let (d, top, bot) = match found {
            Ok(x) => x,
            Err(e) => {
                self.note = format!(
                    "{total} columns changed, {} strips: {matched} matched, {missed} did not",
                    matched + missed
                );
                return e;
            }
        };
        // The result keeps the changed columns around the matched ones.
        for &(a, b) in &changed {
            if self.runs.iter().any(|r| a < r.1 && r.0 < b) {
                self.span = Some(match self.span {
                    Some((x0, x1)) => (x0.min(a), x1.max(b)),
                    None => (a, b),
                });
            }
        }
        self.header = top;
        self.footer = bot;
        let row = w * 4;
        // The last frame's rows under its moving part (its footer) are replaced by
        // the new rows and the new footer.
        let keep = self.y + h - bot;
        let mut add = d;
        let mut full = false;
        if self.y + h + d > self.max_h {
            add = self.max_h.saturating_sub(self.y + h);
            full = true;
        }
        let need = (keep + add + bot) * row;
        if need > self.out.capacity() {
            let grow = (need - self.out.len()).max(self.out.len() / 2);
            let cap = (self.max_h * row).max(need);
            self.out.reserve_exact(grow.min(cap - self.out.len()));
        }
        self.out.truncate(keep * row);
        let from = h - bot - d;
        opaque_rows(&mut self.out, &cur[from * row..(from + add) * row]);
        opaque_rows(&mut self.out, &cur[(h - bot) * row..h * row]);
        self.rows = keep + add + bot;
        self.y += add;
        self.changed = keep;
        self.prev.copy_from_slice(cur);
        self.prev_hash = ch;
        self.prev_sig = cs;
        if full {
            Push::Full
        } else {
            Push::Moved(d)
        }
    }

    /// Columns `[from, to)` the result keeps: those that scrolled, widened over
    /// plain columns next to them (page margins, a border line). A side part that
    /// stayed in place is left out: in the stitched rows it would be a stack of
    /// slices of itself.
    pub fn crop(&self) -> (usize, usize) {
        let Some((mut x0, mut x1)) = self.span else {
            return (0, self.w);
        };
        let (w, top, bot) = (self.w, self.header, self.h - self.footer);
        if top < bot {
            let px = |x: usize, y: usize| &self.prev[(y * w + x) * 4..(y * w + x) * 4 + 3];
            let plain = |x: usize| (top + 1..bot).all(|y| px(x, y) == px(x, top));
            while x0 > 0 && plain(x0 - 1) {
                x0 -= 1;
            }
            while x1 < w && plain(x1) {
                x1 += 1;
            }
        }
        (x0, x1)
    }

    /// The stitched image, cut to `crop()`: width, height, BGRA.
    pub fn finish(self) -> (usize, usize, Vec<u8>) {
        let (x0, x1) = self.crop();
        let (w, rows) = (self.w, self.rows);
        let mut out = self.out;
        out.truncate(rows * w * 4);
        let nw = x1 - x0;
        if nw < w {
            // In place: every row moves to an earlier or the same place.
            for y in 0..rows {
                out.copy_within((y * w + x0) * 4..(y * w + x1) * 4, y * nw * 4);
            }
            out.truncate(rows * nw * 4);
        }
        // Spare room from growing; shrinking a heap block does not move it.
        out.shrink_to_fit();
        (nw, rows, out)
    }
}

// ---------------------------------------------------------------- live preview

/// A small copy of the picture being stitched, shown while scrolling: it grows
/// like a panorama. Only rows that changed are recomputed (box filter), so the
/// cost per frame does not depend on how tall the picture already is.
pub struct Preview {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
    src_w: usize,
    /// Source pixels per preview pixel, both ways.
    step: f32,
    /// Source columns of each preview column: cols[i]..cols[i + 1].
    cols: Vec<usize>,
    acc: Vec<u32>,
    /// Source rows the last frame shows.
    pub view: (usize, usize),
    pub src_rows: usize,
}

impl Preview {
    /// At most `max_w` wide, and never taller than a GPU bitmap allows.
    pub fn new(src_w: usize, max_w: usize, max_src_h: usize) -> Preview {
        let step = (src_w as f32 / max_w as f32)
            .max(max_src_h as f32 / 8000.0)
            .max(1.0);
        let w = ((src_w as f32 / step) as usize).max(1);
        let cols = (0..=w)
            .map(|i| ((i as f32 * step).round() as usize).min(src_w))
            .collect();
        Preview {
            w,
            h: 0,
            px: Vec::new(),
            src_w,
            step,
            cols,
            acc: vec![0; w * 4],
            view: (0, 0),
            src_rows: 0,
        }
    }

    /// The source now has `rows` rows; rows from `from` on are new or changed.
    pub fn update(&mut self, src: &[u8], rows: usize, from: usize) {
        let step = self.step;
        let p0 = ((from as f32 / step) as usize)
            .saturating_sub(1)
            .min(self.h);
        let p1 = (rows as f32 / step).ceil() as usize;
        self.px.truncate(p0 * self.w * 4);
        self.px.resize(p1 * self.w * 4, 255);
        for py in p0..p1 {
            let y0 = (py as f32 * step) as usize;
            let y1 = (((py + 1) as f32 * step) as usize).clamp(y0 + 1, rows);
            if y0 >= rows {
                break;
            }
            self.acc.iter_mut().for_each(|a| *a = 0);
            for y in y0..y1 {
                let row = &src[y * self.src_w * 4..(y + 1) * self.src_w * 4];
                for c in 0..self.w {
                    let a = &mut self.acc[c * 4..c * 4 + 4];
                    for p in row[self.cols[c] * 4..self.cols[c + 1] * 4].chunks_exact(4) {
                        a[0] += p[0] as u32;
                        a[1] += p[1] as u32;
                        a[2] += p[2] as u32;
                        a[3] += 1;
                    }
                }
            }
            let out = &mut self.px[py * self.w * 4..(py + 1) * self.w * 4];
            for c in 0..self.w {
                let a = &self.acc[c * 4..c * 4 + 4];
                let n = a[3].max(1);
                out[c * 4] = (a[0] / n) as u8;
                out[c * 4 + 1] = (a[1] / n) as u8;
                out[c * 4 + 2] = (a[2] / n) as u8;
                out[c * 4 + 3] = 255;
            }
        }
        self.h = p1;
        self.src_rows = rows;
    }

    /// Preview rows of source rows.
    pub fn row_of(&self, y: usize) -> f32 {
        y as f32 / self.step
    }
}

// ---------------------------------------------------------------- screen

/// Grabs one screen rectangle again and again into the same DIB.
struct Grabber {
    screen: HDC,
    mem: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    r: RectI,
}

impl Grabber {
    fn new(r: RectI) -> Option<Grabber> {
        unsafe {
            let screen = GetDC(None);
            let mem = CreateCompatibleDC(screen);
            let mut bmi = BITMAPINFO::default();
            bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: r.w(),
                biHeight: -r.h(),
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let Ok(dib) = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) else {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return None;
            };
            let old = SelectObject(mem, dib);
            Some(Grabber {
                screen,
                mem,
                dib,
                old,
                bits: bits as *mut u8,
                r,
            })
        }
    }

    fn grab(&mut self) -> &[u8] {
        unsafe {
            let _ = BitBlt(
                self.mem,
                0,
                0,
                self.r.w(),
                self.r.h(),
                self.screen,
                self.r.l,
                self.r.t,
                SRCCOPY,
            );
            let _ = GdiFlush();
            std::slice::from_raw_parts(self.bits, (self.r.w() * self.r.h() * 4) as usize)
        }
    }
}

impl Drop for Grabber {
    fn drop(&mut self) {
        unsafe {
            SelectObject(self.mem, self.old);
            let _ = DeleteObject(self.dib);
            let _ = DeleteDC(self.mem);
            ReleaseDC(None, self.screen);
        }
    }
}

// ---------------------------------------------------------------- session

#[derive(Clone, Copy, PartialEq, Debug)]
enum End {
    Stopped,
    Limit,
    NoScroll,
    Failed,
}

struct Outcome {
    /// Left edge of the result in the area (side parts that did not scroll are cut).
    x0: usize,
    w: usize,
    h: usize,
    px: Vec<u8>,
    end: End,
    /// Frames that changed, were added, did not match.
    seen: u32,
    added: u32,
    missed: u32,
}

struct Shared {
    stop: AtomicBool,
    height: AtomicUsize,
    /// The last frames did not match: scrolled too fast, or up.
    lost: AtomicBool,
    preview: std::sync::Mutex<Preview>,
}

struct Session {
    hwnd: HWND,
    main: HWND,
    cfg: Config,
    area: RectI,
    monitor: RectI,
    scale: f32,
    shared: Arc<Shared>,
    hover_stop: bool,
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

const WM_APP_SCROLL_PROGRESS: u32 = WM_APP + 70;
const WM_APP_SCROLL_DONE: u32 = WM_APP + 71;
/// The panel next to the area, logical pixels.
const PANEL_W: f32 = 280.0;
/// Widest live preview, in pixels.
const PREVIEW_W: usize = 240;

pub fn is_running() -> bool {
    SESSION.with(|s| s.try_borrow().map(|s| s.is_some()).unwrap_or(true))
}

/// Starts a scrolling capture of `area` (screen pixels).
pub fn start(main: HWND, cfg: Config, area: RectI) {
    if is_running() {
        return;
    }
    if area.w() < 40 || area.h() < 80 {
        crate::notify_public(
            "Select a larger area",
            "Scrolling capture needs an area at least 80 pixels high.",
        );
        return;
    }
    unsafe {
        let center = POINT {
            x: area.l + area.w() / 2,
            y: area.t + area.h() / 2,
        };
        let mon = MonitorFromPoint(center, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let scale = dx as f32 / 96.0;
        let m = mi.rcMonitor;
        let wa = mi.rcWork;
        let monitor = RectI::new(m.left, m.top, m.right, m.bottom);

        // The window to scroll gets the focus, for "scroll inactive windows" off.
        let target = WindowFromPoint(center);
        if !target.0.is_null() {
            let root = GetAncestor(target, GA_ROOT);
            let _ = SetForegroundWindow(if root.0.is_null() { target } else { root });
        }

        register();
        // A tall panel beside the area, where the picture grows as you scroll.
        let gap = (12.0 * scale) as i32;
        let bw = (PANEL_W * scale) as i32;
        let bh = wa.bottom - wa.top - 2 * gap;
        let x = if wa.right - area.r >= bw + 2 * gap {
            area.r + gap
        } else if area.l - wa.left >= bw + 2 * gap {
            area.l - gap - bw
        } else {
            wa.right - bw - gap
        };
        let y = wa.top + gap;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("RefractScreenerScroll"),
            w!("Refract-screener scrolling capture"),
            WS_POPUP,
            x,
            y,
            bw,
            bh,
            None,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        ) else {
            return;
        };
        // Never in the frames, even when it has to sit over the area.
        let _ = SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE);
        dark_frame(hwnd);
        let round: i32 = 2; // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const _,
            4,
        );
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

        let shared = Arc::new(Shared {
            stop: AtomicBool::new(false),
            height: AtomicUsize::new(area.h() as usize),
            lost: AtomicBool::new(false),
            preview: std::sync::Mutex::new(Preview::new(area.w() as usize, PREVIEW_W, MAX_H)),
        });
        SESSION.with(|s| {
            *s.borrow_mut() = Some(Session {
                hwnd,
                main,
                cfg,
                area,
                monitor,
                scale,
                shared: shared.clone(),
                hover_stop: false,
            })
        });
        crate::hotkey::catch_escape(Some(hwnd));
        log(&format!("scrolling capture of {area:?}"));
        let to = hwnd_to(hwnd);
        std::thread::Builder::new()
            .name("scroll".into())
            .spawn(move || {
                let out = run(area, &shared, to);
                let p = Box::into_raw(Box::new(out));
                if PostMessageW(
                    hwnd_from(to),
                    WM_APP_SCROLL_DONE,
                    WPARAM(0),
                    LPARAM(p as isize),
                )
                .is_err()
                {
                    drop(Box::from_raw(p));
                }
            })
            .ok();
    }
}

fn run(area: RectI, shared: &Shared, to: isize) -> Outcome {
    let (w, h) = (area.w() as usize, area.h() as usize);
    let cols = w - (w / 16).min(20);
    let Some(mut g) = Grabber::new(area) else {
        return Outcome {
            x0: 0,
            w,
            h: 0,
            px: Vec::new(),
            end: End::Failed,
            seen: 0,
            added: 0,
            missed: 0,
        };
    };
    // Let the selection window disappear from the screen first.
    std::thread::sleep(Duration::from_millis(150));
    let first = g.grab();
    let all = [(0, cols)];
    let mut last = Sig::new(first, w, h, &all);
    let mut st = Stitcher::new(first, w, h, MAX_H);
    let show = |st: &Stitcher| {
        if let Ok(mut p) = shared.preview.lock() {
            p.update(st.pixels(), st.height(), st.changed());
            p.view = st.view();
        }
    };
    show(&st);
    let notify = || unsafe {
        let _ = PostMessageW(hwnd_from(to), WM_APP_SCROLL_PROGRESS, WPARAM(0), LPARAM(0));
    };
    notify();
    let mut expect = None;
    let mut moved_once = false;
    let mut misses = 0;
    let (mut seen, mut added, mut missed) = (0u32, 0u32, 0u32);
    let end = loop {
        if shared.stop.load(Ordering::Relaxed) {
            break End::Stopped;
        }
        std::thread::sleep(Duration::from_millis(30));
        let frame = g.grab();
        let sig = Sig::new(frame, w, h, &all);
        if sig.diff(&last) <= sig.still() {
            // Nothing new: a caret, a small animation, or no scrolling.
            continue;
        }
        seen += 1;
        match st.push(frame, expect) {
            Push::Moved(d) => {
                added += 1;
                moved_once = true;
                expect = Some(d);
                last = sig;
                misses = 0;
                shared.lost.store(false, Ordering::Relaxed);
                shared.height.store(st.height(), Ordering::Relaxed);
                show(&st);
                notify();
            }
            Push::Same => last = sig,
            Push::NoMatch => {
                missed += 1;
                if missed <= 3 || missed % 100 == 0 {
                    log(&format!(
                        "scrolling capture: frame not matched ({})",
                        st.note
                    ));
                }
                // Scrolled up, or too far between two looks: wait until the page
                // is back near the last frame. Before the first match there is
                // nothing to go back to.
                misses += 1;
                if misses == 4 && moved_once {
                    shared.lost.store(true, Ordering::Relaxed);
                    notify();
                }
            }
            Push::Full => {
                shared.height.store(st.height(), Ordering::Relaxed);
                show(&st);
                break End::Limit;
            }
        }
    };
    let end = if end == End::Stopped && !moved_once {
        End::NoScroll
    } else {
        end
    };
    let x0 = st.crop().0;
    let (w, h, px) = st.finish();
    Outcome {
        x0,
        w,
        h,
        px,
        end,
        seen,
        added,
        missed,
    }
}

fn register() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            lpszClassName: w!("RefractScreenerScroll"),
            ..Default::default()
        };
        RegisterClassExW(&wc);
    });
}

fn stop_rect(w: i32, h: i32, k: f32) -> RECT {
    let m = (16.0 * k) as i32;
    rect(m, h - m - (36.0 * k) as i32, w - m, h - m)
}

fn session<R>(f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSION.with(|s| s.try_borrow_mut().ok().and_then(|mut s| s.as_mut().map(f)))
}

fn request_stop(why: &str) {
    log(&format!("scrolling capture: stop ({why})"));
    session(|s| s.shared.stop.store(true, Ordering::Relaxed));
    session(|s| unsafe {
        let _ = InvalidateRect(s.hwnd, None, FALSE);
    });
}

unsafe fn draw(dc: HDC, rc: RECT) {
    let Some((k, height, stopping, lost, hover, shared)) = session(|s| {
        (
            s.scale,
            s.shared.height.load(Ordering::Relaxed),
            s.shared.stop.load(Ordering::Relaxed),
            s.shared.lost.load(Ordering::Relaxed),
            s.hover_stop,
            s.shared.clone(),
        )
    }) else {
        return;
    };
    let Ok(p) = shared.preview.lock() else { return };
    draw_panel(dc, rc, k, &p, height, stopping, lost, hover);
}

/// Box filter by a whole factor: a much smaller preview stays readable.
fn shrink(px: &[u8], w: usize, h: usize, f: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = ((w / f).max(1), (h / f).max(1));
    let mut out = vec![255u8; nw * nh * 4];
    for y in 0..nh {
        for x in 0..nw {
            let mut s = [0u32; 3];
            for yy in y * f..((y + 1) * f).min(h) {
                for xx in x * f..((x + 1) * f).min(w) {
                    let i = (yy * w + xx) * 4;
                    s[0] += px[i] as u32;
                    s[1] += px[i + 1] as u32;
                    s[2] += px[i + 2] as u32;
                }
            }
            let n = (f * f) as u32;
            let o = (y * nw + x) * 4;
            out[o] = (s[0] / n) as u8;
            out[o + 1] = (s[1] / n) as u8;
            out[o + 2] = (s[2] / n) as u8;
        }
    }
    (out, nw, nh)
}

#[allow(clippy::too_many_arguments)]
unsafe fn draw_panel(
    _dc: HDC,
    rc: RECT,
    k: f32,
    p: &Preview,
    height: usize,
    stopping: bool,
    lost: bool,
    hover: bool,
) {
    let px = |v: f32| (v * k) as i32;
    let red = rgb(0xFF, 0x9A, 0xA4);
    fill(_dc, rc, BG);
    ellipse(
        _dc,
        rect(px(16.0), px(18.0), px(24.0), px(26.0)),
        if stopping {
            DIM
        } else if lost {
            red
        } else {
            MINT
        },
    );
    let mut t = rect(px(32.0), px(10.0), rc.right - px(12.0), px(30.0));
    text(
        _dc,
        if stopping {
            "Finishing..."
        } else if lost {
            "Scroll back to where it stopped"
        } else {
            "Scroll the page down"
        },
        &mut t,
        13.5 * k,
        600,
        INK,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let mut t = rect(px(32.0), px(30.0), rc.right - px(12.0), px(50.0));
    let height = group(height);
    text(
        _dc,
        &format!("{height} px  \u{00B7}  Esc or Done to finish"),
        &mut t,
        12.0 * k,
        400,
        DIM,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let b = stop_rect(rc.right, rc.bottom, k);
    button(_dc, b, "Done", true, hover, k);

    // The picture so far, as one image: width fitted, then shrunk to the height.
    let inner = (
        px(16.0) as f32,
        px(62.0) as f32,
        (rc.right - px(16.0)) as f32,
        (b.top - px(14.0)) as f32,
    );
    let (iw, ih) = (inner.2 - inner.0, inner.3 - inner.1);
    if p.h == 0 || iw <= 0.0 || ih <= 0.0 {
        return;
    }
    let s = (iw / p.w as f32).min(ih / p.h as f32);
    let (dw, dh) = (p.w as f32 * s, p.h as f32 * s);
    let left = inner.0 + ((iw - dw) * 0.5).round();
    let top = inner.1;
    let f = (1.0 / s).floor() as usize;
    let bmp = if f >= 2 {
        let (v, w, h) = shrink(&p.px, p.w, p.h, f);
        bitmap(&v, w as u32, h as u32)
    } else {
        bitmap(&p.px, p.w as u32, p.h as u32)
    };
    let dst = D2D_RECT_F {
        left,
        top,
        right: left + dw,
        bottom: top + dh,
    };
    if let Some(bmp) = bmp {
        image(&bmp, dst, 5.0 * k);
    }
    // Where the page is now: a frame around the part on screen.
    let y0 = top + p.row_of(p.view.0) * s;
    let y1 = top + p.row_of(p.view.1) * s;
    let edge = if lost { red } else { MINT };
    let Some(b) = brush_a(edge, 1.0) else { return };
    let r = D2D1_ROUNDED_RECT {
        rect: D2D_RECT_F {
            left: dst.left - 1.0,
            top: y0 - 1.0,
            right: dst.right + 1.0,
            bottom: y1.min(dst.bottom) + 1.0,
        },
        radiusX: 5.0 * k,
        radiusY: 5.0 * k,
    };
    with_rt(|rt| rt.DrawRoundedRectangle(&r, &b, 2.0 * k, None));
}

/// A picture of the panel as it looks while scrolling (--ui-shots).
pub fn render_png(dir: &std::path::Path, px: &[u8], w: usize, h: usize) -> Result<(), String> {
    let k = 1.25;
    let mut p = Preview::new(w, PREVIEW_W, MAX_H);
    // As if scrolled about two thirds of the way.
    let rows = h * 2 / 3;
    p.update(&px[..rows * w * 4], rows, 0);
    let view_h = 900.min(rows);
    p.view = (rows - view_h, rows);
    let (pw, ph) = ((PANEL_W * k) as i32, (900.0 * k) as i32);
    let out = paint_offscreen(pw, ph, |dc, rc| unsafe {
        draw_panel(dc, rc, k, &p, rows, false, false, false)
    })
    .ok_or("could not draw the panel")?;
    let img = crate::encode::Image {
        w: pw as u32,
        h: ph as u32,
        bgra: out,
    };
    let data = crate::encode::png(&img, 0)?;
    std::fs::write(dir.join("scroll-panel.png"), data).map_err(|e| e.to_string())
}

/// 12345 -> "12 345".
fn group(n: usize) -> String {
    let s = n.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i) % 3 == 0 {
            out.push('\u{2009}');
        }
        out.push(c);
    }
    out
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h, |dc, rc| draw(dc, rc));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_MOUSEMOVE => {
            let (x, y) = lparam_xy(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            let k = session(|s| s.scale).unwrap_or(1.0);
            let on = inside(&stop_rect(rc.right, rc.bottom, k), x, y);
            if session(|s| std::mem::replace(&mut s.hover_stop, on)) != Some(on) {
                let _ = InvalidateRect(h, None, FALSE);
            }
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: h,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            session(|s| s.hover_stop = false);
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_SETCURSOR => {
            set_cursor(session(|s| s.hover_stop).unwrap_or(false));
            LRESULT(1)
        }
        WM_LBUTTONUP => {
            let (x, y) = lparam_xy(lp);
            let mut rc = RECT::default();
            let _ = GetClientRect(h, &mut rc);
            let k = session(|s| s.scale).unwrap_or(1.0);
            if inside(&stop_rect(rc.right, rc.bottom, k), x, y) {
                request_stop("Done");
            }
            LRESULT(0)
        }
        crate::hotkey::WM_APP_KEY_ESCAPE => {
            request_stop("Esc");
            LRESULT(0)
        }
        WM_APP_SCROLL_PROGRESS => {
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_APP_SCROLL_DONE => {
            let out = *Box::from_raw(lp.0 as *mut Outcome);
            finish(out);
            LRESULT(0)
        }
        WM_DISPLAYCHANGE => {
            request_stop("display changed");
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

fn finish(out: Outcome) {
    crate::hotkey::catch_escape(None);
    let Some(s) = SESSION.with(|s| s.borrow_mut().take()) else {
        return;
    };
    unsafe {
        let _ = DestroyWindow(s.hwnd);
    }
    log(&format!(
        "scrolling capture: {:?}, {}x{} from x {}; frames changed {}, added {}, not matched {}",
        out.end, out.w, out.h, out.x0, out.seen, out.added, out.missed
    ));
    if out.h == 0 {
        crate::notify_public("Scrolling capture failed", "Could not read the screen.");
        return;
    }
    let note = match out.end {
        End::NoScroll => Some("The page did not scroll: this is a normal screenshot".to_string()),
        End::Limit => Some(format!("Stopped at the {} px limit", group(MAX_H))),
        End::Failed | End::Stopped => None,
    };
    let (w, h) = (out.w as i32, out.h as i32);
    let full = RectI::new(0, 0, w, h);
    let shot = Shot {
        ox: s.area.l + out.x0 as i32,
        oy: s.area.t,
        w,
        h,
        px: out.px,
        windows: Vec::new(),
        monitors: vec![Monitor {
            rect: full,
            work: full,
            scale: s.scale,
        }],
        foreground: None,
    };
    if let Err(e) = crate::overlay::open_tall(s.main, s.cfg, shot, s.monitor, note) {
        log(&format!("tall editor failed: {e}"));
        crate::notify_public("Cannot open the editor", &e.message());
    }
}

// ---------------------------------------------------------------- OCR

/// OCR of a tall selection in bands (Windows OCR reads at most a few thousand
/// pixels per side), cut at blank rows and merged in reading order.
pub fn spawn_ocr_bands(hwnd: isize, gen: u64, sel: RectI, bgra: Vec<u8>, lang: String) {
    use crate::ocr::{self, OcrReply};
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
        }
        let (w, h) = (sel.w() as usize, sel.h() as usize);
        let (_, flat) = row_hashes(&bgra, w, h, &[(0, w)]);
        let mut merged = OcrReply::default();
        let mut y0 = 0usize;
        while y0 < h {
            let mut y1 = (y0 + 1800).min(h);
            if y1 < h {
                // Cut through a blank row near the target, never through text.
                if let Some(b) = (y1.saturating_sub(300)..(y1 + 300).min(h))
                    .filter(|&y| flat[y])
                    .min_by_key(|&y| y.abs_diff(y1))
                {
                    y1 = b.max(y0 + 200);
                }
            }
            let band = RectI::new(sel.l, sel.t + y0 as i32, sel.r, sel.t + y1 as i32);
            match ocr::recognize(&bgra[y0 * w * 4..y1 * w * 4], band, &lang) {
                Ok(r) => {
                    let (wo, lo) = (merged.words.len(), merged.lines.len());
                    let ro = merged.lines.iter().map(|l| l.row + 1).max().unwrap_or(0);
                    merged.words.extend(r.words.into_iter().map(|mut x| {
                        x.line += lo;
                        x
                    }));
                    merged.lines.extend(r.lines.into_iter().map(|mut l| {
                        l.first_word += wo;
                        l.row += ro;
                        l
                    }));
                    merged.blocks.extend(r.blocks);
                    if merged.lang.is_empty() {
                        merged.lang = r.lang;
                    }
                    if merged.hint.is_none() {
                        merged.hint = r.hint;
                    }
                }
                Err(e) => {
                    merged.error = Some(e);
                    break;
                }
            }
            y0 = y1;
        }
        merged.gen = gen;
        unsafe {
            let p = Box::into_raw(Box::new(merged));
            if PostMessageW(hwnd_from(hwnd), WM_APP_OCR, WPARAM(0), LPARAM(p as isize)).is_err() {
                drop(Box::from_raw(p));
            }
        }
    });
}

// ---------------------------------------------------------------- self test

/// Synthetic long page: a sticky header and footer, text-like lines, blank gaps
/// and repeated rows. Scrolls it with uneven steps and checks the stitched
/// result byte for byte. Returns a report line, Err on a mismatch.
pub fn stitch_test() -> Result<String, String> {
    let (w, header, footer, view) = (360usize, 44usize, 26usize, 300usize);
    let content_h = 4200usize;
    let mut seed = 0x1234_5678u32;
    let mut rnd = move || {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        seed
    };
    let px = |c: u32| {
        [
            (c & 0xFF) as u8,
            (c >> 8 & 0xFF) as u8,
            (c >> 16 & 0xFF) as u8,
            0u8,
        ]
    };
    let mut content = vec![0u8; w * content_h * 4];
    let mut y = 0;
    while y < content_h {
        let kind = rnd() % 10;
        let lh = 10 + (rnd() % 12) as usize;
        for r in y..(y + lh).min(content_h) {
            for x in 0..w {
                let c = match kind {
                    // Blank gap.
                    0..=2 => 0xFFFFFF,
                    // A coloured band with identical rows.
                    3 => 0xE8F0FF,
                    // Text: dark specks on white.
                    _ => {
                        if (x * 7 + r * 13 + (rnd() as usize % 5)) % 9 == 0 {
                            0x202020 + (rnd() % 64)
                        } else {
                            0xFFFFFF
                        }
                    }
                };
                content[(r * w + x) * 4..(r * w + x) * 4 + 4].copy_from_slice(&px(c));
            }
        }
        y += lh;
    }
    let bar = |rows: usize, base: u32, rnd: &mut dyn FnMut() -> u32| {
        let mut v = vec![0u8; w * rows * 4];
        for r in 0..rows {
            for x in 0..w {
                let c = if (x / 9 + r / 5) % 4 == 0 {
                    base ^ (rnd() % 32)
                } else {
                    base
                };
                v[(r * w + x) * 4..(r * w + x) * 4 + 4].copy_from_slice(&px(c));
            }
        }
        v
    };
    let head = bar(header, 0x2B6CB0, &mut rnd);
    let foot = bar(footer, 0x333A40, &mut rnd);
    let inner = view - header - footer;
    let frame = |off: usize| {
        let mut f = Vec::with_capacity(w * view * 4);
        f.extend_from_slice(&head);
        f.extend_from_slice(&content[off * w * 4..(off + inner) * w * 4]);
        f.extend_from_slice(&foot);
        f
    };
    let opaque = |mut v: Vec<u8>| {
        for a in v.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        v
    };

    let mut expected = Vec::new();
    expected.extend_from_slice(&head);
    expected.extend_from_slice(&content);
    expected.extend_from_slice(&foot);
    let expected = opaque(expected);

    let last = content_h - inner;
    let mut st = Stitcher::new(&frame(0), w, view, MAX_H);
    let mut off = 0;
    let mut steps = 0;
    let mut step_rnd = 0x9E37_79B9u32;
    loop {
        step_rnd = step_rnd.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let step = 40 + (step_rnd >> 16) as usize % 100;
        let next = (off + step).min(last);
        // Half of the steps without a hint: the match alone must find them.
        let hint = (steps % 2 == 1).then_some(step);
        let r = st.push(&frame(next), hint);
        if next == off {
            if r != Push::Same {
                return Err(format!("no scroll at the end gave {r:?}"));
            }
            break;
        }
        if r != Push::Moved(next - off) {
            return Err(format!("step {steps}: moved {} but got {r:?}", next - off));
        }
        off = next;
        steps += 1;
    }
    let (rw, rh, out) = st.finish();
    if rw != w || rh != header + content_h + footer || out != expected {
        return Err(format!(
            "stitched {rw}x{rh}, expected {w}x{}; first different byte {:?}",
            header + content_h + footer,
            out.iter().zip(&expected).position(|(a, b)| a != b)
        ));
    }

    // Scrolling by hand: small uneven steps, now and then a little back up.
    // Frames from scrolling up must change nothing; the result stays exact.
    let mut st = Stitcher::new(&frame(0), w, view, MAX_H);
    // The live preview follows along, updated only where rows changed.
    let mut live = Preview::new(w, 100, MAX_H);
    live.update(st.pixels(), st.height(), 0);
    let (mut pos, mut done, mut hand) = (0usize, 0usize, 0x2468_ACE1u32);
    let mut ups = 0;
    while done < last {
        hand = hand.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let r = (hand >> 16) as usize;
        pos = if r % 7 == 0 && pos > 50 {
            ups += 1;
            pos - 10 - r % 40
        } else {
            (pos + 3 + r % 60).min(last)
        };
        match st.push(&frame(pos), None) {
            Push::Moved(d) => {
                if pos != done + d {
                    return Err(format!("by hand: at {pos} moved {d} from {done}"));
                }
                done = pos;
                live.update(st.pixels(), st.height(), st.changed());
            }
            Push::Same | Push::NoMatch if pos <= done => {}
            other => return Err(format!("by hand: at {pos} (done {done}) got {other:?}")),
        }
    }
    let mut fresh = Preview::new(w, 100, MAX_H);
    fresh.update(st.pixels(), st.height(), 0);
    if live.h != fresh.h || live.px != fresh.px {
        return Err(format!(
            "live preview {}x{} differs from a fresh one {}x{}",
            live.w, live.h, fresh.w, fresh.h
        ));
    }
    let (_, hh, out) = st.finish();
    if hh != rh || out != expected {
        return Err(format!("by hand: {hh} rows, not identical"));
    }

    // The height limit: cut to MAX, footer kept.
    let limit = 1500;
    let mut st = Stitcher::new(&frame(0), w, view, limit);
    let mut off = 0;
    loop {
        let next = (off + 120).min(last);
        if st.push(&frame(next), None) == Push::Full {
            break;
        }
        off = next;
    }
    let (_, lh, out) = st.finish();
    let mut want = expected[..(limit - footer) * w * 4].to_vec();
    want.extend_from_slice(&expected[expected.len() - footer * w * 4..]);
    if lh != limit || out != want {
        return Err(format!("limit: got {lh} rows, expected {limit}"));
    }

    // A translucent window: a smooth background that does not scroll shows
    // through, and noise changes every frame. No row ever repeats exactly; the
    // tolerant match must still find every shift.
    let mut noise = 0xC0FF_EE11u32;
    let mut glass = |off: usize| {
        let mut f = frame(off);
        for y in 0..view {
            for x in 0..w {
                let bg = 128.0 + 70.0 * ((x as f32 / 53.0) + (y as f32 / 41.0)).sin();
                let i = (y * w + x) * 4;
                for c in 0..3 {
                    noise ^= noise << 13;
                    noise ^= noise >> 17;
                    noise ^= noise << 5;
                    let n = (noise % 7) as f32 - 3.0;
                    let v = f[i + c] as f32 * 0.78 + bg * 0.22 + n;
                    f[i + c] = v.clamp(0.0, 255.0) as u8;
                }
            }
        }
        f
    };
    let mut st = Stitcher::new(&glass(0), w, view, MAX_H);
    let (mut off, mut gsteps) = (0, 0);
    let mut step_rnd = 0x1357_9BDFu32;
    loop {
        step_rnd = step_rnd.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let step = 40 + (step_rnd >> 16) as usize % 100;
        let next = (off + step).min(last);
        let hint = (gsteps % 2 == 1).then_some(step);
        let r = st.push(&glass(next), hint);
        if next == off {
            if r != Push::Same {
                return Err(format!("translucent: no scroll at the end gave {r:?}"));
            }
            break;
        }
        if r != Push::Moved(next - off) {
            return Err(format!(
                "translucent step {gsteps}: moved {} but got {r:?}",
                next - off
            ));
        }
        off = next;
        gsteps += 1;
    }
    let (_, gh, _) = st.finish();
    if gh != rh {
        return Err(format!("translucent: {gh} rows, expected {rh}"));
    }

    // A whole window or screen: a site menu on the left and a panel on the
    // right stay in place while only the page between them scrolls. Every row
    // of those side parts is different, so whole rows never match; the match
    // must use the scrolling columns only, and the result is the page alone.
    let (left, right) = (52usize, 34usize);
    let ww = left + w + right;
    let side = |x: usize, y: usize| -> u32 {
        // Menu items: blocks of rows with a label-like pattern, unlike their neighbours.
        let item = y / 23;
        if (x + item * 5) % 7 < 3 && y % 23 > 5 {
            0x404850 + (item as u32 * 37 % 64)
        } else {
            0xEEF1F4 ^ (item as u32 * 13 % 16)
        }
    };
    let wide = |off: usize| {
        let f = frame(off);
        let mut v = vec![0u8; ww * view * 4];
        for y in 0..view {
            for x in 0..ww {
                let i = (y * ww + x) * 4;
                if (left..left + w).contains(&x) {
                    let j = (y * w + x - left) * 4;
                    v[i..i + 4].copy_from_slice(&f[j..j + 4]);
                } else {
                    v[i..i + 4].copy_from_slice(&px(side(x, y)));
                }
            }
        }
        v
    };
    let mut st = Stitcher::new(&wide(0), ww, view, MAX_H);
    let (mut off, mut ssteps) = (0, 0);
    let mut step_rnd = 0x0BAD_F00Du32;
    loop {
        step_rnd = step_rnd.wrapping_mul(1_103_515_245).wrapping_add(12345);
        let step = 20 + (step_rnd >> 16) as usize % 120;
        let next = (off + step).min(last);
        let r = st.push(&wide(next), None);
        if next == off {
            break;
        }
        if r != Push::Moved(next - off) {
            return Err(format!(
                "side parts, step {ssteps}: moved {} but got {r:?}",
                next - off
            ));
        }
        off = next;
        ssteps += 1;
    }
    let crop = st.crop();
    let (sw, sh, out) = st.finish();
    if crop != (left, left + w) || sw != w || sh != rh || out != expected {
        return Err(format!(
            "side parts: kept columns {crop:?} of {ww} ({sw}x{sh}), expected {:?} ({w}x{rh}); identical: {}",
            (left, left + w),
            out == expected
        ));
    }
    // A video plays next to the page and changes every frame. Its columns
    // match no shift; the page must still be found, and frames where only the
    // video changed are no scroll at all.
    let (gap, video) = (6usize, 220usize);
    let vw = w + gap + video;
    let mut film = 0xFACE_B00Cu32;
    let mut beside = |off: usize| {
        let f = frame(off);
        let mut v = vec![0u8; vw * view * 4];
        for y in 0..view {
            let i = y * vw * 4;
            v[i..i + w * 4].copy_from_slice(&f[y * w * 4..(y + 1) * w * 4]);
            for x in w..vw {
                let c = if x < w + gap {
                    0x30_3030 + (y as u32 % 50)
                } else {
                    film ^= film << 13;
                    film ^= film >> 17;
                    film ^= film << 5;
                    film & 0xFF_FFFF
                };
                v[i + x * 4..i + x * 4 + 4].copy_from_slice(&px(c));
            }
        }
        v
    };
    let mut st = Stitcher::new(&beside(0), vw, view, MAX_H);
    let (mut off, mut vsteps, mut vstill) = (0, 0, 0);
    let mut step_rnd = 0x5EED_1234u32;
    loop {
        step_rnd = step_rnd.wrapping_mul(1_103_515_245).wrapping_add(12345);
        if vsteps > 0 && step_rnd >> 28 == 0 {
            // A pause: only the video moves.
            let r = st.push(&beside(off), None);
            if r != Push::Same {
                return Err(format!("video: a pause at {off} gave {r:?}"));
            }
            vstill += 1;
            continue;
        }
        let step = 20 + (step_rnd >> 16) as usize % 120;
        let next = (off + step).min(last);
        if next == off {
            break;
        }
        let r = st.push(&beside(next), None);
        if r != Push::Moved(next - off) {
            return Err(format!(
                "video, step {vsteps}: moved {} but got {r:?} ({})",
                next - off,
                st.note
            ));
        }
        off = next;
        vsteps += 1;
    }
    let crop = st.crop();
    let (_, vh, out) = st.finish();
    if crop != (0, w) || vh != rh || out != expected {
        return Err(format!(
            "video: kept columns {crop:?} of {vw}, {vh} rows; identical: {}",
            out == expected
        ));
    }

    Ok(format!(
        "scroll stitch: {steps} steps, {w}x{rh}, sticky header {header} + footer {footer}: identical; \
         by hand with {ups} scrolls back up: identical, live preview too; translucent window with noise: {gsteps} steps, every shift found; \
         page between a fixed menu and a panel ({ww} px wide): {ssteps} steps, cut to the page, identical; \
         page next to a playing video: {vsteps} steps and {vstill} pauses, cut to the page, identical"
    ))
}

#[cfg(test)]
mod tests {
    #[test]
    fn stitch_synthetic_page() {
        let r = super::stitch_test();
        assert!(r.is_ok(), "{r:?}");
    }
}

#[cfg(test)]
mod panel_tests {
    #[test]
    fn panel_renders() {
        let (w, h) = (1100usize, 5500usize);
        let px: Vec<u8> = (0..w * h * 4).map(|i| (i % 251) as u8).collect();
        let dir = std::env::temp_dir();
        super::render_png(&dir, &px, w, h).unwrap();
    }
}
