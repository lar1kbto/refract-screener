//! Drawing kit for the Refract-styled utility windows (settings, update card).
//! Everything is drawn with Direct2D (anti-aliased shapes) and DirectWrite (text)
//! through a DC render target bound to the window during WM_PAINT. The `dc`
//! parameters are kept for call-site symmetry and are not used.

use std::cell::RefCell;
use std::collections::HashMap;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::Graphics::Imaging::*;
use windows::Win32::System::Com::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const BG: COLORREF = rgb(0x0A, 0x0F, 0x0D);
pub const PANEL: COLORREF = rgb(0x11, 0x18, 0x15);
pub const EDGE: COLORREF = rgb(0x22, 0x2E, 0x29);
pub const INK: COLORREF = rgb(0xEE, 0xF5, 0xF1);
pub const DIM: COLORREF = rgb(0x8A, 0x9A, 0x92);
pub const FAINT: COLORREF = rgb(0x5C, 0x6B, 0x64);
pub const MINT: COLORREF = rgb(0x6F, 0xF2, 0xB8);
pub const MINT_HI: COLORREF = rgb(0x8C, 0xF7, 0xC8);
pub const ON_MINT: COLORREF = rgb(0x0A, 0x1A, 0x12);

pub const fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

pub fn rect(l: i32, t: i32, r: i32, b: i32) -> RECT {
    RECT {
        left: l,
        top: t,
        right: r,
        bottom: b,
    }
}

pub fn inside(r: &RECT, x: i32, y: i32) -> bool {
    x >= r.left && x < r.right && y >= r.top && y < r.bottom
}

struct Ctx {
    dw: IDWriteFactory,
    rt: ID2D1DCRenderTarget,
    /// Logo pixels (premultiplied BGRA) per size, scaled once with a high-quality filter.
    logos: HashMap<i32, Vec<u8>>,
}

thread_local! {
    static CTX: RefCell<Option<Ctx>> = const { RefCell::new(None) };
}

thread_local! {
    /// Bumped whenever the render target is (re)created: bitmaps made on the old one are gone.
    static GEN: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

pub fn generation() -> u32 {
    GEN.with(|g| g.get())
}

fn ctx<R>(f: impl FnOnce(&mut Ctx) -> R) -> Option<R> {
    CTX.with(|c| c.borrow_mut().as_mut().map(f))
}

fn init() -> windows::core::Result<()> {
    if CTX.with(|c| c.borrow().is_some()) {
        return Ok(());
    }
    unsafe {
        let d2d: ID2D1Factory = D2D1CreateFactory(
            D2D1_FACTORY_TYPE_SINGLE_THREADED,
            Some(&D2D1_FACTORY_OPTIONS::default()),
        )?;
        let dw: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_GDI_COMPATIBLE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = d2d.CreateDCRenderTarget(&props)?;
        rt.SetAntialiasMode(D2D1_ANTIALIAS_MODE_PER_PRIMITIVE);
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_CLEARTYPE);
        GEN.with(|g| g.set(g.get() + 1));
        CTX.with(|c| {
            *c.borrow_mut() = Some(Ctx {
                dw,
                rt,
                logos: HashMap::new(),
            })
        });
    }
    Ok(())
}

fn col(c: COLORREF) -> D2D1_COLOR_F {
    D2D1_COLOR_F {
        r: (c.0 & 0xFF) as f32 / 255.0,
        g: ((c.0 >> 8) & 0xFF) as f32 / 255.0,
        b: ((c.0 >> 16) & 0xFF) as f32 / 255.0,
        a: 1.0,
    }
}

fn rf(r: &RECT) -> D2D_RECT_F {
    D2D_RECT_F {
        left: r.left as f32,
        top: r.top as f32,
        right: r.right as f32,
        bottom: r.bottom as f32,
    }
}

fn brush(rt: &ID2D1DCRenderTarget, c: COLORREF) -> Option<ID2D1SolidColorBrush> {
    unsafe { rt.CreateSolidColorBrush(&col(c), None).ok() }
}

pub unsafe fn fill(_dc: HDC, r: RECT, c: COLORREF) {
    ctx(|x| {
        if let Some(b) = brush(&x.rt, c) {
            x.rt.FillRectangle(&rf(&r), &b);
        }
    });
}

