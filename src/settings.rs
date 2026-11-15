//! The settings window: screenshot folder (change / open / reset), format, what
//! happens after a capture, capture options, start with Windows, Snipping Tool
//! replacement and updates. Changes apply immediately and are saved to config.json.

use crate::gdi::*;
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
}

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
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State::default());
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    S.with(|s| f(&mut s.borrow_mut()))
}

const W: f32 = 760.0;
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
    let (lx, lw) = (28.0, 344.0);
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

    // Right column: capture + system.
    let (rx, rw) = (404.0, 328.0);
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
            r(rx, 452.0, 196.0, 34.0),
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
        r(rx + 208.0, 452.0, 120.0, 34.0),
        Kind::Button(EDIT_CFG, "config.json", false, true),
    );
    (items, texts, lines)
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
    let mut t = rect(px(404.0), px(30.0), rc.right - px(28.0), px(74.0));
    text(
        dc,
        "PrintScreen  \u{00B7}  Win+Shift+S",
        &mut t,
        12.5 * k,
        500,
        FAINT,
        DT_SINGLELINE | DT_RIGHT | DT_TOP,
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
    let pb = rect(px(28.0), px(126.0), px(372.0), px(166.0));
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

    for (i, it) in items.iter().enumerate() {
        let r = scaled(&it.r, k);
        let hov = hover.map(|h| h.0) == Some(i);
        match &it.kind {
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
}

fn hit(x: i32, y: i32) -> Option<(usize, i32)> {
    let k = st(|s| s.k.max(1.0));
    let (items, _, _) = layout(&crate::settings_snapshot());
    for (i, it) in items.iter().enumerate() {
        let r = scaled(&it.r, k);
        if inside(&r, x, y) {
            if let Kind::Button(_, _, _, false) = it.kind {
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
    let Some((i, seg)) = hit(x, y) else { return };
    let (items, _, _) = layout(&crate::settings_snapshot());
    let id = match items[i].kind {
        Kind::Toggle(id, _) => id,
        Kind::Button(id, ..) => id,
        Kind::Seg(png, webp, _) => {
            if seg == 0 {
                png
            } else {
                webp
            }
        }
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
            let _ = DestroyWindow(h);
            LRESULT(0)
        }
        WM_ACTIVATE => {
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
            st(|s| {
                s.hwnd = None;
                s.hover = None;
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}
