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
use windows::Win32::Foundation::{LPARAM, S_OK, WPARAM};
use windows::Win32::Globalization::{ISpellChecker, ISpellCheckerFactory, SpellCheckerFactory};
use windows::Win32::System::Com::{CoCreateInstance, CLSCTX_INPROC_SERVER};
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
    /// Visual row: lines side by side (table cells, split code lines) share one.
    pub row: usize,
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
    /// Advice to show once, e.g. to add a missing OCR language pack.
    pub hint: Option<String>,
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

/// OCR engines to run. An explicit language gives one engine. Otherwise the profile
/// engine comes first, followed by every other installed OCR language with a different
/// script: the English engine reads Cyrillic as Latin junk and silently drops short
/// words ("и", "в", "для"), so Russian text needs the Russian engine, while the Russian
/// engine turns English words into Cyrillic look-alikes ("оп", "сору").
fn engines(lang: &str) -> Result<Vec<OcrEngine>, String> {
    if !lang.trim().is_empty() {
        if let Ok(l) = Language::CreateLanguage(&HSTRING::from(lang.trim())) {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&l) {
                return Ok(vec![e]);
            }
        }
    }
    let mut out: Vec<OcrEngine> = Vec::new();
    let mut scripts: Vec<u8> = Vec::new();
    let mut add = |e: OcrEngine, out: &mut Vec<OcrEngine>| {
        let s = script_of(&engine_tag(&e));
        if !scripts.contains(&s) && out.len() < 3 {
            scripts.push(s);
            out.push(e);
        }
    };
    if let Ok(e) = OcrEngine::TryCreateFromUserProfileLanguages() {
        add(e, &mut out);
    }
    if let Ok(langs) = OcrEngine::AvailableRecognizerLanguages() {
        for l in langs {
            if let Ok(e) = OcrEngine::TryCreateFromLanguage(&l) {
                add(e, &mut out);
            }
        }
    }
    if out.is_empty() {
        return Err(
            "No OCR language installed. Add one in Settings > Time & language > Language & region."
                .to_string(),
        );
    }
    Ok(out)
}

fn engine_tag(e: &OcrEngine) -> String {
    e.RecognizerLanguage()
        .and_then(|l| l.LanguageTag())
        .map(|t| t.to_string())
        .unwrap_or_default()
}

/// Rough script family of a language tag: 0 Latin, 1 Cyrillic, 2 Greek, 3 other.
fn script_of(tag: &str) -> u8 {
    let t = tag.to_ascii_lowercase();
    let primary = t.split('-').next().unwrap_or("");
    if t.contains("-latn") {
        0
    } else if t.contains("-cyrl")
        || ["ru", "uk", "be", "bg", "mk", "kk", "ky", "mn", "tg", "sr"].contains(&primary)
    {
        1
    } else if primary == "el" {
        2
    } else if ["zh", "ja", "ko", "ar", "he", "fa", "hi", "th", "ur"].contains(&primary) {
        3
    } else {
        0
    }
}

/// A profile language without an OCR pack, as a display name ("Russian").
fn missing_pack() -> Option<String> {
    let langs = windows::System::UserProfile::GlobalizationPreferences::Languages().ok()?;
    let installed: Vec<String> = OcrEngine::AvailableRecognizerLanguages()
        .map(|v| {
            v.into_iter()
                .filter_map(|a| a.LanguageTag().ok())
                .map(|t| primary_tag(&t.to_string()))
                .collect()
        })
        .unwrap_or_default();
    for t in langs {
        // Region variants share a pack: en-GB is fine when en-US is installed.
        if installed.contains(&primary_tag(&t.to_string())) {
            continue;
        }
        if let Ok(l) = Language::CreateLanguage(&t) {
            return l.DisplayName().ok().map(|n| n.to_string());
        }
    }
    None
}

