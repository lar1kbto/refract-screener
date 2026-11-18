//! The settings window: screenshot folder (change / open / reset), format, what
//! happens after a capture, capture options, start with Windows, Snipping Tool
//! replacement, updates and hotkeys. Changes apply immediately and are saved to
//! config.json.

use crate::gdi::*;
use crate::hotkey::{self, Action, Combo};
use crate::menu_id::*;
use std::cell::RefCell;
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub struct Snapshot {
    pub cfg: crate::config::Config,
    pub dir: std::path::PathBuf,
    pub autostart: bool,
    pub prtsc: bool,
    pub update_ready: Option<String>,
}

#[derive(Clone)]
enum Kind {
    Toggle(usize, bool),
    Button(usize, &'static str, bool, bool), // id, label, primary, enabled
    Seg(usize, usize, bool),                 // png id, webp id, png selected
    Chip(Action, usize, Chip),               // shortcut of an action
    Remove(Action, usize),                   // the x on a chip
    Add(Action),                             // "+": record one more shortcut
    ResetKeys(bool),                         // enabled
}

#[derive(Clone, Copy, PartialEq)]
enum Chip {
    Normal,
    /// The same shortcut is set more than once: only the first one works.
    Conflict,
    /// PrintScreen / Win+Shift+S while "Replace Snipping Tool" is off.
    Windows,
}

/// A shortcut being recorded: which action, which chip (None = a new one).
#[derive(Clone)]
struct Rec {
    action: Action,
    index: Option<usize>,
    /// Modifiers held right now, shown live in the chip.
    mods: Combo,
    since: std::time::Instant,
}

const TIMER_REC: usize = 1;

#[derive(Clone)]
struct Item {
    r: RECT,
    kind: Kind,
}

#[derive(Default)]
struct State {
    hwnd: Option<HWND>,
    hover: Option<(usize, i32)>, // item index, segment (0/1) for Seg
    k: f32,
    rec: Option<Rec>,
    /// Message under the hotkeys (true = error).
    note: Option<(String, bool)>,
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State::default());
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    S.with(|s| f(&mut s.borrow_mut()))
}

const W: f32 = 1040.0;
const H: f32 = 512.0;
const ROW: f32 = 40.0;

struct Text {
    at: (f32, f32),
    s: String,
    size: f32,
    weight: i32,
    color: COLORREF,
}

