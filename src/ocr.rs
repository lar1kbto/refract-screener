//! Text recognition (Windows.Media.Ocr, built into Windows, offline) and translation
//! (Google Cloud Translation API with a key, otherwise the public Google Translate
//! endpoint). Both run on worker threads and post results to the overlay window.

use crate::config::Rgb;
use crate::geom::*;
use crate::win::*;
use std::cell::Cell;
use windows::core::HSTRING;
use windows::Foundation::Uri;
use windows::Globalization::Language;
use windows::Graphics::Imaging::{BitmapPixelFormat, SoftwareBitmap};
use windows::Media::Ocr::OcrEngine;
use windows::Storage::Streams::{DataWriter, UnicodeEncoding};
use windows::Web::Http::{HttpClient, HttpStringContent};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};
use windows::Win32::UI::WindowsAndMessaging::PostMessageW;

#[derive(Clone, Debug)]
pub struct Word {
    pub rect: RectF,
    pub text: String,
    pub line: usize,
}

#[derive(Clone, Debug)]
pub struct Line {
    pub rect: RectF,
    pub first_word: usize,
    pub word_count: usize,
    pub text: String,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub rect: RectF,
    pub line_rects: Vec<RectF>,
    pub text: String,
    pub line_h: f32,
    pub bg: Rgb,
    pub fg: Rgb,
    pub translated: Option<String>,
    /// Font size that fits the translation into the block (0 = not measured yet).
    pub fitted: Cell<f32>,
}

#[derive(Default)]
pub struct OcrReply {
    pub gen: u64,
    pub words: Vec<Word>,
    pub lines: Vec<Line>,
    pub blocks: Vec<Block>,
    pub lang: String,
    pub error: Option<String>,
}

pub struct TranslateReply {
    pub gen: u64,
    pub target: String,
    pub result: Result<Vec<String>, String>,
}

/// Runs OCR on `bgra` (the selection, `sel` in image coordinates).
pub fn spawn_ocr(hwnd: isize, gen: u64, sel: RectI, bgra: Vec<u8>, lang: String) {
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let mut reply = match recognize(&bgra, sel, &lang) {
            Ok(r) => r,
            Err(e) => OcrReply {
                error: Some(e),
                ..Default::default()
            },
        };
        reply.gen = gen;
        let b = Box::new(reply);
        unsafe {
            let p = Box::into_raw(b);
            if PostMessageW(hwnd_from(hwnd), WM_APP_OCR, WPARAM(0), LPARAM(p as isize)).is_err() {
                drop(Box::from_raw(p));
            }
        }
    });
}

fn engine(lang: &str) -> Result<OcrEngine, String> {
    if !lang.trim().is_empty() {
        if let Ok(l) = Language::CreateLanguage(&HSTRING::from(lang.trim())) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&l) {
                return Ok(e);
            }
        }
    }
    OcrEngine::TryCreateFromUserProfileLanguages().map_err(|_| {
        "No OCR language installed. Add one in Settings > Time & language > Language & region."
            .to_string()
    })
}

/// Bilinear resize of BGRA.
fn resize(src: &[u8], w: usize, h: usize, nw: usize, nh: usize) -> Vec<u8> {
    let mut out = vec![0u8; nw * nh * 4];
    let sx = w as f32 / nw as f32;
    let sy = h as f32 / nh as f32;
    for y in 0..nh {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let y0 = fy as usize;
        let y1 = (y0 + 1).min(h - 1);
        let ty = fy - y0 as f32;
        for x in 0..nw {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
            let x0 = fx as usize;
            let x1 = (x0 + 1).min(w - 1);
            let tx = fx - x0 as f32;
            for c in 0..4 {
                let g = |xx: usize, yy: usize| src[(yy * w + xx) * 4 + c] as f32;
                let v = (g(x0, y0) * (1.0 - tx) + g(x1, y0) * tx) * (1.0 - ty)
                    + (g(x0, y1) * (1.0 - tx) + g(x1, y1) * tx) * ty;
                out[(y * nw + x) * 4 + c] = (v + 0.5) as u8;
            }
        }
    }
    out
}