fn primary_tag(t: &str) -> String {
    t.split('-').next().unwrap_or("").to_ascii_lowercase()
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

/// One engine's line: (box, text) words in the engine's order.
type RawLine = Vec<(RectF, String)>;

fn run_engine(
    engine: &OcrEngine,
    bitmap: &SoftwareBitmap,
    sel: RectI,
    scale: f32,
) -> Result<Vec<RawLine>, String> {
    let e = |x: windows::core::Error| x.message().to_string();
    let result = engine.RecognizeAsync(bitmap).map_err(e)?.get().map_err(e)?;
    let mut out = Vec::new();
    for line in result.Lines().map_err(e)? {
        let mut words = Vec::new();
        for word in line.Words().map_err(e)? {
            let r = word.BoundingRect().map_err(e)?;
            let rect = RectF::xywh(
                sel.l as f32 + r.X / scale,
                sel.t as f32 + r.Y / scale,
                r.Width / scale,
                r.Height / scale,
            );
            let text = word.Text().map(|t| t.to_string()).unwrap_or_default();
            if !text.trim().is_empty() {
                words.push((rect, text));
            }
        }
        if !words.is_empty() {
            out.push(words);
        }
    }
    Ok(out)
}

fn bitmap(bgra: &[u8], w: usize, h: usize, scale: f32) -> Result<SoftwareBitmap, String> {
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
    SoftwareBitmap::CreateCopyFromBuffer(&buffer, BitmapPixelFormat::Bgra8, nw as i32, nh as i32)
        .map_err(e)
}

pub fn recognize(bgra: &[u8], sel: RectI, lang: &str) -> Result<OcrReply, String> {
    let (w, h) = (sel.w() as usize, sel.h() as usize);
    if w < 4 || h < 4 {
        return Ok(OcrReply::default());
    }
    let max = OcrEngine::MaxImageDimension().unwrap_or(2600).max(1000) as f32;
    let side = w.max(h) as f32;
    // Small UI text is read much better enlarged. Each scale misses different words
    // (short tokens, long code expressions), so small pictures are read at 2x and 3x
    // and the results combined; one pass takes 10-40 ms.
    let scales: Vec<f32> = if side <= 1300.0 {
        vec![2.0f32.min(max / side), 3.0f32.min(max / side)]
    } else {
        vec![1.0f32.min(max / side)]
    };
    let engines = engines(lang)?;
    let tags: Vec<String> = engines.iter().map(engine_tag).collect();
    let mut runs = Vec::new();
    let mut run_tags = Vec::new();
    for (si, &scale) in scales.iter().enumerate() {
        if si > 0 && (scale - scales[si - 1]).abs() < 0.25 {
            continue;
        }
        let bmp = bitmap(bgra, w, h, scale)?;
        for (i, engine) in engines.iter().enumerate() {
            match run_engine(engine, &bmp, sel, scale) {
                Ok(r) => {
                    runs.push(r);
                    run_tags.push(tags[i].clone());
                }
                // The first pass must work; the others are best effort.
                Err(err) if runs.is_empty() => return Err(err),
                Err(_) => {}
            }
        }
    }
    let mut out = OcrReply {
        lang: tags.join("+"),
        ..Default::default()
    };
    layout(merge(&runs, &run_tags), &mut out);
    out.blocks = blocks(&out.lines, bgra, sel);
    if engines.len() == 1 && !out.words.is_empty() {
        let junk = out.words.iter().filter(|w| !clean(&w.text)).count();
        if junk * 4 >= out.words.len() {
            if let Some(name) = missing_pack() {
                out.hint = Some(format!(
                    "Words missing? Add the {name} OCR pack: Settings > Time & language > Language & region > {name} > Language options"
                ));
            }
        }
    }
    Ok(out)
}

// ---------------- combining engines ----------------

fn latin(c: char) -> bool {
    c.is_ascii_alphabetic() || ('\u{C0}'..='\u{24F}').contains(&c) && c != '\u{D7}' && c != '\u{F7}'
}

/// Cyrillic and Greek letters that look like Latin ones (or digits): an engine that
/// reads English through Cyrillic glasses produces words made only of these.
fn lookalike(c: char) -> bool {
    "аАвВеЕкКмМнНоОпрРсСтТуУхХЗзЬьгіІјЈѕЅԁԛԝΑΒΕΗΙΚΜΝΟΡΤΥΧαβεικνορτυχ".contains(c)
}

/// A token with digits is a number, model or code ("8945HX", "0x80070002"): Cyrillic
/// look-alikes in it are Latin letters (or 3) misread by the Cyrillic engine.
fn latinize(t: &str) -> String {
    if !t.chars().any(|c| c.is_ascii_digit()) || real_foreign(t) || !any_foreign(t) {
        return t.to_string();
    }
    const FROM: &str = "АВЕКМНОРСТУХЗзавекмнопрстухЬьгіІјѕ";
    const TO: &str = "ABEKMHOPCTYX33aBekMHonpcTyxbbriIjs";
    t.chars()
        .map(|c| {
            FROM.chars()
                .position(|f| f == c)
                .map_or(c, |i| TO.chars().nth(i).unwrap_or(c))
        })
        .collect()
}

/// A non-Latin letter that is not a look-alike: real Cyrillic, Greek, CJK...
fn real_foreign(s: &str) -> bool {
    s.chars()
        .any(|c| c.is_alphabetic() && !latin(c) && !lookalike(c))
}

fn any_foreign(s: &str) -> bool {
    s.chars().any(|c| c.is_alphabetic() && !latin(c))
}

/// Plausible word shape: no digits inside letters ("pa3Aene"), no case jumps inside
/// a word ("OTKPOiTe", "CtrI"), apart from camelCase humps and numbers at the ends.
pub fn clean(s: &str) -> bool {
    s.split(|c: char| !c.is_alphanumeric())
        .filter(|p| !p.is_empty())
        .all(clean_part)
}

fn clean_part(p: &str) -> bool {
    let cs: Vec<char> = p.chars().collect();
    let digits = cs.iter().filter(|c| c.is_numeric()).count();
    if digits == cs.len() {
        return true;
    }
    if digits > 0 {
        let lower = p.to_lowercase();
        if lower.starts_with("0x") && cs[2..].iter().all(|c| c.is_ascii_hexdigit()) {
            return true;
        }
        // Win32, x64, utf8 / 1st, 2nd, 64k.
        let lead = cs.iter().take_while(|c| c.is_numeric()).count();
        let trail = cs.iter().rev().take_while(|c| c.is_numeric()).count();
        if lead + trail == digits {
            if lead == 0 {
                let letters: String = cs[..cs.len() - trail].iter().collect();
                return clean_part(&letters);
            }
            if trail == 0 {
                return cs.len() - lead <= 2;
            }
        }
        return false;
    }
    let up = |c: &char| c.is_uppercase();
    if cs.iter().all(up) || !cs.iter().any(up) {
        return true;
    }
    // camelCase / PascalCase: a capital starts a hump of >= 2 lower letters; runs of
    // capitals are fine (XMLHttp, macOS), a lone capital inside a word is not (CtrI).
    let mut i = 0;
    while i < cs.len() {
        if up(&cs[i]) {
            let run = cs[i..].iter().take_while(|c| up(c)).count();
            let lows = cs[i + run..].iter().take_while(|c| !up(c)).count();
            if lows == 0 {
                if run < 2 && i > 0 {
                    return false;
                }
            } else if lows < 2 && !(i == 0 && run == 1) {
                return false;
            }
            i += run + lows;
        } else {
            i += 1;
        }
    }
    true
}

/// Windows spell checkers, to tell a real word from a misreading of another script.
struct Speller {
    factory: Option<ISpellCheckerFactory>,
    cache: Vec<(String, Option<ISpellChecker>)>,
}

// The spell checking objects are free-threaded; OCR threads are all in the MTA.
unsafe impl Send for Speller {}

impl Speller {
    fn new() -> Self {
        Speller {
            factory: unsafe { CoCreateInstance(&SpellCheckerFactory, None, CLSCTX_INPROC_SERVER) }
                .ok(),
            cache: Vec::new(),
        }
    }

    /// Whether every word of `text` is in the `tag` dictionary; None without one.
    fn known(&mut self, tag: &str, text: &str) -> Option<bool> {
        let letters: String = text
            .chars()
            .map(|c| {
                if c.is_alphabetic() || c == '\'' {
                    c
                } else {
                    ' '
                }
            })
            .collect();
        if letters.trim().is_empty() {
            return None;
        }
        if !self.cache.iter().any(|c| c.0 == tag) {
            let f = self.factory.as_ref()?;
            let t = HSTRING::from(tag);
            let c = unsafe {
                f.IsSupported(&t)
                    .is_ok_and(|b| b.as_bool())
                    .then(|| f.CreateSpellChecker(&t).ok())
                    .flatten()
            };
            self.cache.push((tag.to_string(), c));
        }
        let c = self.cache.iter().find(|c| c.0 == tag)?.1.as_ref()?;
        let errors = unsafe { c.Check(&HSTRING::from(letters.trim())) }.ok()?;
        let mut e = None;
        let hr = unsafe { errors.Next(&mut e) };
        Some(!(hr == S_OK && e.is_some()))
    }
}

struct Cand {
    run: usize,
    line: usize,
    rect: RectF,
    text: String,
}

/// Intersection over the smaller box.
fn overlap(a: &RectF, b: &RectF) -> f32 {
    let w = (a.r.min(b.r) - a.l.max(b.l)).max(0.0);
    let h = (a.b.min(b.b) - a.t.max(b.t)).max(0.0);
    let small = (a.w() * a.h()).min(b.w() * b.h()).max(1.0);
    w * h / small
}

fn find(p: &mut [usize], mut i: usize) -> usize {
    while p[i] != i {
        p[i] = p[p[i]];
        i = p[i];
    }
    i
}

fn union(p: &mut [usize], a: usize, b: usize) {
    let (a, b) = (find(p, a), find(p, b));
    p[a] = b;
}

struct Group {
    /// (engine, its words here, left to right)
    options: Vec<(usize, Vec<usize>)>,
    pick: usize,
    rect: RectF,
    /// Script of the chosen reading when it is certain: true = non-Latin.
    foreign: Option<bool>,
}

/// Combines the engines word by word. Words of different engines at the same place
/// form a group, and the group takes the reading of the engine that read it best.
/// Words only one engine saw are kept, so no word on the picture is left without a
/// box. Returns lines of words, in no particular order.
/// `tags[k]`: language of the engine that made run k.
fn merge(runs: &[Vec<RawLine>], tags: &[String]) -> Vec<RawLine> {
    let foreign: Vec<bool> = tags.iter().map(|t| script_of(t) != 0).collect();
    let mut cands: Vec<Cand> = Vec::new();
    let mut nlines = 0;
    for (k, run) in runs.iter().enumerate() {
        for (li, l) in run.iter().enumerate() {
            for (rect, text) in l {
                cands.push(Cand {
                    run: k,
                    line: nlines + li,
                    rect: *rect,
                    text: text.clone(),
                });
            }
        }
        nlines += run.len();
    }
    let n = cands.len();
    let mut parent: Vec<usize> = (0..n).collect();
    if runs.len() > 1 {
        for i in 0..n {
            for j in i + 1..n {
                if cands[i].run != cands[j].run && overlap(&cands[i].rect, &cands[j].rect) > 0.5 {
                    union(&mut parent, i, j);
                }
            }
        }
    }
    let mut members: std::collections::BTreeMap<usize, Vec<usize>> = Default::default();
    for i in 0..n {
        let g = find(&mut parent, i);
        members.entry(g).or_default().push(i);
    }
    let mut gs: Vec<Group> = Vec::new();
    for (_, ms) in members {
        let mut options: Vec<(usize, Vec<usize>)> = Vec::new();
        for &m in &ms {
            match options.iter_mut().find(|o| o.0 == cands[m].run) {
                Some(o) => o.1.push(m),
                None => options.push((cands[m].run, vec![m])),
            }
        }
        options.sort_by_key(|o| o.0);
        for o in &mut options {
            o.1.sort_by(|a, b| cands[*a].rect.l.total_cmp(&cands[*b].rect.l));
        }
        let rect = ms
            .iter()
            .map(|&m| cands[m].rect)
            .reduce(|a, b| a.union(&b))
            .unwrap();
        gs.push(Group {
            options,
            pick: 0,
            rect,
            foreign: None,
        });
    }
    let text_of = |o: &(usize, Vec<usize>)| -> String {
        o.1.iter()
            .map(|&m| cands[m].text.as_str())
            .collect::<Vec<_>>()
            .join(" ")
    };
    let score = |t: &str| -> usize {
        t.split(|c: char| !c.is_alphanumeric())
            .filter(|p| clean_part(p))
            .map(|p| p.chars().count())
            .sum()
    };
    let latin_word = |t: &str| t.chars().any(latin) && !any_foreign(t) && clean(t);
    // Releasing a spell checker takes ~200 ms, so they live for the whole run.
    static SPELLER: std::sync::Mutex<Option<Speller>> = std::sync::Mutex::new(None);
    let mut guard = SPELLER.lock().unwrap_or_else(|e| e.into_inner());
    let speller = guard.get_or_insert_with(Speller::new);
    // First pass: groups where one reading is clearly right.
    // (group, Latin option, foreign option, the foreign one has real non-Latin letters)
    let mut ambiguous: Vec<(usize, usize, usize, bool)> = Vec::new();
    for (gi, g) in gs.iter_mut().enumerate() {
        let texts: Vec<String> = g.options.iter().map(text_of).collect();
        let ascii = (0..texts.len())
            .filter(|&i| texts[i].is_ascii() && clean(&texts[i]))
            .max_by_key(|&i| (!foreign[g.options[i].0], score(&texts[i])));
        let real = (0..texts.len())
            .filter(|&i| real_foreign(&texts[i]))
            .max_by_key(|&i| (foreign[g.options[i].0], score(&texts[i])));
        let look = (0..texts.len()).find(|&i| any_foreign(&texts[i]));
        match (ascii, real.or(look)) {
            (Some(a), Some(f)) => {
                if texts[a].chars().any(|c| c.is_ascii_digit()) && real.is_none() {
                    // "0x80070002" over "0х80070002", "3" over "З".
                    g.pick = a;
                } else {
                    // "mouse" or "тоше", "on" or "оп", "Bicmoqnre" or "включите".
                    ambiguous.push((gi, a, f, real.is_some()));
                }
                continue;
            }
            (None, Some(f)) if real.is_some() => {
                // Real Cyrillic (Greek, CJK...) against junk: that engine knows the script.
                g.pick = f;
                g.foreign = Some(true);
                continue;
            }
            _ => {}
        }
        // Otherwise a well-formed reading, by the engine made for its script
        // ("button" from the English engine, not "buttory" from the Russian one).
        // Among those, a dictionary word ("панели", not "панельт").
        let keys: Vec<_> = (0..texts.len())
            .map(|i| {
                let fits = foreign[g.options[i].0] == any_foreign(&texts[i]);
                let known = texts.iter().any(|t| *t != texts[i])
                    && speller.known(&tags[g.options[i].0], &texts[i]) == Some(true);
                (clean(&texts[i]), fits, known, score(&texts[i]))
            })
            .collect();
        g.pick = (0..texts.len())
            .max_by(|&a, &b| keys[a].cmp(&keys[b]).then(b.cmp(&a)))
            .unwrap_or(0);
        if latin_word(&texts[g.pick]) {
            g.foreign = Some(false);
        } else if real_foreign(&texts[g.pick]) {
            g.foreign = Some(true);
        }
    }
    // Second pass: the dictionary decides, then the script of the neighbours.
    let latin_tag = (0..runs.len())
        .find(|&k| !foreign[k])
        .map(|k| tags[k].as_str());
    for &(gi, a, f, strong) in &ambiguous {
        let (ta, tf) = (text_of(&gs[gi].options[a]), text_of(&gs[gi].options[f]));
        let ka = latin_tag.and_then(|t| speller.known(t, &ta));
        let kf = Some(gs[gi].options[f].0)
            .filter(|&k| foreign[k])
            .and_then(|k| speller.known(&tags[k], &tf));
        let caps = |t: &str| t.chars().filter(|c| c.is_uppercase()).count();
        let want = match (ka, kf) {
            (Some(true), Some(false) | None) => false,
            (Some(false) | None, Some(true)) => true,
            // Lower-case Cyrillic looks like small capitals to the English engine:
            // "в" → "B", "код" → "KOA", "и" → "VI".
            _ if caps(&ta) > caps(&tf) => true,
            _ => {
                // The script of the words around, on the line, then on the picture.
                let r = gs[gi].rect;
                let (mut fo, mut la, mut gfo, mut gla) = (0, 0, 0, 0);
                for g in &gs {
                    let Some(fg) = g.foreign else { continue };
                    let v =
                        (r.b.min(g.rect.b) - r.t.max(g.rect.t)) / r.h().min(g.rect.h()).max(1.0);
                    let dx = (g.rect.l - r.r).max(r.l - g.rect.r).max(0.0);
                    let near = v > 0.5 && dx < 20.0 * r.h();
                    if fg {
                        gfo += 1;
                        fo += near as usize;
                    } else {
                        gla += 1;
                        la += near as usize;
                    }
                }
                if fo != la {
                    fo > la
                } else if gfo != gla {
                    gfo > gla
                } else {
                    strong
                }
            }
        };
        gs[gi].pick = if want { f } else { a };
    }
    drop(guard);
    // Lines: an engine's line, joined with other engines' lines it shares a word with.
    let mut lp: Vec<usize> = (0..nlines).collect();
    for g in &gs {
        let ids: Vec<usize> = g
            .options
            .iter()
            .flat_map(|o| o.1.iter().map(|&m| cands[m].line))
            .collect();
        for w in ids.windows(2) {
            union(&mut lp, w[0], w[1]);
        }
    }
    let mut lines: std::collections::BTreeMap<usize, RawLine> = Default::default();
    for g in &gs {
        for &m in &g.options[g.pick].1 {
            let root = find(&mut lp, cands[m].line);
            lines
                .entry(root)
                .or_default()
                .push((cands[m].rect, latinize(&cands[m].text)));
        }
    }
    lines
        .into_values()
        .map(|mut ws| {
            ws.sort_by(|a, b| a.0.l.total_cmp(&b.0.l));
            ws
        })
        .collect()
}

/// Puts words in reading order: rows top to bottom (lines that share a baseline, like
/// table cells, form one row), lines in a row left to right, words left to right.
/// Selection works on this order, so dragging over text selects what it covers.
fn layout(lines: Vec<RawLine>, out: &mut OcrReply) {
    let mut ls: Vec<(RectF, RawLine)> = lines
        .into_iter()
        .filter(|l| !l.is_empty())
        .map(|l| {
            let r = l.iter().map(|w| w.0).reduce(|a, b| a.union(&b)).unwrap();
            (r, l)
        })
        .collect();
    ls.sort_by(|a, b| a.0.cy().total_cmp(&b.0.cy()));
    // A line joins the current row when its middle is inside the row's first line.
    let mut rows: Vec<Vec<(RectF, RawLine)>> = Vec::new();
    for l in ls {
        match rows.last_mut() {
            Some(row) if l.0.cy() > row[0].0.t && l.0.cy() < row[0].0.b => row.push(l),
            _ => rows.push(vec![l]),
        }
    }
    for (ri, mut row) in rows.into_iter().enumerate() {
        row.sort_by(|a, b| a.0.l.total_cmp(&b.0.l));
        for (rect, ws) in row {
            let first = out.words.len();
            let text = join_words(ws.iter().map(|w| w.1.as_str()));
            for (r, t) in ws {
                out.words.push(Word {
                    rect: r,
                    text: t,
                    line: out.lines.len(),
                });
            }
            out.lines.push(Line {
                rect,
                first_word: first,
                word_count: out.words.len() - first,
                text,
                row: ri,
            });
        }
    }
}

/// Words joined with spaces, except between CJK characters.
pub fn join_words<'a>(ws: impl Iterator<Item = &'a str>) -> String {
    let cjk = |c: char| matches!(c as u32, 0x3040..=0x30FF | 0x3400..=0x4DBF | 0x4E00..=0x9FFF | 0xAC00..=0xD7AF | 0xF900..=0xFAFF | 0xFF00..=0xFFEF);
    let mut s = String::new();
    for w in ws {
        let glue = match (s.chars().last(), w.chars().next()) {
            (Some(a), Some(b)) => !(cjk(a) && cjk(b)),
            _ => false,
        };
        if glue {
            s.push(' ');
        }
        s.push_str(w);
    }
    s
}