/// Everything on the page, in logical pixels (scaled by k when used).
fn layout(snap: &Snapshot) -> (Vec<Item>, Vec<Text>, Vec<(f32, f32, f32)>) {
    let c = &snap.cfg;
    let mut items = Vec::new();
    let mut texts = Vec::new();
    let mut lines = Vec::new();
    let head = |texts: &mut Vec<Text>, x: f32, y: f32, s: &str| {
        texts.push(Text {
            at: (x, y),
            s: s.to_string(),
            size: 11.5,
            weight: 600,
            color: FAINT,
        });
    };
    let r = |l: f32, t: f32, w: f32, h: f32| (l, t, w, h);
    let push = |items: &mut Vec<Item>, (l, t, w, h): (f32, f32, f32, f32), kind: Kind| {
        items.push(Item {
            r: RECT {
                left: l as i32,
                top: t as i32,
                right: (l + w) as i32,
                bottom: (t + h) as i32,
            },
            kind,
        });
    };

    // Left column: folder + saving.
    let (lx, lw) = (28.0, LW);
    head(&mut texts, lx, 104.0, "SCREENSHOT FOLDER");
    push(
        &mut items,
        r(lx, 176.0, 140.0, 34.0),
        Kind::Button(CHANGE_DIR, "Change folder...", true, true),
    );
    push(
        &mut items,
        r(lx + 148.0, 176.0, 76.0, 34.0),
        Kind::Button(OPEN_DIR, "Open", false, true),
    );
    push(
        &mut items,
        r(lx + 232.0, 176.0, 76.0, 34.0),
        Kind::Button(RESET_DIR, "Reset", false, !c.save_dir.is_empty()),
    );

    head(&mut texts, lx, 236.0, "SAVING");
    let rows_l: [(&str, Option<(usize, bool)>); 5] = [
        ("Format", None),
        ("Copy to clipboard", Some((COPY, c.copy_to_clipboard))),
        (
            "Save to the folder automatically",
            Some((AUTOSAVE, c.auto_save)),
        ),
        ("Play sound when saved", Some((SOUND, c.sound))),
        ("Show notifications", Some((NOTIFY, c.notify))),
    ];
    for (i, (label, t)) in rows_l.iter().enumerate() {
        let y = 258.0 + i as f32 * ROW;
        texts.push(Text {
            at: (lx, y + if i == 0 { 3.0 } else { 11.0 }),
            s: label.to_string(),
            size: 14.0,
            weight: 400,
            color: INK,
        });
        match t {
            Some((id, on)) => push(
                &mut items,
                r(lx + lw - 40.0, y + 9.0, 40.0, 22.0),
                Kind::Toggle(*id, *on),
            ),
            None => push(
                &mut items,
                r(lx + lw - 140.0, y + 5.0, 140.0, 30.0),
                Kind::Seg(PNG, WEBP, c.format == crate::config::ImageFormat::Png),
            ),
        }
        if i > 0 {
            lines.push((lx, lx + lw, y));
        }
    }

    // Middle column: capture + system.
    let (rx, rw) = (MX, 300.0);
    head(&mut texts, rx, 104.0, "CAPTURE");
    let rows_c = [
        ("Include mouse cursor", CURSOR, c.capture_cursor),
        ("Magnifier while selecting", LOUPE, c.magnifier),
        ("Snap to windows", SNAP, c.snap_windows),
        ("Recognise text automatically", AUTO_OCR, c.auto_ocr),
    ];
    for (i, (label, id, on)) in rows_c.iter().enumerate() {
        let y = 126.0 + i as f32 * ROW;
        texts.push(Text {
            at: (rx, y + 11.0),
            s: label.to_string(),
            size: 14.0,
            weight: 400,
            color: INK,
        });
        push(
            &mut items,
            r(rx + rw - 40.0, y + 9.0, 40.0, 22.0),
            Kind::Toggle(*id, *on),
        );
        if i > 0 {
            lines.push((rx, rx + rw, y));
        }
    }
    head(&mut texts, rx, 300.0, "SYSTEM");
    let rows_s = [
        ("Start with Windows", AUTOSTART, snap.autostart),
        ("Replace Snipping Tool (PrintScreen)", PRTSC, snap.prtsc),
        ("Update automatically", AUTO_UPDATE, c.auto_update),
    ];
    for (i, (label, id, on)) in rows_s.iter().enumerate() {
        let y = 322.0 + i as f32 * ROW;
        texts.push(Text {
            at: (rx, y + 11.0),
            s: label.to_string(),
            size: 14.0,
            weight: 400,
            color: INK,
        });
        push(
            &mut items,
            r(rx + rw - 40.0, y + 9.0, 40.0, 22.0),
            Kind::Toggle(*id, *on),
        );
        if i > 0 {
            lines.push((rx, rx + rw, y));
        }
    }
    match &snap.update_ready {
        Some(_) => push(
            &mut items,
            r(rx, 452.0, 168.0, 34.0),
            Kind::Button(APPLY_UPDATE, "Restart to update", true, true),
        ),
        None => push(
            &mut items,
            r(rx, 452.0, 160.0, 34.0),
            Kind::Button(CHECK_UPDATE, "Check for updates", false, true),
        ),
    }
    push(
        &mut items,
        r(rx + rw - 120.0, 452.0, 120.0, 34.0),
        Kind::Button(EDIT_CFG, "config.json", false, true),
    );
    // Header, top right.
    push(
        &mut items,
        r(W - 28.0 - 112.0, 30.0, 112.0, 34.0),
        Kind::Button(HISTORY, "History", false, true),
    );
    hotkeys_layout(snap, &mut items, &mut texts);
    (items, texts, lines)
}

