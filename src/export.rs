//! Renders the selection with all annotations into a pixel buffer, 1:1 with the
//! physical screen pixels (no scaling, no resampling of the screenshot).

use crate::capture::Shot;
use crate::config::Tool;
use crate::effects;
use crate::encode::Image;
use crate::geom::{RectF, RectI};
use crate::ocr::Block;
use crate::paint::*;
use crate::shapes::Shape;
use std::collections::HashMap;
use windows::core::Result;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;

/// Blurred / pixelated copies of the screenshot, keyed by (tool, strength, band).
/// A normal screenshot is one band; a tall one (scrolling capture) is cut into
/// bands of BAND rows, computed only where an effect is drawn.
#[derive(Default)]
pub struct FxCache {
    pub cpu: HashMap<FxKey, Vec<u8>>,
    order: Vec<FxKey>,
    bytes: usize,
}

pub type FxKey = (Tool, u32, u32);

/// Band height of tall pictures.
pub const BAND: i32 = 1024;
/// Pictures up to this height are one band.
const ONE_BAND: i32 = 4096;
/// Memory for the cached copies of a tall picture.
const TALL_BYTES: usize = 192 << 20;

impl FxCache {
    pub fn quantize(tool: Tool, s: u32) -> u32 {
        let s = s.max(2);
        if tool == Tool::Blur {
            (s + 1) / 2 * 2
        } else {
            s
        }
    }
    fn band_h(shot: &Shot) -> i32 {
        if shot.h <= ONE_BAND {
            shot.h.max(1)
        } else {
            BAND
        }
    }
    /// Image rectangle of a band.
    pub fn band_rect(shot: &Shot, band: u32) -> RectI {
        let bh = Self::band_h(shot);
        let t = band as i32 * bh;
        RectI::new(0, t, shot.w, (t + bh).min(shot.h))
    }
    /// Bands that `r` (image coordinates) touches.
    pub fn bands(shot: &Shot, r: RectF) -> std::ops::Range<u32> {
        let bh = Self::band_h(shot);
        let t = (r.t.floor() as i32).clamp(0, shot.h - 1);
        let b = (r.b.ceil() as i32).clamp(t + 1, shot.h);
        (t / bh) as u32..((b - 1) / bh + 1) as u32
    }
    pub fn get(&mut self, shot: &Shot, tool: Tool, s: u32, band: u32) -> &Vec<u8> {
        let key = (tool, Self::quantize(tool, s), band);
        if !self.cpu.contains_key(&key) {
            let data = Self::compute(shot, key);
            self.bytes += data.len();
            self.cpu.insert(key, data);
            self.order.push(key);
            let tall = shot.h > ONE_BAND;
            while self.order.len() > 1
                && ((!tall && self.order.len() > 6) || (tall && self.bytes > TALL_BYTES))
            {
                let old = self.order.remove(0);
                if let Some(v) = self.cpu.remove(&old) {
                    self.bytes -= v.len();
                }
            }
        }
        &self.cpu[&key]
    }
    pub fn evicted(&self, key: &FxKey) -> bool {
        !self.cpu.contains_key(key)
    }
    /// One band of the effect, exactly as if it was computed on the whole picture:
    /// the blur reads 3 radii beyond the band, the mosaic starts on its block grid.
    fn compute(shot: &Shot, (tool, s, band): FxKey) -> Vec<u8> {
        let r = Self::band_rect(shot, band);
        let w = shot.w as usize;
        if r.t == 0 && r.b == shot.h {
            return if tool == Tool::Blur {
                effects::blur(&shot.px, w, shot.h as usize, s)
            } else {
                effects::pixelate(&shot.px, w, shot.h as usize, s)
            };
        }
        let (t, b) = if tool == Tool::Blur {
            let m = effects::blur_reach(s) as i32;
            ((r.t - m).max(0), (r.b + m).min(shot.h))
        } else {
            let bs = s.max(2) as i32;
            (r.t / bs * bs, ((r.b + bs - 1) / bs * bs).min(shot.h))
        };
        let src = &shot.px[t as usize * w * 4..b as usize * w * 4];
        let h = (b - t) as usize;
        let full = if tool == Tool::Blur {
            effects::blur(src, w, h, s)
        } else {
            effects::pixelate(src, w, h, s)
        };
        let from = (r.t - t) as usize * w * 4;
        full[from..from + r.h() as usize * w * 4].to_vec()
    }
}