pub fn recognize(bgra: &[u8], sel: RectI, lang: &str) -> Result<OcrReply, String> {
    let (w, h) = (sel.w() as usize, sel.h() as usize);
    if w < 4 || h < 4 {
        return Ok(OcrReply::default());
    }
    let max = OcrEngine::MaxImageDimension().unwrap_or(2600).max(1000) as f32;
    // Small UI text is read much better at 2x.
    let mut scale: f32 = if w.max(h) <= 1300 { 2.0 } else { 1.0 };
    if (w.max(h) as f32) * scale > max {
        scale = max / w.max(h) as f32;
    }
    let (nw, nh) = (
        ((w as f32 * scale) as usize).max(1),
        ((h as f32 * scale) as usize).max(1),
    );
    let pixels = if nw == w && nh == h {
        bgra.to_vec()
    } else {
        resize(bgra, w, h, nw, nh)
    };

    let e = |x: windows::core::Error| x.message().to_string();
    let writer = DataWriter::new().map_err(e)?;
    writer.WriteBytes(&pixels).map_err(e)?;
    let buffer = writer.DetachBuffer().map_err(e)?;
    let bitmap = SoftwareBitmap::CreateCopyFromBuffer(
        &buffer,
        BitmapPixelFormat::Bgra8,
        nw as i32,
        nh as i32,
    )
    .map_err(e)?;
    let engine = engine(lang)?;
    let lang_tag = engine
        .RecognizerLanguage()
        .and_then(|l| l.LanguageTag())
        .map(|t| t.to_string())
        .unwrap_or_default();
    let result = engine
        .RecognizeAsync(&bitmap)
        .map_err(e)?
        .get()
        .map_err(e)?;

    let mut out = OcrReply {
        lang: lang_tag,
        ..Default::default()
    };
    let lines = result.Lines().map_err(e)?;
    for li in 0..lines.Size().map_err(e)? {
        let line = lines.GetAt(li).map_err(e)?;
        let words = line.Words().map_err(e)?;
        let first = out.words.len();
        let mut lr: Option<RectF> = None;
        for wi in 0..words.Size().map_err(e)? {
            let word = words.GetAt(wi).map_err(e)?;
            let r = word.BoundingRect().map_err(e)?;
            let rect = RectF::xywh(
                sel.l as f32 + r.X / scale,
                sel.t as f32 + r.Y / scale,
                r.Width / scale,
                r.Height / scale,
            );
            lr = Some(lr.map_or(rect, |u| u.union(&rect)));
            out.words.push(Word {
                rect,
                text: word.Text().map(|t| t.to_string()).unwrap_or_default(),
                line: out.lines.len(),
            });
        }
        if let Some(rect) = lr {
            out.lines.push(Line {
                rect,
                first_word: first,
                word_count: out.words.len() - first,
                text: line.Text().map(|t| t.to_string()).unwrap_or_default(),
            });
        }
    }
    out.blocks = blocks(&out.lines, bgra, sel);
    Ok(out)
}

/// Groups lines into paragraphs: translating whole sentences reads far better
/// than translating line by line.
fn blocks(lines: &[Line], bgra: &[u8], sel: RectI) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut prev: Option<&Line> = None;
    for l in lines {
        let h = l.rect.h().max(1.0);
        let join = prev.is_some_and(|p| {
            let ph = p.rect.h().max(1.0);
            let ratio = h / ph;
            let gap = l.rect.t - p.rect.b;
            let overlap = l.rect.l < p.rect.r && p.rect.l < l.rect.r;
            (0.72..1.38).contains(&ratio)
                && gap > -0.4 * h
                && gap < 0.95 * h.max(ph)
                && overlap
                && (l.rect.l - p.rect.l).abs() < 6.0 * h
        });
        if join {
            let b = out.last_mut().unwrap();
            b.rect = b.rect.union(&l.rect);
            b.line_rects.push(l.rect);
            if b.text.ends_with('-') && !b.text.ends_with(" -") {
                b.text.pop();
            } else {
                b.text.push(' ');
            }
            b.text.push_str(&l.text);
            let n = b.line_rects.len() as f32;
            b.line_h = (b.line_h * (n - 1.0) + h) / n;
        } else {
            out.push(Block {
                rect: l.rect,
                line_rects: vec![l.rect],
                text: l.text.clone(),
                line_h: h,
                bg: Rgb(0xFFFFFF),
                fg: Rgb(0x000000),
                translated: None,
                fitted: Cell::new(0.0),
            });
        }
        prev = Some(l);
    }
    for b in &mut out {
        let (bg, fg) = colors(b, bgra, sel);
        b.bg = bg;
        b.fg = fg;
    }
    out
}