const LW: f32 = 316.0;
const MX: f32 = 372.0;
/// Hotkeys column.
const HX: f32 = 700.0;
const HW: f32 = 312.0;
const CHIP_H: f32 = 28.0;
const CHIP_FONT: f32 = 12.5;
/// Room for the x at the right end of a chip.
const CHIP_X: f32 = 22.0;
const MAX_KEYS: usize = 3;
/// Recording gives up after this long without a shortcut.
const REC_TIMEOUT_S: u64 = 30;

fn short_desc(a: Action) -> &'static str {
    match a {
        Action::Region => "select an area",
        Action::Fullscreen => "monitor under the cursor",
        Action::Window => "active window",
        Action::Scroll => "long page",
    }
}

/// Every shortcut with the action that uses it, in the order the hook tries
/// them (the first one wins).
fn uses(c: &crate::config::Config) -> Vec<(Combo, Action, usize)> {
    let mut out = Vec::new();
    for a in Action::ALL {
        for (i, k) in a.keys(c).iter().enumerate() {
            if let Some(k) = hotkey::parse(k) {
                out.push((k, a, i));
            }
        }
    }
    out
}

fn chip_state(c: &crate::config::Config, combo: Combo) -> Chip {
    if !c.replace_snipping_tool && hotkey::is_snipping_combo(&combo) {
        Chip::Windows
    } else if uses(c).iter().filter(|(k, ..)| *k == combo).count() > 1 {
        Chip::Conflict
    } else {
        Chip::Normal
    }
}

fn rec_label(r: &Rec) -> String {
    if r.mods.has_mods() {
        // label() of a combo without a key ends with "+".
        format!("{}...", r.mods.label())
    } else {
        "Press a shortcut".into()
    }
}

fn hotkeys_layout(snap: &Snapshot, items: &mut Vec<Item>, texts: &mut Vec<Text>) {
    let c = &snap.cfg;
    let rec = st(|s| s.rec.clone());
    let rect = |l: f32, t: f32, w: f32, h: f32| RECT {
        left: l as i32,
        top: t as i32,
        right: (l + w) as i32,
        bottom: (t + h) as i32,
    };
    texts.push(Text {
        at: (HX, 104.0),
        s: "HOTKEYS".into(),
        size: 11.5,
        weight: 600,
        color: FAINT,
    });
    let mut y = 126.0;
    for a in Action::ALL {
        texts.push(Text {
            at: (HX, y),
            s: a.name().into(),
            size: 14.0,
            weight: 400,
            color: INK,
        });
        let nw = text_width(a.name(), 14.0, 400);
        texts.push(Text {
            at: (HX + nw + 8.0, y + 2.5),
            s: short_desc(a).into(),
            size: 11.5,
            weight: 400,
            color: FAINT,
        });
        let (mut cx, mut cy) = (HX, y + 24.0);
        let place = |w: f32, cx: &mut f32, cy: &mut f32| {
            if *cx > HX && *cx + w > HX + HW {
                *cx = HX;
                *cy += CHIP_H + 6.0;
            }
            let at = (*cx, *cy);
            *cx += w + 6.0;
            at
        };
        let keys = a.keys(c);
        for (i, k) in keys.iter().enumerate() {
            let recording = rec
                .as_ref()
                .filter(|r| r.action == a && r.index == Some(i));
            let (label, state) = match (recording, hotkey::parse(k)) {
                (Some(r), _) => (rec_label(r), Chip::Normal),
                (None, Some(combo)) => (combo.label(), chip_state(c, combo)),
                (None, None) => (format!("{k}?"), Chip::Conflict),
            };
            let w = text_width(&label, CHIP_FONT, 500).ceil() + 18.0 + CHIP_X;
            let (x, yy) = place(w, &mut cx, &mut cy);
            if recording.is_none() {
                items.push(Item {
                    r: rect(x + w - CHIP_X - 2.0, yy, CHIP_X + 2.0, CHIP_H),
                    kind: Kind::Remove(a, i),
                });
            }
            items.push(Item {
                r: rect(x, yy, w, CHIP_H),
                kind: Kind::Chip(a, i, state),
            });
        }
        let adding = rec
            .as_ref()
            .filter(|r| r.action == a && r.index.is_none());
        let add_w = match adding {
            Some(r) => Some(text_width(&rec_label(r), CHIP_FONT, 500) + 24.0),
            None if keys.len() < MAX_KEYS && keys.is_empty() => {
                Some(text_width("Add shortcut", CHIP_FONT, 500).ceil() + CHIP_H + 14.0)
            }
            None if keys.len() < MAX_KEYS => Some(CHIP_H),
            None => None,
        };
        if let Some(w) = add_w {
            let (x, yy) = place(w, &mut cx, &mut cy);
            items.push(Item {
                r: rect(x, yy, w, CHIP_H),
                kind: Kind::Add(a),
            });
        }
        y = cy + CHIP_H + 12.0;
    }
    let defaults = crate::config::Config::default();
    let changed = Action::ALL
        .iter()
        .any(|a| a.keys(c) != a.keys(&defaults));
    items.push(Item {
        r: rect(HX, 452.0, 150.0, 34.0),
        kind: Kind::ResetKeys(changed),
    });
}