pub fn bitmap_props() -> D2D1_BITMAP_PROPERTIES {
    D2D1_BITMAP_PROPERTIES {
        pixelFormat: D2D1_PIXEL_FORMAT {
            format: DXGI_FORMAT_B8G8R8A8_UNORM,
            alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
        },
        dpiX: 96.0,
        dpiY: 96.0,
    }
}

pub fn make_bitmap(rt: &ID2D1RenderTarget, px: &[u8], w: i32, h: i32) -> Result<ID2D1Bitmap> {
    make_bitmap_pitch(rt, px, w, h, w * 4)
}

/// A bitmap from rows `pitch` bytes apart (a part of a wider picture, no copy).
pub fn make_bitmap_pitch(
    rt: &ID2D1RenderTarget,
    px: &[u8],
    w: i32,
    h: i32,
    pitch: i32,
) -> Result<ID2D1Bitmap> {
    assert!(px.len() >= ((h - 1).max(0) * pitch + w * 4) as usize);
    unsafe {
        rt.CreateBitmap(
            D2D_SIZE_U {
                width: w as u32,
                height: h as u32,
            },
            Some(px.as_ptr() as *const _),
            pitch as u32,
            &bitmap_props(),
        )
    }
}

/// Rows rendered at a time: a tall picture is drawn in strips, straight into
/// the output buffer, so nothing the size of the whole picture is made twice.
const STRIP: i32 = 2048;

pub fn render(
    painter: &Painter,
    shot: &Shot,
    sel: RectI,
    shapes: &[Shape],
    fx: &mut FxCache,
    translation: Option<&[Block]>,
) -> Result<Image> {
    let sel = sel.clamp_to(&shot.full());
    let (w, h) = (sel.w().max(1) as u32, sel.h().max(1) as u32);

    // Fast path: nothing drawn, copy pixels directly.
    if shapes.is_empty() && translation.map_or(true, |t| t.iter().all(|b| b.translated.is_none())) {
        return Ok(Image {
            w,
            h,
            bgra: shot.crop(&sel),
        });
    }

    let mut bgra = vec![0u8; (w * h * 4) as usize];
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let strip = (h as i32).min(STRIP);
        let target = wic.CreateBitmap(
            w,
            strip as u32,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapCacheOnLoad,
        )?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = painter.d2d.CreateWicBitmapRenderTarget(&target, &props)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        let mut bitmaps: HashMap<FxKey, ID2D1Bitmap> = HashMap::new();
        let pitch = shot.w * 4;

        let mut y0 = 0i32;
        while y0 < h as i32 {
            let sh = strip.min(h as i32 - y0);
            let top = sel.t + y0;
            // Only this strip of the screenshot goes to the GPU, read in place.
            let at = ((top * shot.w + sel.l) * 4) as usize;
            let base = make_bitmap_pitch(&rt, &shot.px[at..], w as i32, sh, pitch)?;
            let view =
                crate::geom::RectF::new(sel.l as f32, top as f32, sel.r as f32, (top + sh) as f32);
            rt.BeginDraw();
            rt.SetTransform(&Matrix3x2::identity());
            rt.Clear(Some(&rgba(0.0, 0.0, 0.0, 1.0)));
            rt.DrawBitmap(
                &base,
                Some(&rc(crate::geom::RectF::new(0.0, 0.0, w as f32, sh as f32))),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                None,
            );
            rt.SetTransform(&Matrix3x2::translation(-sel.l as f32, -top as f32));
            if let Some(blocks) = translation {
                painter.draw_translation(&rt, blocks);
            }
            let mut src = |tool: Tool, s: u32, r: crate::geom::RectF| {
                let mut out = Vec::new();
                let r = crate::geom::RectF::new(r.l, r.t.max(view.t), r.r, r.b.min(view.b));
                if r.h() <= 0.0 {
                    return out;
                }
                for band in FxCache::bands(shot, r) {
                    let key = (tool, FxCache::quantize(tool, s), band);
                    let br = FxCache::band_rect(shot, band);
                    let b = match bitmaps.get(&key) {
                        Some(b) => b.clone(),
                        None => {
                            let px = fx.get(shot, tool, s, band);
                            let Ok(b) = make_bitmap(&rt, px, br.w(), br.h()) else {
                                continue;
                            };
                            bitmaps.insert(key, b.clone());
                            b
                        }
                    };
                    out.push((b, br.f()));
                }
                out
            };
            painter.draw_shapes(&rt, shapes, &mut src, None);
            rt.EndDraw(None, None)?;
            // Bitmaps of bands this strip no longer needs.
            bitmaps.retain(|k, _| FxCache::band_rect(shot, k.2).b > top + sh);
            let from = (y0 as u32 * w * 4) as usize;
            let part = WICRect {
                X: 0,
                Y: 0,
                Width: w as i32,
                Height: sh,
            };
            target.CopyPixels(
                &part,
                w * 4,
                &mut bgra[from..from + (sh as u32 * w * 4) as usize],
            )?;
            y0 += sh;
        }
    }
    for a in bgra.iter_mut().skip(3).step_by(4) {
        *a = 255;
    }
    Ok(Image { w, h, bgra })
}

