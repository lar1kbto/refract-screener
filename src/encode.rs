//! Lossless encoders and the clipboard.
//!
//! Pixels are never changed: PNG goes through oxipng (exact palette when the shot has
//! <= 256 colours, bit-depth/colour-type reduction, filter search, libdeflate level
//! 12), WebP uses libwebp 1.6 in lossless mode with `exact` so even hidden pixels stay.

use crate::config::ImageFormat;
use crate::win::*;
use std::path::Path;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::{CF_DIB, CF_UNICODETEXT};

pub struct Image {
    pub w: u32,
    pub h: u32,
    /// BGRA, top-down, opaque.
    pub bgra: Vec<u8>,
}

fn rgb(img: &Image) -> Vec<u8> {
    let mut out = Vec::with_capacity((img.w * img.h * 3) as usize);
    for px in img.bgra.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out
}

/// `effort` 0..6 maps to oxipng presets.
pub fn png(img: &Image, effort: u8) -> Result<Vec<u8>, String> {
    let mut raw = oxipng::RawImage::new(
        img.w,
        img.h,
        oxipng::ColorType::RGB {
            transparent_color: None,
        },
        oxipng::BitDepth::Eight,
        rgb(img),
    )
    .map_err(|e| e.to_string())?;
    // sRGB (perceptual): tells viewers the pixels are plain sRGB, 13 bytes.
    raw.add_png_chunk(*b"sRGB", vec![0]);
    let mut opts = oxipng::Options::from_preset(effort.min(6));
    opts.strip = oxipng::StripChunks::Safe;
    opts.optimize_alpha = false;
    if effort >= 3 {
        opts.deflater = oxipng::Deflater::Libdeflater { compression: 12 };
    }
    raw.create_optimized_png(&opts).map_err(|e| e.to_string())
}

/// Lossless WebP via libwebp 1.6. Method 5 is within ~1 % of method 6 on screenshots
/// but 2-6x faster (a 4K game frame: 6 s instead of 24 s), `exact` keeps every pixel.
pub fn webp(img: &Image) -> Result<Vec<u8>, String> {
    use libwebp_sys::*;
    let rgb = rgb(img);
    let mut cfg = WebPConfig::new().map_err(|_| "WebPConfig init failed".to_string())?;
    cfg.lossless = 1;
    cfg.quality = 100.0; // in lossless mode: compression effort
    cfg.method = 5;
    cfg.exact = 1;
    cfg.thread_level = 1;
    let mut pic = WebPPicture::new().map_err(|_| "WebPPicture init failed".to_string())?;
    pic.use_argb = 1;
    pic.width = img.w as i32;
    pic.height = img.h as i32;
    unsafe {
        let mut wr: WebPMemoryWriter = std::mem::zeroed();
        WebPMemoryWriterInit(&mut wr);
        let ok = WebPPictureImportRGB(&mut pic, rgb.as_ptr(), (img.w * 3) as i32) != 0 && {
            pic.writer = Some(WebPMemoryWrite);
            pic.custom_ptr = &mut wr as *mut WebPMemoryWriter as *mut _;
            WebPEncode(&cfg, &mut pic) != 0
        };
        let err = pic.error_code;
        let out = if ok && !wr.mem.is_null() {
            Ok(std::slice::from_raw_parts(wr.mem, wr.size).to_vec())
        } else {
            Err(format!("WebP encode failed: {err:?}"))
        };
        WebPPictureFree(&mut pic);
        WebPMemoryWriterClear(&mut wr);
        out
    }
}

/// Decodes a WebP to RGB (used by the self test).
pub fn webp_decode_rgb(data: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    use libwebp_sys::*;
    unsafe {
        let (mut w, mut h) = (0, 0);
        let p = WebPDecodeRGB(data.as_ptr(), data.len(), &mut w, &mut h);
        if p.is_null() {
            return None;
        }
        let px = std::slice::from_raw_parts(p, (w * h * 3) as usize).to_vec();
        WebPFree(p as *mut _);
        Some((w as u32, h as u32, px))
    }
}

pub fn encode(img: &Image, fmt: ImageFormat, effort: u8) -> Result<Vec<u8>, String> {
    match fmt {
        ImageFormat::Png => png(img, effort),
        ImageFormat::WebP => webp(img),
    }
}

pub fn format_for(path: &Path) -> ImageFormat {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("webp") => ImageFormat::WebP,
        _ => ImageFormat::Png,
    }
}

pub struct SaveError {
    pub msg: String,
    /// Windows refused the write (Controlled folder access returns "not found" /
    /// "access denied" for blocked apps).
    pub blocked: bool,
}