/// Background = most common colour around the lines; text = the pixels inside the
/// lines that differ most from it.
fn colors(b: &Block, bgra: &[u8], sel: RectI) -> (Rgb, Rgb) {
    let (w, h) = (sel.w(), sel.h());
    let px = |x: i32, y: i32| -> Option<(u8, u8, u8)> {
        let (x, y) = (x - sel.l, y - sel.t);
        if x < 0 || y < 0 || x >= w || y >= h {
            return None;
        }
        let i = ((y * w + x) * 4) as usize;
        Some((bgra[i + 2], bgra[i + 1], bgra[i]))
    };
    let mut hist: std::collections::HashMap<u32, (u32, [u32; 3])> =
        std::collections::HashMap::new();
    let mut add = |c: (u8, u8, u8)| {
        let key = ((c.0 as u32 >> 4) << 8) | ((c.1 as u32 >> 4) << 4) | (c.2 as u32 >> 4);
        let e = hist.entry(key).or_insert((0, [0; 3]));
        e.0 += 1;
        e.1[0] += c.0 as u32;
        e.1[1] += c.1 as u32;
        e.1[2] += c.2 as u32;
    };
    for r in &b.line_rects {
        let d = (r.h() * 0.18).max(2.0) as i32;
        let (l, t, rr, bb) = (
            r.l as i32 - d,
            r.t as i32 - d,
            r.r as i32 + d,
            r.b as i32 + d,
        );
        let step = ((rr - l) / 60).max(1);
        for x in (l..rr).step_by(step as usize) {
            px(x, t).map(&mut add);
            px(x, bb).map(&mut add);
        }
        let step = ((bb - t) / 20).max(1);
        for y in (t..bb).step_by(step as usize) {
            px(l, y).map(&mut add);
            px(rr, y).map(&mut add);
        }
    }
    let bg = hist
        .values()
        .max_by_key(|v| v.0)
        .map(|v| {
            Rgb::from_f(
                v.1[0] as f32 / v.0 as f32 / 255.0,
                v.1[1] as f32 / v.0 as f32 / 255.0,
                v.1[2] as f32 / v.0 as f32 / 255.0,
            )
        })
        .unwrap_or(Rgb(0xFFFFFF));
    let dist = |c: (u8, u8, u8)| {
        let (r, g, bl) = (
            c.0 as f32 / 255.0 - bg.r(),
            c.1 as f32 / 255.0 - bg.g(),
            c.2 as f32 / 255.0 - bg.b(),
        );
        r * r + g * g + bl * bl
    };
    let mut inside: Vec<(f32, (u8, u8, u8))> = Vec::new();
    for r in &b.line_rects {
        let sx = ((r.w() / 80.0) as i32).max(1);
        let sy = ((r.h() / 12.0) as i32).max(1);
        for y in (r.t as i32..r.b as i32).step_by(sy as usize) {
            for x in (r.l as i32..r.r as i32).step_by(sx as usize) {
                if let Some(c) = px(x, y) {
                    inside.push((dist(c), c));
                }
            }
        }
    }
    inside.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    let top = &inside[..(inside.len() / 8).max(1).min(inside.len())];
    let fg = if top.is_empty() {
        crate::paint::contrast(bg)
    } else {
        let n = top.len() as f32;
        let s = top.iter().fold((0.0, 0.0, 0.0), |a, (_, c)| {
            (a.0 + c.0 as f32, a.1 + c.1 as f32, a.2 + c.2 as f32)
        });
        Rgb::from_f(s.0 / n / 255.0, s.1 / n / 255.0, s.2 / n / 255.0)
    };
    let fg = if (fg.luma() - bg.luma()).abs() < 0.3 {
        crate::paint::contrast(bg)
    } else {
        fg
    };
    (bg, fg)
}

// ---------------- translation ----------------

/// Blocking translation (used by the self test).
pub fn translate_now(texts: &[String], target: &str, key: &str) -> Result<Vec<String>, String> {
    if key.trim().is_empty() {
        translate_web(texts, target)
    } else {
        translate_cloud(texts, target, key.trim())
    }
}