/// Filled rounded rectangle; `rad` is the corner diameter (like GDI RoundRect).
pub unsafe fn round(_dc: HDC, r: RECT, rad: i32, c: COLORREF) {
    ctx(|x| {
        if let Some(b) = brush(&x.rt, c) {
            let w = (r.right - r.left) as f32 * 0.5;
            let h = (r.bottom - r.top) as f32 * 0.5;
            let rr = (rad as f32 * 0.5).min(w).min(h);
            x.rt.FillRoundedRectangle(
                &D2D1_ROUNDED_RECT {
                    rect: rf(&r),
                    radiusX: rr,
                    radiusY: rr,
                },
                &b,
            );
        }
    });
}

/// Rounded rectangle with a crisp 1 px border.
pub unsafe fn frame(_dc: HDC, r: RECT, rad: i32, fill_c: COLORREF, edge: COLORREF) {
    ctx(|x| {
        let rr = (rad as f32 * 0.5).min((r.bottom - r.top) as f32 * 0.5);
        let inner = D2D_RECT_F {
            left: r.left as f32 + 0.5,
            top: r.top as f32 + 0.5,
            right: r.right as f32 - 0.5,
            bottom: r.bottom as f32 - 0.5,
        };
        let shape = D2D1_ROUNDED_RECT {
            rect: inner,
            radiusX: rr,
            radiusY: rr,
        };
        if let Some(b) = brush(&x.rt, fill_c) {
            x.rt.FillRoundedRectangle(&shape, &b);
        }
        if let Some(b) = brush(&x.rt, edge) {
            x.rt.DrawRoundedRectangle(&shape, &b, 1.0, None);
        }
    });
}

pub unsafe fn ellipse(_dc: HDC, r: RECT, c: COLORREF) {
    ctx(|x| {
        if let Some(b) = brush(&x.rt, c) {
            let (rx, ry) = (
                (r.right - r.left) as f32 * 0.5,
                (r.bottom - r.top) as f32 * 0.5,
            );
            x.rt.FillEllipse(
                &D2D1_ELLIPSE {
                    point: D2D_POINT_2F {
                        x: r.left as f32 + rx,
                        y: r.top as f32 + ry,
                    },
                    radiusX: rx,
                    radiusY: ry,
                },
                &b,
            );
        }
    });
}

/// Text in a box. Understands the DT_* flags used here: alignment, vertical
/// centring, single line / word wrap, end and path ellipsis.
pub unsafe fn text(
    _dc: HDC,
    s: &str,
    r: &mut RECT,
    size: f32,
    weight: i32,
    color: COLORREF,
    flags: DRAW_TEXT_FORMAT,
) {
    if s.is_empty() {
        return;
    }
    ctx(|x| {
        let Ok(f) = x.dw.CreateTextFormat(
            w!("Segoe UI"),
            None,
            DWRITE_FONT_WEIGHT(weight),
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            size.max(1.0),
            w!("en-us"),
        ) else {
            return;
        };
        let _ = f.SetTextAlignment(if flags.0 & DT_CENTER.0 != 0 {
            DWRITE_TEXT_ALIGNMENT_CENTER
        } else if flags.0 & DT_RIGHT.0 != 0 {
            DWRITE_TEXT_ALIGNMENT_TRAILING
        } else {
            DWRITE_TEXT_ALIGNMENT_LEADING
        });
        let _ = f.SetParagraphAlignment(if flags.0 & DT_VCENTER.0 != 0 {
            DWRITE_PARAGRAPH_ALIGNMENT_CENTER
        } else {
            DWRITE_PARAGRAPH_ALIGNMENT_NEAR
        });
        let wrap = flags.0 & DT_WORDBREAK.0 != 0;
        let _ = f.SetWordWrapping(if wrap {
            DWRITE_WORD_WRAPPING_WRAP
        } else {
            DWRITE_WORD_WRAPPING_NO_WRAP
        });
        if flags.0 & (DT_END_ELLIPSIS.0 | DT_PATH_ELLIPSIS.0) != 0 {
            let path = flags.0 & DT_PATH_ELLIPSIS.0 != 0;
            let trim = DWRITE_TRIMMING {
                granularity: DWRITE_TRIMMING_GRANULARITY_CHARACTER,
                delimiter: if path { '\\' as u32 } else { 0 },
                delimiterCount: if path { 1 } else { 0 },
            };
            if let Ok(sign) = x.dw.CreateEllipsisTrimmingSign(&f) {
                let _ = f.SetTrimming(&trim, &sign);
            }
        }
        let wtext: Vec<u16> = s.encode_utf16().collect();
        let (w, h) = (
            (r.right - r.left).max(1) as f32,
            (r.bottom - r.top).max(1) as f32,
        );
        if let (Ok(l), Some(b)) = (x.dw.CreateTextLayout(&wtext, &f, w, h), brush(&x.rt, color)) {
            x.rt.DrawTextLayout(
                D2D_POINT_2F {
                    x: r.left as f32,
                    y: r.top as f32,
                },
                &l,
                &b,
                D2D1_DRAW_TEXT_OPTIONS_CLIP,
            );
        }
    });
}