/// The line under the hotkeys: what to do while recording, else what is wrong.
fn hotkeys_note(snap: &Snapshot) -> Option<(String, bool)> {
    let c = &snap.cfg;
    if let Some(n) = st(|s| s.note.clone()) {
        return Some(n);
    }
    if st(|s| s.rec.is_some()) {
        return Some(("Press the new shortcut. Esc cancels.".into(), false));
    }
    let all = uses(c);
    for (i, (k, a, _)) in all.iter().enumerate() {
        if !c.replace_snipping_tool && hotkey::is_snipping_combo(k) {
            continue;
        }
        if let Some((_, b, _)) = all[i + 1..].iter().find(|(x, ..)| x == k) {
            let msg = if a == b {
                format!("{} is set twice for {}.", k.label(), a.name())
            } else {
                format!(
                    "{} is set for {} and {}: only {} gets it.",
                    k.label(),
                    a.name(),
                    b.name(),
                    a.name()
                )
            };
            return Some((msg, true));
        }
    }
    if !c.replace_snipping_tool && all.iter().any(|(k, ..)| hotkey::is_snipping_combo(k)) {
        return Some((
            "PrtSc and Win+Shift+S go to Windows while Replace Snipping Tool is off.".into(),
            false,
        ));
    }
    None
}

// ---------------------------------------------------------------- recording

fn rec_begin(action: Action, index: Option<usize>) {
    let Some(h) = st(|s| s.hwnd) else { return };
    st(|s| {
        s.note = None;
        s.rec = Some(Rec {
            action,
            index,
            mods: Combo::unpack(0),
            since: std::time::Instant::now(),
        })
    });
    crate::guard::pause(true);
    hotkey::record_start(h);
    unsafe {
        SetTimer(h, TIMER_REC, 1000, None);
    }
    refresh();
}

fn rec_end() {
    if st(|s| s.rec.take()).is_none() {
        return;
    }
    hotkey::record_stop();
    crate::guard::pause(false);
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            let _ = KillTimer(h, TIMER_REC);
        }
    }
    refresh();
}

fn on_recorded(combo: Combo) {
    let Some(rec) = st(|s| s.rec.clone()) else {
        return;
    };
    if combo.vk == VK_ESCAPE.0 as u32 && !combo.has_mods() {
        rec_end();
        return;
    }
    if let Some(p) = hotkey::problem(&combo) {
        // Keep recording: the next press may be fine.
        st(|s| s.note = Some((p, true)));
        refresh();
        return;
    }
    let snap = crate::settings_snapshot();
    let mut keys = rec.action.keys(&snap.cfg).clone();
    let same = keys
        .iter()
        .enumerate()
        .any(|(i, k)| hotkey::parse(k) == Some(combo) && Some(i) != rec.index);
    rec_end();
    if same {
        st(|s| {
            s.note = Some((
                format!("{} is already set for {}.", combo.label(), rec.action.name()),
                false,
            ))
        });
        refresh();
        return;
    }
    match rec.index {
        Some(i) if i < keys.len() => keys[i] = combo.text(),
        _ => keys.push(combo.text()),
    }
    crate::set_hotkeys(rec.action, keys);
    refresh();
}