pub fn spawn_translate(hwnd: isize, gen: u64, texts: Vec<String>, target: String, key: String) {
    std::thread::spawn(move || {
        unsafe {
            let _ = RoInitialize(RO_INIT_MULTITHREADED);
        }
        let result = if key.trim().is_empty() {
            translate_web(&texts, &target)
        } else {
            translate_cloud(&texts, &target, key.trim())
        };
        let b = Box::new(TranslateReply {
            gen,
            target,
            result,
        });
        unsafe {
            let p = Box::into_raw(b);
            if PostMessageW(
                hwnd_from(hwnd),
                WM_APP_TRANSLATED,
                WPARAM(0),
                LPARAM(p as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(p));
            }
        }
    });
}

fn post(url: &str, body: &str, media: &str) -> Result<String, String> {
    let e = |x: windows::core::Error| format!("Network error: {}", x.message());
    let client = HttpClient::new().map_err(e)?;
    let uri = Uri::CreateUri(&HSTRING::from(url)).map_err(e)?;
    let content = HttpStringContent::CreateFromStringWithEncodingAndMediaType(
        &HSTRING::from(body),
        UnicodeEncoding::Utf8,
        &HSTRING::from(media),
    )
    .map_err(e)?;
    let resp = client
        .PostAsync(&uri, &content)
        .map_err(e)?
        .get()
        .map_err(e)?;
    let status = resp.StatusCode().map_err(e)?.0;
    let text = resp
        .Content()
        .map_err(e)?
        .ReadAsStringAsync()
        .map_err(e)?
        .get()
        .map_err(e)?
        .to_string();
    if !(200..300).contains(&status) {
        let msg = serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| v["error"]["message"].as_str().map(String::from))
            .unwrap_or_else(|| text.chars().take(160).collect());
        return Err(format!("Google Translate HTTP {status}: {msg}"));
    }
    Ok(text)
}

fn urlenc(s: &str) -> String {
    let mut o = String::with_capacity(s.len() * 3);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            _ => o.push_str(&format!("%{b:02X}")),
        }
    }
    o
}

/// Official Cloud Translation API v2 (needs an API key with the API enabled).
fn translate_cloud(texts: &[String], target: &str, key: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(100) {
        let body =
            serde_json::json!({ "q": chunk, "target": target, "format": "text" }).to_string();
        let url = format!(
            "https://translation.googleapis.com/language/translate/v2?key={}",
            urlenc(key)
        );
        let resp = post(&url, &body, "application/json")?;
        let v: serde_json::Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
        let arr = v["data"]["translations"]
            .as_array()
            .ok_or("Unexpected response from Google")?;
        for t in arr {
            out.push(t["translatedText"].as_str().unwrap_or("").to_string());
        }
    }
    if out.len() != texts.len() {
        return Err("Google returned a different number of lines".into());
    }
    Ok(out)
}

fn web_one(text: &str, target: &str) -> Result<String, String> {
    let url = format!(
        "https://translate.googleapis.com/translate_a/single?client=gtx&sl=auto&tl={}&dt=t",
        urlenc(target)
    );
    let resp = post(
        &url,
        &format!("q={}", urlenc(text)),
        "application/x-www-form-urlencoded",
    )?;
    let v: serde_json::Value = serde_json::from_str(&resp).map_err(|e| e.to_string())?;
    let mut s = String::new();
    if let Some(segs) = v[0].as_array() {
        for seg in segs {
            if let Some(t) = seg[0].as_str() {
                s.push_str(t);
            }
        }
    }
    Ok(s)
}

/// Public Google Translate endpoint (no key). Paragraphs are sent together,
/// separated by new lines, in as few requests as possible.
fn translate_web(texts: &[String], target: &str) -> Result<Vec<String>, String> {
    let mut out = Vec::with_capacity(texts.len());
    let mut i = 0;
    while i < texts.len() {
        let mut j = i;
        let mut len = 0;
        while j < texts.len() && (j == i || len + texts[j].len() < 3500) {
            len += texts[j].len() + 1;
            j += 1;
        }
        let group = &texts[i..j];
        let joined = group
            .iter()
            .map(|t| t.replace('\n', " "))
            .collect::<Vec<_>>()
            .join("\n");
        let tr = web_one(&joined, target)?;
        let parts: Vec<String> = tr.split('\n').map(|s| s.trim().to_string()).collect();
        if parts.len() == group.len() {
            out.extend(parts);
        } else {
            for t in group {
                out.push(web_one(t, target)?.trim().to_string());
            }
        }
        i = j;
    }
    Ok(out)
}