pub fn save(img: &Image, path: &Path, effort: u8) -> Result<u64, SaveError> {
    let data = encode(img, format_for(path), effort).map_err(|msg| SaveError {
        msg,
        blocked: false,
    })?;
    write(path, &data)?;
    Ok(data.len() as u64)
}

pub fn write(path: &Path, data: &[u8]) -> Result<(), SaveError> {
    let io = |e: std::io::Error| SaveError {
        blocked: matches!(e.raw_os_error(), Some(2) | Some(5)),
        msg: e.to_string(),
    };
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(io)?;
    }
    std::fs::write(path, data).map_err(io)
}

// ---------------- clipboard ----------------

unsafe fn global(bytes: &[u8]) -> Option<HGLOBAL> {
    let h = GlobalAlloc(GMEM_MOVEABLE, bytes.len()).ok()?;
    let p = GlobalLock(h);
    if p.is_null() {
        let _ = GlobalFree(h);
        return None;
    }
    std::ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
    let _ = GlobalUnlock(h);
    Some(h)
}

/// 24-bit bottom-up DIB: the format every Windows app can paste.
fn dib(img: &Image) -> Vec<u8> {
    let stride = ((img.w * 3 + 3) & !3) as usize;
    let hdr = BITMAPINFOHEADER {
        biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
        biWidth: img.w as i32,
        biHeight: img.h as i32,
        biPlanes: 1,
        biBitCount: 24,
        biCompression: BI_RGB.0,
        biSizeImage: (stride * img.h as usize) as u32,
        biXPelsPerMeter: 3780,
        biYPelsPerMeter: 3780,
        ..Default::default()
    };
    let hs = std::mem::size_of::<BITMAPINFOHEADER>();
    let mut out = vec![0u8; hs + stride * img.h as usize];
    unsafe {
        std::ptr::copy_nonoverlapping(&hdr as *const _ as *const u8, out.as_mut_ptr(), hs);
    }
    let w = img.w as usize;
    for y in 0..img.h as usize {
        let src = &img.bgra[y * w * 4..(y + 1) * w * 4];
        let row = hs + (img.h as usize - 1 - y) * stride;
        for x in 0..w {
            out[row + x * 3..row + x * 3 + 3].copy_from_slice(&src[x * 4..x * 4 + 3]);
        }
    }
    out
}

fn open_clipboard(owner: HWND) -> bool {
    for _ in 0..10 {
        if unsafe { OpenClipboard(owner) }.is_ok() {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(30));
    }
    false
}

/// Puts the image on the clipboard as DIB (universal) and PNG (lossless, used by
/// browsers, Discord, Telegram, Office...). `png` is an already encoded copy.
pub fn copy_image(owner: HWND, img: &Image, png: Option<&[u8]>) -> bool {
    if !open_clipboard(owner) {
        return false;
    }
    let mut ok = false;
    unsafe {
        let _ = EmptyClipboard();
        if let Some(h) = global(&dib(img)) {
            ok = SetClipboardData(CF_DIB.0 as u32, HANDLE(h.0)).is_ok();
        }
        if let Some(data) = png {
            let fmt = RegisterClipboardFormatW(W::new("PNG").p());
            if let Some(h) = global(data) {
                let _ = SetClipboardData(fmt, HANDLE(h.0));
            }
        }
        let _ = CloseClipboard();
    }
    ok
}

pub fn copy_text(owner: HWND, text: &str) -> bool {
    if !open_clipboard(owner) {
        return false;
    }
    let mut ok = false;
    unsafe {
        let _ = EmptyClipboard();
        let w = wide(&text.replace("\r\n", "\n").replace('\n', "\r\n"));
        let bytes = std::slice::from_raw_parts(w.as_ptr() as *const u8, w.len() * 2);
        if let Some(h) = global(bytes) {
            ok = SetClipboardData(CF_UNICODETEXT.0 as u32, HANDLE(h.0)).is_ok();
        }
        let _ = CloseClipboard();
    }
    ok
}

pub fn paste_text(owner: HWND) -> Option<String> {
    if !open_clipboard(owner) {
        return None;
    }
    let mut out = None;
    unsafe {
        if let Ok(h) = GetClipboardData(CF_UNICODETEXT.0 as u32) {
            let g = HGLOBAL(h.0);
            let p = GlobalLock(g) as *const u16;
            if !p.is_null() {
                let mut n = 0;
                while *p.add(n) != 0 {
                    n += 1;
                }
                out = Some(String::from_utf16_lossy(std::slice::from_raw_parts(p, n)));
                let _ = GlobalUnlock(g);
            }
        }
        let _ = CloseClipboard();
    }
    out
}