/// Plus sign centred in `r`, `size` px across, lines snapped to whole pixels.
pub unsafe fn plus(_dc: HDC, r: RECT, size: f32, c: COLORREF) {
    let t = (size / 7.0).round().max(1.0);
    let half = (size / 2.0).round();
    // Odd line widths sit on pixel centres, even ones on pixel edges.
    let snap = |v: i32| v as f32 + if t as i32 % 2 == 1 { 0.5 } else { 0.0 };
    let (cx, cy) = (snap((r.left + r.right) / 2), snap((r.top + r.bottom) / 2));
    ctx(|x| {
        if let Some(b) = brush(&x.rt, c) {
            let p = |x, y| D2D_POINT_2F { x, y };
            x.rt.DrawLine(p(cx - half, cy), p(cx + half, cy), &b, t, None);
            x.rt.DrawLine(p(cx, cy - half), p(cx, cy + half), &b, t, None);
        }
    });
}

/// Cross (x) centred in `r`, `size` px across, with round ends.
pub unsafe fn cross(_dc: HDC, r: RECT, size: f32, width: f32, c: COLORREF) {
    let cx = (r.left + r.right) as f32 / 2.0;
    let cy = (r.top + r.bottom) as f32 / 2.0;
    let h = size / 2.0;
    ctx(|x| {
        let Some(b) = brush(&x.rt, c) else { return };
        let Ok(f) = x.rt.GetFactory() else { return };
        let props = D2D1_STROKE_STYLE_PROPERTIES {
            startCap: D2D1_CAP_STYLE_ROUND,
            endCap: D2D1_CAP_STYLE_ROUND,
            ..Default::default()
        };
        let style = f.CreateStrokeStyle(&props, None).ok();
        let p = |x, y| D2D_POINT_2F { x, y };
        x.rt.DrawLine(p(cx - h, cy - h), p(cx + h, cy + h), &b, width, style.as_ref());
        x.rt.DrawLine(p(cx - h, cy + h), p(cx + h, cy - h), &b, width, style.as_ref());
    });
}

/// Width of `s` in pixels when drawn by `text` with this size and weight.
pub fn text_width(s: &str, size: f32, weight: i32) -> f32 {
    let guess = s.chars().count() as f32 * size * 0.55;
    if s.is_empty() || init().is_err() {
        return if s.is_empty() { 0.0 } else { guess };
    }
    ctx(|x| unsafe {
        let f = x
            .dw
            .CreateTextFormat(
                w!("Segoe UI"),
                None,
                DWRITE_FONT_WEIGHT(weight),
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                size.max(1.0),
                w!("en-us"),
            )
            .ok()?;
        let wtext: Vec<u16> = s.encode_utf16().collect();
        let l = x.dw.CreateTextLayout(&wtext, &f, 100_000.0, 1000.0).ok()?;
        let mut m = DWRITE_TEXT_METRICS::default();
        l.GetMetrics(&mut m).ok()?;
        Some(m.widthIncludingTrailingWhitespace)
    })
    .flatten()
    .unwrap_or(guess)
}