/// Checks that blur / pixelate computed band by band on a tall picture is byte
/// for byte the same as on the whole picture (used by --selftest).
pub fn fx_band_test() -> std::result::Result<String, String> {
    let (w, h) = (283i32, 9100i32);
    let mut seed = 0x2545_F491u32;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for (i, b) in px.iter_mut().enumerate() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *b = if i % 4 == 3 { 255 } else { (seed >> 24) as u8 };
    }
    let shot = Shot {
        ox: 0,
        oy: 0,
        w,
        h,
        px,
        windows: Vec::new(),
        monitors: Vec::new(),
        foreground: None,
    };
    let mut fx = FxCache::default();
    for (tool, s) in [
        (Tool::Blur, 12u32),
        (Tool::Blur, 40),
        (Tool::Pixelate, 7),
        (Tool::Pixelate, 10),
    ] {
        let q = FxCache::quantize(tool, s);
        let whole = if tool == Tool::Blur {
            effects::blur(&shot.px, w as usize, h as usize, q)
        } else {
            effects::pixelate(&shot.px, w as usize, h as usize, q)
        };
        for band in FxCache::bands(&shot, shot.full().f()) {
            let r = FxCache::band_rect(&shot, band);
            let part = fx.get(&shot, tool, s, band);
            let from = (r.t * w * 4) as usize;
            if part[..] != whole[from..from + part.len()] {
                return Err(format!("{tool:?} {s}: band {band} differs"));
            }
        }
    }
    Ok(format!(
        "banded blur/pixelate on {w}x{h}: identical to whole-picture"
    ))
}

/// Renders a tall picture with shapes across strip and band borders and checks
/// that every row away from the shapes is the source, byte for byte, and that
/// the shapes were drawn (used by --selftest).
pub fn strip_test(painter: &Painter) -> std::result::Result<String, String> {
    use crate::config::Style;
    use crate::geom::p;
    use crate::shapes::Kind;
    let (w, h) = (400i32, 5200i32);
    let mut seed = 0x6A09_E667u32;
    let mut px = vec![0u8; (w * h * 4) as usize];
    for (i, b) in px.iter_mut().enumerate() {
        seed ^= seed << 13;
        seed ^= seed >> 17;
        seed ^= seed << 5;
        *b = if i % 4 == 3 { 255 } else { (seed >> 24) as u8 };
    }
    let shot = Shot {
        ox: 0,
        oy: 0,
        w,
        h,
        px,
        windows: Vec::new(),
        monitors: Vec::new(),
        foreground: None,
    };
    let style = Style::default();
    let shapes = vec![
        // Across the first strip border (2048).
        Shape {
            kind: Kind::Rect(p(50.0, 2000.0), p(300.0, 2100.0)),
            style: Style {
                shadow: false,
                ..style
            },
        },
        // Blur across an effect band border (4096) and the second strip border.
        Shape {
            kind: Kind::Blur(p(20.0, 4060.0), p(380.0, 4140.0)),
            style: Style {
                width: 12.0,
                ..style
            },
        },
    ];
    let mut fx = FxCache::default();
    let sel = shot.full();
    let img = render(painter, &shot, sel, &shapes, &mut fx, None).map_err(|e| e.to_string())?;
    if img.w as i32 != w || img.h as i32 != h {
        return Err(format!("strips: {}x{} instead of {w}x{h}", img.w, img.h));
    }
    let row = (w * 4) as usize;
    let same = |y: i32| {
        img.bgra[y as usize * row..(y as usize + 1) * row]
            == shot.px[y as usize * row..(y as usize + 1) * row]
    };
    for (a, b) in [(0, 1980), (2120, 4040), (4160, h)] {
        if let Some(y) = (a..b).find(|&y| !same(y)) {
            return Err(format!("strips: row {y} away from the shapes changed"));
        }
    }
    if same(2000) || same(2099) || same(4100) {
        return Err("strips: a shape is missing".into());
    }
    Ok(format!(
        "tall export in strips ({w}x{h}): untouched rows identical, shapes drawn"
    ))
}