fn remove_key(action: Action, i: usize) {
    let mut keys = action.keys(&crate::settings_snapshot().cfg).clone();
    if i < keys.len() {
        keys.remove(i);
        crate::set_hotkeys(action, keys);
    }
}

fn reset_keys() {
    let d = crate::config::Config::default();
    for a in Action::ALL {
        crate::set_hotkeys(a, a.keys(&d).clone());
    }
}

fn scaled(r: &RECT, k: f32) -> RECT {
    rect(
        (r.left as f32 * k) as i32,
        (r.top as f32 * k) as i32,
        (r.right as f32 * k) as i32,
        (r.bottom as f32 * k) as i32,
    )
}

unsafe fn draw(dc: HDC, rc: RECT) {
    let k = st(|s| s.k.max(1.0));
    let hover = st(|s| s.hover);
    let snap = crate::settings_snapshot();
    let px = |v: f32| (v * k) as i32;
    fill(dc, rc, BG);

    // Header.
    logo(dc, px(28.0), px(24.0), px(48.0));
    let mut t = rect(px(88.0), px(26.0), rc.right - px(28.0), px(54.0));
    text(
        dc,
        "Refract-screener",
        &mut t,
        21.0 * k,
        600,
        INK,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let mut t = rect(px(88.0), px(54.0), rc.right - px(28.0), px(76.0));
    text(
        dc,
        &format!("Settings  \u{00B7}  version {}", crate::update::current()),
        &mut t,
        13.0 * k,
        400,
        DIM,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    fill(
        dc,
        rect(px(28.0), px(88.0), rc.right - px(28.0), px(88.0) + 1),
        EDGE,
    );

    let (items, texts, lines) = layout(&snap);
    for tx in &texts {
        let mut r = rect(
            px(tx.at.0),
            px(tx.at.1),
            px(tx.at.0 + 300.0),
            px(tx.at.1 + 24.0),
        );
        text(
            dc,
            &tx.s,
            &mut r,
            tx.size * k,
            tx.weight,
            tx.color,
            DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        );
    }
    for (a, b, y) in lines {
        fill(
            dc,
            rect(px(a), px(y), px(b), px(y) + 1),
            rgb(0x18, 0x22, 0x1E),
        );
    }

    // Folder path box.
    let pb = rect(px(28.0), px(126.0), px(28.0 + LW), px(166.0));
    frame(dc, pb, px(12.0), PANEL, EDGE);
    let mut t = rect(pb.left + px(14.0), pb.top, pb.right - px(12.0), pb.bottom);
    text(
        dc,
        &snap.dir.display().to_string(),
        &mut t,
        13.5 * k,
        400,
        INK,
        DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_PATH_ELLIPSIS,
    );

    let rec = st(|s| s.rec.clone());
    let mut keys_bottom = 0;
    for (i, it) in items.iter().enumerate() {
        let r = scaled(&it.r, k);
        let hov = hover.map(|h| h.0) == Some(i);
        match &it.kind {
            Kind::Remove(..) => {}
            Kind::Chip(a, idx, state) => {
                keys_bottom = keys_bottom.max(it.r.bottom);
                let recording = rec
                    .as_ref()
                    .is_some_and(|x| x.action == *a && x.index == Some(*idx));
                let x_hov = hover
                    .and_then(|h| items.get(h.0))
                    .is_some_and(|h| matches!(h.kind, Kind::Remove(b, j) if b == *a && j == *idx));
                let label = match (recording, rec.as_ref()) {
                    (true, Some(x)) => rec_label(x),
                    _ => a
                        .keys(&snap.cfg)
                        .get(*idx)
                        .map(|s| match hotkey::parse(s) {
                            Some(c) => c.label(),
                            None => format!("{s}?"),
                        })
                        .unwrap_or_default(),
                };
                let lit = hov || x_hov;
                let (bg, edge, ink) = if recording {
                    (rgb(0x12, 0x22, 0x1B), MINT, MINT)
                } else {
                    match state {
                        Chip::Normal => (
                            if lit { rgb(0x1C, 0x27, 0x22) } else { PANEL },
                            if lit { rgb(0x3A, 0x5A, 0x4B) } else { EDGE },
                            INK,
                        ),
                        Chip::Conflict => (
                            if lit { rgb(0x2A, 0x1A, 0x1D) } else { rgb(0x1E, 0x14, 0x16) },
                            rgb(0xC8, 0x52, 0x60),
                            rgb(0xFF, 0x9A, 0xA4),
                        ),
                        Chip::Windows => (
                            BG,
                            if lit { EDGE } else { rgb(0x18, 0x22, 0x1E) },
                            FAINT,
                        ),
                    }
                };
                frame(dc, r, r.bottom - r.top, bg, edge);
                let mut t = rect(r.left + px(12.0), r.top, r.right - px(CHIP_X), r.bottom);
                text(
                    dc,
                    &label,
                    &mut t,
                    CHIP_FONT * k,
                    500,
                    ink,
                    DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
                );
                if !recording {
                    let t = rect(r.right - px(CHIP_X), r.top, r.right - px(4.0), r.bottom);
                    cross(dc, t, 6.5 * k, 1.4 * k, if x_hov { INK } else { FAINT });
                }
            }
            Kind::Add(a) => {
                keys_bottom = keys_bottom.max(it.r.bottom);
                let recording = rec
                    .as_ref()
                    .filter(|x| x.action == *a && x.index.is_none());
                let empty = a.keys(&snap.cfg).is_empty();
                let (label, edge, ink) = match recording {
                    Some(x) => (rec_label(x), MINT, MINT),
                    None if empty => (
                        "Add shortcut".to_string(),
                        if hov { rgb(0x3A, 0x5A, 0x4B) } else { EDGE },
                        if hov { INK } else { DIM },
                    ),
                    None => (
                        String::new(),
                        if hov { rgb(0x3A, 0x5A, 0x4B) } else { EDGE },
                        if hov { INK } else { DIM },
                    ),
                };
                let bg = if recording.is_some() {
                    rgb(0x12, 0x22, 0x1B)
                } else if hov {
                    rgb(0x1C, 0x27, 0x22)
                } else {
                    BG
                };
                frame(dc, r, r.bottom - r.top, bg, edge);
                if label.is_empty() {
                    plus(dc, r, 10.0 * k, ink);
                } else if recording.is_none() {
                    // "+ Add shortcut": the sign in a square at the left, then the text.
                    let h = r.bottom - r.top;
                    plus(dc, rect(r.left + px(4.0), r.top, r.left + px(4.0) + h, r.bottom), 10.0 * k, ink);
                    let mut t = rect(r.left + px(4.0) + h - px(4.0), r.top, r.right, r.bottom);
                    text(dc, &label, &mut t, CHIP_FONT * k, 500, ink, DT_SINGLELINE | DT_LEFT | DT_VCENTER);
                } else {
                    let mut t = r;
                    text(dc, &label, &mut t, CHIP_FONT * k, 500, ink, DT_SINGLELINE | DT_CENTER | DT_VCENTER);
                }
            }
            Kind::ResetKeys(enabled) => {
                if *enabled {
                    button(dc, r, "Reset to defaults", false, hov, k);
                } else {
                    frame(dc, r, px(10.0), BG, rgb(0x18, 0x22, 0x1E));
                    let mut t = r;
                    text(
                        dc,
                        "Reset to defaults",
                        &mut t,
                        13.5 * k,
                        500,
                        FAINT,
                        DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                    );
                }
            }
            Kind::Toggle(_, on) => toggle(dc, r, *on, hov),
            Kind::Button(_, label, primary, enabled) => {
                if *enabled {
                    button(dc, r, label, *primary, hov, k);
                } else {
                    frame(dc, r, px(10.0), BG, rgb(0x18, 0x22, 0x1E));
                    let mut t = r;
                    text(
                        dc,
                        label,
                        &mut t,
                        13.5 * k,
                        500,
                        FAINT,
                        DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                    );
                }
            }
            Kind::Seg(_, _, png) => {
                frame(dc, r, px(10.0), PANEL, EDGE);
                let mid = (r.left + r.right) / 2;
                let halves = [
                    rect(r.left + 3, r.top + 3, mid, r.bottom - 3),
                    rect(mid, r.top + 3, r.right - 3, r.bottom - 3),
                ];
                for (j, h) in halves.iter().enumerate() {
                    let sel = (j == 0) == *png;
                    let hv = hover == Some((i, j as i32));
                    if sel {
                        round(dc, *h, px(8.0), rgb(0x1F, 0x3A, 0x2F));
                    } else if hv {
                        round(dc, *h, px(8.0), rgb(0x18, 0x22, 0x1E));
                    }
                    let mut t = *h;
                    text(
                        dc,
                        if j == 0 { "PNG" } else { "WebP" },
                        &mut t,
                        13.0 * k,
                        if sel { 600 } else { 500 },
                        if sel { MINT } else { DIM },
                        DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                    );
                }
            }
        }
    }
    let note = match snap.cfg.format {
        crate::config::ImageFormat::Png => "Lossless PNG, opens everywhere",
        crate::config::ImageFormat::WebP => "Lossless WebP, about 30% smaller",
    };
    let mut t = rect(px(28.0), px(280.0), px(220.0), px(296.0));
    text(
        dc,
        note,
        &mut t,
        11.5 * k,
        400,
        FAINT,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    if let Some((msg, err)) = hotkeys_note(&snap) {
        let y = keys_bottom as f32 + 2.0;
        let mut t = rect(px(HX), px(y), px(HX + HW), px(446.0));
        text(
            dc,
            &msg,
            &mut t,
            12.0 * k,
            400,
            if err { rgb(0xFF, 0x9A, 0xA4) } else { DIM },
            DT_WORDBREAK | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        );
    }
}

fn hit(x: i32, y: i32) -> Option<(usize, i32)> {
    let k = st(|s| s.k.max(1.0));
    let (items, _, _) = layout(&crate::settings_snapshot());
    for (i, it) in items.iter().enumerate() {
        let r = scaled(&it.r, k);
        if inside(&r, x, y) {
            if let Kind::Button(_, _, _, false) | Kind::ResetKeys(false) = it.kind {
                return None;
            }
            let seg = if let Kind::Seg(..) = it.kind {
                (x >= (r.left + r.right) / 2) as i32
            } else {
                0
            };
            return Some((i, seg));
        }
    }
    None
}

fn click(x: i32, y: i32) {
    let hit = hit(x, y);
    let (items, _, _) = layout(&crate::settings_snapshot());
    let kind = hit.and_then(|(i, _)| items.get(i)).map(|it| it.kind.clone());
    // Any click ends a recording; clicking the recorded chip again just cancels.
    if let Some(r) = st(|s| s.rec.clone()) {
        rec_end();
        let same = match kind {
            Some(Kind::Chip(a, i, _)) => r.action == a && r.index == Some(i),
            Some(Kind::Add(a)) => r.action == a && r.index.is_none(),
            _ => false,
        };
        if same {
            return;
        }
    }
    st(|s| s.note = None);
    let Some((_, seg)) = hit else {
        refresh();
        return;
    };
    let id = match kind {
        Some(Kind::Toggle(id, _)) => id,
        Some(Kind::Button(id, ..)) => id,
        Some(Kind::Seg(png, webp, _)) => {
            if seg == 0 {
                png
            } else {
                webp
            }
        }
        Some(Kind::Chip(a, i, _)) => {
            rec_begin(a, Some(i));
            return;
        }
        Some(Kind::Add(a)) => {
            rec_begin(a, None);
            return;
        }
        Some(Kind::Remove(a, i)) => {
            remove_key(a, i);
            refresh();
            return;
        }
        Some(Kind::ResetKeys(_)) => {
            reset_keys();
            refresh();
            return;
        }
        None => return,
    };
    crate::menu_action(id);
    refresh();
}

pub fn refresh() {
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            let _ = InvalidateRect(h, None, FALSE);
        }
    }
}

pub fn show() {
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    unsafe {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let inst = GetModuleHandleW(None).unwrap_or_default();
        ONCE.call_once(|| {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(proc_),
                hInstance: inst.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hIcon: crate::icon::make(32),
                hIconSm: crate::icon::make(16),
                lpszClassName: w!("RefractScreenerSettings"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
        });
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let k = dx as f32 / 96.0;
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = rect(0, 0, (W * k) as i32, (H * k) as i32);
        let _ = AdjustWindowRectExForDpi(&mut r, style, FALSE, WINDOW_EX_STYLE(0), dx);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let wa = mi.rcWork;
        let x = wa.left + ((wa.right - wa.left - w) / 2).max(0);
        let y = wa.top + ((wa.bottom - wa.top - h) / 2).max(0);
        st(|s| s.k = k);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("RefractScreenerSettings"),
            w!("Refract-screener Settings"),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            inst,
            None,
        ) else {
            return;
        };
        dark_frame(hwnd);
        st(|s| s.hwnd = Some(hwnd));
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h, |dc, rc| draw(dc, rc));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEMOVE => {
            let (x, y) = crate::win::lparam_xy(lp);
            let now = hit(x, y);
            if st(|s| std::mem::replace(&mut s.hover, now)) != now {
                refresh();
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
            st(|s| s.hover = None);
            refresh();
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            set_cursor(st(|s| s.hover.is_some()));
            LRESULT(1)
        }
        WM_LBUTTONUP => {
            let (x, y) = crate::win::lparam_xy(lp);
            click(x, y);
            LRESULT(0)
        }
        WM_KEYDOWN if wp.0 as u16 == VK_ESCAPE.0 => {
            if st(|s| s.rec.is_some()) {
                rec_end();
            } else {
                let _ = DestroyWindow(h);
            }
            LRESULT(0)
        }
        hotkey::WM_APP_KEY_RECORDED => {
            on_recorded(Combo::unpack(wp.0));
            LRESULT(0)
        }
        hotkey::WM_APP_KEY_MODS => {
            st(|s| {
                if let Some(r) = s.rec.as_mut() {
                    r.mods = Combo::unpack(wp.0);
                    r.since = std::time::Instant::now();
                }
            });
            refresh();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_REC => {
            let stale = st(|s| {
                s.rec
                    .as_ref()
                    .is_some_and(|r| r.since.elapsed().as_secs() >= REC_TIMEOUT_S)
            });
            if stale {
                rec_end();
            }
            LRESULT(0)
        }
        WM_ACTIVATE => {
            if wp.0 & 0xFFFF == 0 {
                // WA_INACTIVE: another window took the keyboard.
                rec_end();
            }
            // Settings may have been changed elsewhere (tray, config.json).
            refresh();
            LRESULT(0)
        }
        WM_DPICHANGED => {
            st(|s| s.k = (wp.0 & 0xFFFF) as f32 / 96.0);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(h, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            refresh();
            LRESULT(0)
        }
        WM_DESTROY => {
            rec_end();
            st(|s| {
                s.hwnd = None;
                s.hover = None;
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// Pictures of the window without showing it (--ui-shots): as it opens, and
/// while a shortcut is recorded. `k` is the display scale.
pub fn render_png(dir: &std::path::Path, k: f32) -> Result<(), String> {
    let (w, h) = ((W * k) as i32, (H * k) as i32);
    st(|s| s.k = k);
    let shots = [
        ("settings.png", None),
        (
            "settings-recording.png",
            Some(Rec {
                action: Action::Scroll,
                index: None,
                mods: Combo::unpack(Combo { ctrl: true, alt: false, shift: true, win: false, vk: 0 }.pack()),
                since: std::time::Instant::now(),
            }),
        ),
    ];
    for (name, rec) in shots {
        st(|s| s.rec = rec);
        let px = paint_offscreen(w, h, |dc, rc| unsafe { draw(dc, rc) })
            .ok_or("could not draw the settings window")?;
        let img = crate::encode::Image { w: w as u32, h: h as u32, bgra: px };
        let data = crate::encode::png(&img, 0)?;
        std::fs::write(dir.join(name), data).map_err(|e| e.to_string())?;
    }
    st(|s| s.rec = None);
    Ok(())
}