/// Groups lines into paragraphs: translating whole sentences reads far better
/// than translating line by line. A line continues the paragraph whose last line
/// sits right above it (columns interleave in reading order).
fn blocks(lines: &[Line], bgra: &[u8], sel: RectI) -> Vec<Block> {
    let mut out: Vec<Block> = Vec::new();
    let mut last: Vec<RectF> = Vec::new();
    for l in lines {
        let h = l.rect.h().max(1.0);
        let joins = |p: &RectF| {
            let ph = p.h().max(1.0);
            let ratio = h / ph;
            let gap = l.rect.t - p.b;
            let overlap = l.rect.l < p.r && p.l < l.rect.r;
            (0.72..1.38).contains(&ratio)
                && gap > -0.4 * h
                && gap < 0.95 * h.max(ph)
                && overlap
                && (l.rect.l - p.l).abs() < 6.0 * h
        };
        if let Some(bi) = (0..out.len()).rev().find(|&i| joins(&last[i])) {
            let b = &mut out[bi];
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
            last[bi] = l.rect;
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
            last.push(l.rect);
        }
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

// ---------------- text selection ----------------

/// The word under `q`. Boxes are grown by 3 px, so between two close words the
/// nearer one wins.
pub fn word_at(r: &OcrReply, q: P) -> Option<usize> {
    let d = |w: &Word| {
        let dx = (w.rect.l - q.x).max(q.x - w.rect.r).max(0.0);
        let dy = (w.rect.t - q.y).max(q.y - w.rect.b).max(0.0);
        dx + dy
    };
    r.words
        .iter()
        .enumerate()
        .filter(|(_, w)| w.rect.inflate(3.0).contains(q))
        .min_by(|a, b| d(a.1).total_cmp(&d(b.1)))
        .map(|(i, _)| i)
}

/// The word a drag point stands for, like a text caret: first the row under the
/// pointer (or the closest one above or below), then the closest word in that row.
/// Past the end of a row this is the row's last word, before its start the first;
/// above all text the first word, below it the last.
pub fn caret(r: &OcrReply, q: P) -> Option<usize> {
    if let Some(i) = word_at(r, q) {
        return Some(i);
    }
    let rows = r.lines.last()?.row + 1;
    let mut ext: Vec<Option<(f32, f32)>> = vec![None; rows];
    for l in &r.lines {
        let e = &mut ext[l.row];
        *e = Some(e.map_or((l.rect.t, l.rect.b), |(t, b)| {
            (t.min(l.rect.t), b.max(l.rect.b))
        }));
    }
    if ext.iter().flatten().all(|e| q.y > e.1) {
        return Some(r.words.len() - 1);
    }
    if ext.iter().flatten().all(|e| q.y < e.0) {
        return Some(0);
    }
    let row = (0..rows)
        .filter_map(|i| ext[i].map(|(t, b)| (i, (t - q.y).max(q.y - b).max(0.0))))
        .min_by(|a, b| a.1.total_cmp(&b.1))?
        .0;
    r.words
        .iter()
        .enumerate()
        .filter(|(_, w)| r.lines[w.line].row == row)
        .min_by(|a, b| {
            let d = |w: &Word| (w.rect.l - q.x).max(q.x - w.rect.r).max(0.0);
            d(a.1).total_cmp(&d(b.1))
        })
        .map(|(i, _)| i)
}

/// Text of words `a..=b`: lines of one row joined by a space (a tab across a wide
/// gap, as between table columns), rows by new lines.
pub fn text_range(r: &OcrReply, a: usize, b: usize) -> String {
    let (a, b) = (a.min(b), a.max(b));
    let mut out = String::new();
    let mut prev: Option<&Line> = None;
    for line in &r.lines {
        let lo = line.first_word;
        let hi = line.first_word + line.word_count;
        if hi <= a || lo > b {
            continue;
        }
        let text = if lo >= a && hi - 1 <= b {
            line.text.clone()
        } else {
            join_words(
                r.words[lo.max(a)..hi.min(b + 1)]
                    .iter()
                    .map(|w| w.text.as_str()),
            )
        };
        if let Some(p) = prev {
            if p.row != line.row {
                out.push('\n');
            } else if line.rect.l - p.rect.r > 2.0 * line.rect.h().max(p.rect.h()) {
                out.push('\t');
            } else {
                out.push(' ');
            }
        }
        out.push_str(&text);
        prev = Some(line);
    }
    out
}

// ---------------- test harness ----------------

/// `--ocr-test <dir> [lang]`: for every `name.png` with `name.box` (one word per line:
/// `l t r b<TAB>word`, in reading order) and `name.txt` (the text) next to it:
/// recognises the picture, lists words that got no box, drags between every pair of
/// boxed words and checks that exactly the words in between get selected, and shows
/// the copied text when it differs. Writes `ocr-test.txt`; the exit code is the number
/// of pictures where a drag selected wrong words.
pub fn ocr_test(
    dir: &std::path::Path,
    lang: &str,
    load: impl Fn(&std::path::Path) -> Option<(u32, u32, Vec<u8>)>,
) -> i32 {
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphanumeric())
            .flat_map(|c| c.to_lowercase())
            .collect()
    };
    let inter = |a: &RectF, b: &RectF| {
        (a.r.min(b.r) - a.l.max(b.l)).max(0.0) * (a.b.min(b.b) - a.t.max(b.t)).max(0.0)
    };
    let mut log = String::new();
    let mut bad = 0;
    let (mut total_words, mut total_misread, mut total_ms) = (0, 0, 0u128);
    let mut total_unseen = 0;
    let mut files: Vec<_> = std::fs::read_dir(dir)
        .map(|d| d.flatten().map(|e| e.path()).collect())
        .unwrap_or_default();
    files.sort();
    for png in files
        .iter()
        .filter(|p| p.extension().is_some_and(|e| e == "png"))
    {
        let (Ok(boxes), Ok(expected)) = (
            std::fs::read_to_string(png.with_extension("box")),
            std::fs::read_to_string(png.with_extension("txt")),
        ) else {
            continue;
        };
        let gt: Vec<(RectF, String)> = boxes
            .lines()
            .filter_map(|l| {
                let (nums, word) = l.split_once('\t')?;
                let v: Vec<f32> = nums.split(' ').filter_map(|x| x.parse().ok()).collect();
                (v.len() == 4).then(|| (RectF::new(v[0], v[1], v[2], v[3]), word.to_string()))
            })
            .collect();
        let name = png.file_name().unwrap().to_string_lossy().to_string();
        let Some((w, h, px)) = load(png) else {
            log.push_str(&format!("FAIL {name}: cannot load\n"));
            bad += 1;
            continue;
        };
        let sel = RectI::new(0, 0, w as i32, h as i32);
        let t0 = std::time::Instant::now();
        let r = match recognize(&px, sel, lang) {
            Ok(r) => r,
            Err(e) => {
                log.push_str(&format!("FAIL {name}: {e}\n"));
                bad += 1;
                continue;
            }
        };
        let ms = t0.elapsed().as_millis();
        total_ms += ms;
        // OCR words covering each expected word.
        let cover: Vec<Vec<usize>> = gt
            .iter()
            .map(|(g, _)| {
                (0..r.words.len())
                    .filter(|&i| inter(g, &r.words[i].rect) > 0.3 * g.w() * g.h())
                    .collect()
            })
            .collect();
        let no_box: Vec<&str> = (0..gt.len())
            .filter(|&k| {
                let a: f32 = cover[k]
                    .iter()
                    .map(|&i| inter(&gt[k].0, &r.words[i].rect))
                    .sum();
                a < 0.5 * gt[k].0.w() * gt[k].0.h()
            })
            .map(|k| gt[k].1.as_str())
            .collect();
        let misread: Vec<String> = (0..gt.len())
            .filter_map(|k| {
                let got: String = cover[k].iter().map(|&i| r.words[i].text.as_str()).collect();
                (norm(&got) != norm(&gt[k].1)).then(|| format!("{}→{}", gt[k].1, got))
            })
            .collect();
        total_words += gt.len();
        total_misread += misread.len();
        // Drag from the start of word i to the end of word j, as a user would.
        let mut fails = Vec::new();
        let mut pairs = 0;
        // Pairs whose ends have a box: a word OCR did not see cannot be dragged from.
        let seen: Vec<bool> = (0..gt.len())
            .map(|k| !no_box.contains(&gt[k].1.as_str()))
            .collect();
        for i in (0..gt.len()).filter(|&i| seen[i]) {
            for j in (i..gt.len()).filter(|&j| seen[j]) {
                pairs += 1;
                let a = P {
                    x: gt[i].0.l + 1.0,
                    y: gt[i].0.cy(),
                };
                let b = P {
                    x: gt[j].0.r - 1.0,
                    y: gt[j].0.cy(),
                };
                let Some(anchor) = caret(&r, a) else { continue };
                let mut end = anchor;
                for s in 0..=24 {
                    if let Some(x) = caret(&r, a.lerp(b, s as f32 / 24.0)) {
                        end = x;
                    }
                }
                let (lo, hi) = (anchor.min(end), anchor.max(end));
                let mut miss = Vec::new();
                let mut extra = Vec::new();
                for k in 0..gt.len() {
                    if !seen[k] {
                        continue;
                    }
                    let selected = cover[k].iter().any(|&x| x >= lo && x <= hi);
                    let want = k >= i && k <= j;
                    if want && !selected {
                        miss.push(gt[k].1.as_str());
                    }
                    // A word OCR merged with a wanted neighbour ("foxjumps") is fine.
                    let shared = cover[k]
                        .iter()
                        .any(|x| (i..=j).any(|m| cover[m].contains(x)));
                    if !want && selected && !shared {
                        extra.push(gt[k].1.as_str());
                    }
                }
                if !miss.is_empty() || !extra.is_empty() {
                    fails.push(format!(
                        "{} .. {}: missed [{}] extra [{}]",
                        gt[i].1,
                        gt[j].1,
                        miss.join(" "),
                        extra.join(" ")
                    ));
                }
            }
        }
        let copied = if r.words.is_empty() {
            String::new()
        } else {
            text_range(&r, 0, r.words.len() - 1)
        };
        let ok = fails.is_empty();
        if !ok {
            bad += 1;
        }
        total_unseen += no_box.len();
        log.push_str(&format!(
            "{} {name} [{}] {ms} ms, {} words ({} expected), misread {}, drags failed {}/{}\n",
            if ok { "ok  " } else { "FAIL" },
            r.lang,
            r.words.len(),
            gt.len(),
            misread.len(),
            fails.len(),
            pairs
        ));
        if !no_box.is_empty() {
            log.push_str(&format!(
                "     no box (OCR did not see): {}\n",
                no_box.join(" ")
            ));
        }
        for f in fails.iter().take(4) {
            log.push_str(&format!("     drag {f}\n"));
        }
        if !misread.is_empty() {
            log.push_str(&format!("     misread: {}\n", misread.join("  ")));
        }
        if copied.split_whitespace().ne(expected.split_whitespace()) {
            for l in copied.lines() {
                log.push_str(&format!("     | {l}\n"));
            }
        }
    }
    log.push_str(&format!(
        "{bad} failed drags; of {total_words} words {total_unseen} without a box, {total_misread} misread; OCR {total_ms} ms\n"
    ));
    let _ = std::fs::write(dir.join("ocr-test.txt"), &log);
    bad.min(100)
}