/// Turned text: the export draws it inside its turned box, the hit test follows
/// that box, and a text turned by 90 degrees stands upright (used by --selftest).
pub fn text_rotate_test(painter: &Painter) -> std::result::Result<String, String> {
    use crate::config::{Fill, Rgb, Style};
    use crate::geom::{p, RectF};
    use crate::shapes::Kind;
    let (w, h) = (480i32, 480i32);
    let mut px = vec![0x40u8; (w * h * 4) as usize];
    for a in px.chunks_exact_mut(4) {
        a[3] = 255;
    }
    let shot = Shot {
        ox: 0,
        oy: 0,
        w,
        h,
        px,
        windows: Vec::new(),
        monitors: Vec::new(),
        foreground: None,
    };
    let mut fx = FxCache::default();
    let mut report = Vec::new();
    for angle in [0.0f32, 90.0, -30.0] {
        let shape = Shape {
            kind: Kind::Text {
                at: p(150.0, 220.0),
                text: "Turned text".into(),
                ext: Default::default(),
                angle,
                wrap: 0.0,
            },
            style: Style {
                color: Rgb(0xFF3B5C),
                width: 28.0,
                fill: Fill::Solid,
                shadow: false,
                ..Style::default()
            },
        };
        let shapes = [shape];
        let img = render(painter, &shot, shot.full(), &shapes, &mut fx, None)
            .map_err(|e| e.to_string())?;
        let s = &shapes[0];
        // Box of the pixels the text changed.
        let (mut l, mut t, mut r, mut b) = (w, h, -1, -1);
        for y in 0..h {
            for x in 0..w {
                let i = ((y * w + x) * 4) as usize;
                if img.bgra[i..i + 3] != shot.px[i..i + 3] {
                    l = l.min(x);
                    t = t.min(y);
                    r = r.max(x + 1);
                    b = b.max(y + 1);
                }
            }
        }
        if r < 0 {
            return Err(format!("turned text {angle}: nothing drawn"));
        }
        let drawn = RectF::new(l as f32, t as f32, r as f32, b as f32);
        let want = s.bounds();
        let inside = drawn.l >= want.l - 2.0
            && drawn.t >= want.t - 2.0
            && drawn.r <= want.r + 2.0
            && drawn.b <= want.b + 2.0;
        let filled = drawn.w() * drawn.h() >= want.w() * want.h() * 0.85;
        if !inside || !filled {
            return Err(format!(
                "turned text {angle}: drawn {drawn:?}, its box says {want:?}"
            ));
        }
        if angle == 90.0 && drawn.h() <= drawn.w() {
            return Err(format!("turned text 90: not upright, {drawn:?}"));
        }
        let c = p(want.cx(), want.cy());
        if !s.hit(c, 0.0) {
            return Err(format!("turned text {angle}: centre does not hit"));
        }
        // The corner of the outer box lies outside a turned text.
        if angle == -30.0 && s.hit(p(want.l + 3.0, want.t + 3.0), 0.0) {
            return Err("turned text -30: hit outside the turned box".into());
        }
        report.push(format!("{angle}\u{00B0} {}x{}", drawn.w() as i32, drawn.h() as i32));
    }
    Ok(format!(
        "turned text drawn and hit-tested inside its box: {}",
        report.join(", ")
    ))
}