/// Scales the Refract logo PNG to `size` with a high-quality filter.
fn logo_pixels(size: i32) -> Option<Vec<u8>> {
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(crate::icon::MARK_PNG).ok()?;
        let dec = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let scaler = wic.CreateBitmapScaler().ok()?;
        scaler
            .Initialize(
                &frame,
                size as u32,
                size as u32,
                WICBitmapInterpolationModeHighQualityCubic,
            )
            .ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &scaler,
            &GUID_WICPixelFormat32bppPBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let mut buf = vec![0u8; (size * size * 4) as usize];
        conv.CopyPixels(std::ptr::null(), (size * 4) as u32, &mut buf)
            .ok()?;
        Some(buf)
    }
}

/// The Refract logo at `size` px.
pub unsafe fn logo(_dc: HDC, x: i32, y: i32, size: i32) {
    if size <= 0 {
        return;
    }
    let px = match ctx(|c| c.logos.get(&size).cloned()).flatten() {
        Some(p) => p,
        None => {
            let Some(p) = logo_pixels(size) else { return };
            ctx(|c| c.logos.insert(size, p.clone()));
            p
        }
    };
    ctx(|c| {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        if let Ok(bmp) = c.rt.CreateBitmap(
            D2D_SIZE_U {
                width: size as u32,
                height: size as u32,
            },
            Some(px.as_ptr() as *const _),
            (size * 4) as u32,
            &props,
        ) {
            let dst = D2D_RECT_F {
                left: x as f32,
                top: y as f32,
                right: (x + size) as f32,
                bottom: (y + size) as f32,
            };
            c.rt.DrawBitmap(
                &bmp,
                Some(&dst),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
                None,
            );
        }
    });
}

/// Switch: pill track and round knob, both anti-aliased.
pub unsafe fn toggle(dc: HDC, r: RECT, on: bool, hover: bool) {
    let h = r.bottom - r.top;
    let track = match (on, hover) {
        (true, false) => MINT,
        (true, true) => MINT_HI,
        (false, false) => rgb(0x26, 0x31, 0x2C),
        (false, true) => rgb(0x31, 0x3E, 0x38),
    };
    round(dc, r, h, track);
    let pad = (h as f32 * 0.14).round().max(2.0) as i32;
    let d = h - pad * 2;
    let x = if on { r.right - pad - d } else { r.left + pad };
    ellipse(
        dc,
        rect(x, r.top + pad, x + d, r.top + pad + d),
        if on { ON_MINT } else { rgb(0xA4, 0xB2, 0xAB) },
    );
}

pub unsafe fn button(dc: HDC, r: RECT, label: &str, primary: bool, hover: bool, k: f32) {
    let rad = (20.0 * k) as i32;
    if primary {
        round(dc, r, rad, if hover { MINT_HI } else { MINT });
    } else {
        frame(
            dc,
            r,
            rad,
            if hover {
                rgb(0x1C, 0x27, 0x22)
            } else {
                rgb(0x14, 0x1C, 0x18)
            },
            if hover { rgb(0x3A, 0x5A, 0x4B) } else { EDGE },
        );
    }
    let mut t = r;
    text(
        dc,
        label,
        &mut t,
        13.5 * k,
        if primary { 600 } else { 500 },
        if primary { ON_MINT } else { INK },
        DT_SINGLELINE | DT_CENTER | DT_VCENTER,
    );
}

/// Dark title bar where Windows supports it.
pub unsafe fn dark_frame(hwnd: HWND) {
    use windows::Win32::Graphics::Dwm::*;
    let dark: BOOL = TRUE;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_USE_IMMERSIVE_DARK_MODE,
        &dark as *const _ as *const _,
        4,
    );
    let caption = BG;
    let _ = DwmSetWindowAttribute(
        hwnd,
        DWMWA_CAPTION_COLOR,
        &caption as *const _ as *const _,
        4,
    );
}

/// WM_PAINT: binds the Direct2D target to the window and calls `draw`.
pub unsafe fn paint(hwnd: HWND, draw: impl FnOnce(HDC, RECT)) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    if init().is_ok() {
        if let Some(rt) = ctx(|c| c.rt.clone()) {
            if rt.BindDC(hdc, &rc).is_ok() {
                rt.BeginDraw();
                draw(hdc, rc);
                if rt.EndDraw(None, None).is_err() {
                    // Device lost: start over on the next paint.
                    CTX.with(|c| *c.borrow_mut() = None);
                }
            }
        }
    }
    let _ = EndPaint(hwnd, &ps);
}

/// Draws into memory instead of a window: `w` x `h` BGRA pixels (README and
/// self-test pictures of the windows).
pub fn paint_offscreen(w: i32, h: i32, draw: impl FnOnce(HDC, RECT)) -> Option<Vec<u8>> {
    unsafe {
        init().ok()?;
        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        ReleaseDC(None, screen);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(mem, &bmi, DIB_RGB_COLORS, &mut bits, None, 0).ok()?;
        let old = SelectObject(mem, dib);
        let rc = rect(0, 0, w, h);
        let mut ok = false;
        if let Some(rt) = ctx(|c| c.rt.clone()) {
            if rt.BindDC(mem, &rc).is_ok() {
                rt.BeginDraw();
                draw(mem, rc);
                ok = rt.EndDraw(None, None).is_ok();
            }
        }
        let _ = GdiFlush();
        let mut out = vec![0u8; (w * h * 4) as usize];
        if ok && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, out.as_mut_ptr(), out.len());
        }
        // The target stays bound to the memory DC: drop it with the DC.
        CTX.with(|c| *c.borrow_mut() = None);
        SelectObject(mem, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem);
        for a in out.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        ok.then_some(out)
    }
}

pub fn set_cursor(hand: bool) {
    unsafe {
        if let Ok(c) = LoadCursorW(None, if hand { IDC_HAND } else { IDC_ARROW }) {
            SetCursor(c);
        }
    }
}

/// Direct access to the target during `paint` (custom shapes, clips).
pub unsafe fn with_rt<R>(f: impl FnOnce(&ID2D1RenderTarget) -> R) -> Option<R> {
    ctx(|c| f(&c.rt))
}

/// Solid brush on the current target, `a` = opacity.
pub unsafe fn brush_a(c: COLORREF, a: f32) -> Option<ID2D1SolidColorBrush> {
    ctx(|x| x.rt.CreateSolidColorBrush(&D2D1_COLOR_F { a, ..col(c) }, None).ok()).flatten()
}

/// Opaque BGRA pixels as a bitmap of the current target (valid while `generation()` stays).
pub fn bitmap(px: &[u8], w: u32, h: u32) -> Option<ID2D1Bitmap> {
    if init().is_err() || px.len() < (w * h * 4) as usize {
        return None;
    }
    ctx(|c| unsafe {
        let props = D2D1_BITMAP_PROPERTIES {
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_IGNORE,
            },
            dpiX: 96.0,
            dpiY: 96.0,
        };
        c.rt.CreateBitmap(
            D2D_SIZE_U {
                width: w,
                height: h,
            },
            Some(px.as_ptr() as *const _),
            w * 4,
            &props,
        )
        .ok()
    })
    .flatten()
}

/// Draws `bmp` stretched to `dst` with anti-aliased rounded corners.
pub unsafe fn image(bmp: &ID2D1Bitmap, dst: D2D_RECT_F, radius: f32) {
    ctx(|c| {
        let sz = bmp.GetSize();
        if sz.width <= 0.0 || sz.height <= 0.0 {
            return;
        }
        let props = D2D1_BITMAP_BRUSH_PROPERTIES {
            extendModeX: D2D1_EXTEND_MODE_CLAMP,
            extendModeY: D2D1_EXTEND_MODE_CLAMP,
            interpolationMode: D2D1_BITMAP_INTERPOLATION_MODE_LINEAR,
        };
        let Ok(b) = c.rt.CreateBitmapBrush(bmp, Some(&props), None) else {
            return;
        };
        let (sx, sy) = (
            (dst.right - dst.left) / sz.width,
            (dst.bottom - dst.top) / sz.height,
        );
        b.SetTransform(&windows::Foundation::Numerics::Matrix3x2 {
            M11: sx,
            M12: 0.0,
            M21: 0.0,
            M22: sy,
            M31: dst.left,
            M32: dst.top,
        });
        c.rt.FillRoundedRectangle(
            &D2D1_ROUNDED_RECT {
                rect: dst,
                radiusX: radius,
                radiusY: radius,
            },
            &b,
        );
    });
}
