//! RefractScreenerSetup.exe: a small online installer. It downloads the current
//! Refract-screener build from the update server (REFRACT_UPDATE_URL at build time), checks its size and
//! SHA-256 against the published manifest and runs its per-user install.
//!
//!   RefractScreenerSetup.exe [--from https://your.server]

#![windows_subsystem = "windows"]

#[path = "../net.rs"]
mod net;

use net::{http_get, sha256};
use std::sync::{Arc, Mutex};
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Where the app is downloaded from; set REFRACT_UPDATE_URL when building.
const DEFAULT_BASE: Option<&str> = option_env!("REFRACT_UPDATE_URL");
const WM_APP_UPDATE: u32 = WM_APP + 1;
const WM_APP_DONE: u32 = WM_APP + 2;

#[derive(Clone, Default)]
struct Status {
    stage: String,
    detail: String,
    done: u64,
    total: u64,
    error: Option<String>,
    finished: bool,
}

struct Ui {
    status: Arc<Mutex<Status>>,
    base: String,
    tick: u32,
    dpi: f32,
}

static mut UI: Option<Ui> = None;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn ui() -> &'static mut Ui {
    #[allow(static_mut_refs)]
    unsafe {
        UI.as_mut().unwrap()
    }
}

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }
    let args: Vec<String> = std::env::args().collect();
    let base = args
        .iter()
        .position(|a| a == "--from")
        .and_then(|i| args.get(i + 1))
        .cloned()
        .or_else(|| DEFAULT_BASE.map(String::from))
        .unwrap_or_default()
        .trim_end_matches('/')
        .to_string();
    let status = Arc::new(Mutex::new(Status {
        stage: "Connecting to the update server...".into(),
        ..Default::default()
    }));
    unsafe {
        UI = Some(Ui {
            status: status.clone(),
            base,
            tick: 0,
            dpi: 1.0,
        });
    }

    let hwnd = match create_window() {
        Some(h) => h,
        None => return,
    };
    start_worker(hwnd);
    unsafe {
        SetTimer(hwnd, 1, 33, None);
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn start_worker(hwnd: HWND) {
    let st = ui().status.clone();
    let base = ui().base.clone();
    let h = hwnd.0 as isize;
    {
        let mut s = st.lock().unwrap();
        *s = Status {
            stage: "Connecting to the update server...".into(),
            ..Default::default()
        };
    }
    std::thread::spawn(move || {
        let r = install(&base, &st, h);
        {
            let mut s = st.lock().unwrap();
            match r {
                Ok(v) => {
                    s.finished = true;
                    s.stage = format!("Refract-screener {v} is installed");
                    s.detail = "Press PrintScreen or Win+Shift+S to take a screenshot.".into();
                }
                Err(e) => s.error = Some(e),
            }
        }
        unsafe {
            let _ = PostMessageW(HWND(h as *mut _), WM_APP_DONE, WPARAM(0), LPARAM(0));
        }
    });
}

fn set(st: &Arc<Mutex<Status>>, h: isize, f: impl FnOnce(&mut Status)) {
    f(&mut st.lock().unwrap());
    unsafe {
        let _ = PostMessageW(HWND(h as *mut _), WM_APP_UPDATE, WPARAM(0), LPARAM(0));
    }
}

fn install(base: &str, st: &Arc<Mutex<Status>>, h: isize) -> Result<String, String> {
    if base.is_empty() {
        return Err("This installer was built without an update server. Build it with REFRACT_UPDATE_URL set, or run it with --from https://your.server".into());
    }
    let manifest = http_get(&format!("{base}/api/screens/manifest"), |_, _| {})?;
    let v: serde_json::Value = serde_json::from_slice(&manifest)
        .map_err(|_| "The update server sent an unexpected answer.".to_string())?;
    let version = v["version"].as_str().unwrap_or("?").to_string();
    let sha = v["sha256"].as_str().unwrap_or("").to_ascii_lowercase();
    let size = v["size"].as_u64().unwrap_or(0);
    let url = v["url"].as_str().unwrap_or("");
    if sha.len() != 64 || size == 0 || !url.starts_with('/') {
        return Err("Refract-screener is not available for download right now.".into());
    }
    set(st, h, |s| {
        s.stage = format!("Downloading Refract-screener {version}");
        s.total = size;
    });
    let data = http_get(&format!("{base}{url}"), |done, total| {
        set(st, h, |s| {
            s.done = done;
            if total > 0 {
                s.total = total;
            }
        })
    })?;
    set(st, h, |s| s.stage = "Checking the download".into());
    if data.len() as u64 != size || sha256(&data) != sha {
        return Err("The downloaded file is damaged (checksum mismatch). Try again.".into());
    }
    let dir = std::env::temp_dir().join(format!("RefractScreenerSetup-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot write to the temp folder: {e}"))?;
    let exe = dir.join("refract-screener.exe");
    std::fs::write(&exe, &data).map_err(|e| format!("Cannot write to the temp folder: {e}"))?;
    set(st, h, |s| s.stage = "Installing".into());
    let status = std::process::Command::new(&exe)
        .args(["--install", "--from-setup"])
        .status()
        .map_err(|e| format!("Cannot start the installer: {e}"))?;
    let _ = std::fs::remove_dir_all(&dir);
    if !status.success() {
        return Err("Installation did not finish. See the message from Refract-screener.".into());
    }
    Ok(version)
}

// ------------------------------------------------------------------ window

fn create_window() -> Option<HWND> {
    unsafe {
        let inst = GetModuleHandleW(None).ok()?;
        let class = w!("RefractScreenerSetup");
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: inst.into(),
            hCursor: LoadCursorW(None, IDC_ARROW).ok()?,
            hIcon: LoadIconW(inst, PCWSTR(1 as _)).unwrap_or_default(),
            hIconSm: LoadImageW(inst, PCWSTR(1 as _), IMAGE_ICON, 16, 16, LR_DEFAULTCOLOR)
                .map(|h| HICON(h.0))
                .unwrap_or_default(),
            lpszClassName: class,
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let k = dx as f32 / 96.0;
        ui().dpi = k;
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let mut r = RECT {
            left: 0,
            top: 0,
            right: (480.0 * k) as i32,
            bottom: (210.0 * k) as i32,
        };
        let _ = AdjustWindowRectExForDpi(&mut r, style, FALSE, WINDOW_EX_STYLE(0), dx);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let wa = mi.rcWork;
        let x = wa.left + (wa.right - wa.left - w) / 2;
        let y = wa.top + (wa.bottom - wa.top - h) / 2;
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class,
            w!("Refract-screener Setup"),
            style,
            x,
            y,
            w,
            h,
            None,
            None,
            inst,
            None,
        )
        .ok()?;
        // Dark title bar (Windows 10 20H1+ / 11).
        let dark: BOOL = TRUE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const _,
            4,
        );
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        Some(hwnd)
    }
}

fn rgb(r: u8, g: u8, b: u8) -> COLORREF {
    COLORREF(r as u32 | (g as u32) << 8 | (b as u32) << 16)
}

unsafe fn text(
    dc: HDC,
    s: &str,
    r: &mut RECT,
    size: f32,
    weight: i32,
    color: COLORREF,
    flags: DRAW_TEXT_FORMAT,
) {
    let font = CreateFontW(
        -(size as i32),
        0,
        0,
        0,
        weight,
        0,
        0,
        0,
        DEFAULT_CHARSET.0 as u32,
        OUT_DEFAULT_PRECIS.0 as u32,
        CLIP_DEFAULT_PRECIS.0 as u32,
        CLEARTYPE_QUALITY.0 as u32,
        (DEFAULT_PITCH.0 | FF_SWISS.0) as u32,
        w!("Segoe UI"),
    );
    let old = SelectObject(dc, font);
    SetTextColor(dc, color);
    SetBkMode(dc, TRANSPARENT);
    if s.is_empty() {
        return;
    }
    // A null-terminated buffer: DrawTextW must never see an empty / dangling slice.
    let mut w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
    let n = w.len() - 1;
    DrawTextW(dc, &mut w[..n], r, flags);
    SelectObject(dc, old);
    let _ = DeleteObject(font);
}

unsafe fn fill(dc: HDC, r: RECT, c: COLORREF) {
    let b = CreateSolidBrush(c);
    FillRect(dc, &r, b);
    let _ = DeleteObject(b);
}

unsafe fn paint(hwnd: HWND) {
    let mut ps = PAINTSTRUCT::default();
    let hdc = BeginPaint(hwnd, &mut ps);
    let mut rc = RECT::default();
    let _ = GetClientRect(hwnd, &mut rc);
    // Double buffer.
    let dc = CreateCompatibleDC(hdc);
    let bmp = CreateCompatibleBitmap(hdc, rc.right, rc.bottom);
    let old = SelectObject(dc, bmp);
    let k = ui().dpi;
    let px = |v: f32| (v * k) as i32;
    let s = ui().status.lock().unwrap().clone();
    let mint = rgb(0x6F, 0xF2, 0xB8);

    fill(dc, rc, rgb(0x0A, 0x0F, 0x0D));
    // Glow line at the top.
    fill(
        dc,
        RECT {
            left: 0,
            top: 0,
            right: rc.right,
            bottom: px(2.0),
        },
        if s.error.is_some() {
            rgb(0xFF, 0x8A, 0x80)
        } else {
            mint
        },
    );

    // The Refract logo from the EXE's icon resource.
    let ms = px(48.0);
    if let Ok(h) = LoadImageW(
        GetModuleHandleW(None).unwrap_or_default(),
        PCWSTR(1 as _),
        IMAGE_ICON,
        ms,
        ms,
        LR_DEFAULTCOLOR,
    ) {
        let icon = HICON(h.0);
        let _ = DrawIconEx(dc, px(26.0), px(26.0), icon, ms, ms, 0, None, DI_NORMAL);
        let _ = DestroyIcon(icon);
    }
    let tx = px(88.0);
    let mut r = RECT {
        left: tx,
        top: px(28.0),
        right: rc.right - px(24.0),
        bottom: px(56.0),
    };
    text(
        dc,
        "Refract-screener",
        &mut r,
        21.0 * k,
        600,
        rgb(0xEE, 0xF5, 0xF1),
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let mut r = RECT {
        left: tx,
        top: px(56.0),
        right: rc.right - px(24.0),
        bottom: px(78.0),
    };
    text(
        dc,
        "Screenshots, annotations, text recognition",
        &mut r,
        13.5 * k,
        400,
        rgb(0x8A, 0x9A, 0x92),
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );

    let (stage, detail, color) = match &s.error {
        Some(e) => (
            "Setup could not finish".to_string(),
            e.clone(),
            rgb(0xFF, 0x8A, 0x80),
        ),
        None => {
            let dots = if s.finished {
                String::new()
            } else {
                ".".repeat((ui().tick / 12 % 4) as usize)
            };
            let detail = if s.finished {
                s.detail.clone()
            } else if s.total > 0 && s.done > 0 {
                format!(
                    "{:.1} of {:.1} MB",
                    s.done as f64 / 1048576.0,
                    s.total as f64 / 1048576.0
                )
            } else {
                s.detail.clone()
            };
            (
                format!("{}{}", s.stage.trim_end_matches("..."), dots),
                detail,
                rgb(0xEE, 0xF5, 0xF1),
            )
        }
    };
    let mut r = RECT {
        left: px(28.0),
        top: px(104.0),
        right: rc.right - px(28.0),
        bottom: px(126.0),
    };
    text(
        dc,
        &stage,
        &mut r,
        14.5 * k,
        600,
        color,
        DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
    );

    // Progress bar (indeterminate shimmer until the size is known).
    let bar = RECT {
        left: px(28.0),
        top: px(136.0),
        right: rc.right - px(28.0),
        bottom: px(140.0),
    };
    fill(dc, bar, rgb(0x1B, 0x26, 0x21));
    let bw = bar.right - bar.left;
    if s.error.is_none() {
        if s.finished || (s.total > 0 && s.done > 0) {
            let f = if s.finished {
                1.0
            } else {
                s.done as f64 / s.total.max(1) as f64
            };
            fill(
                dc,
                RECT {
                    left: bar.left,
                    top: bar.top,
                    right: bar.left + (bw as f64 * f.min(1.0)) as i32,
                    bottom: bar.bottom,
                },
                mint,
            );
        } else {
            let seg = bw / 4;
            let p = (ui().tick as i32 * px(6.0)) % (bw + seg) - seg;
            fill(
                dc,
                RECT {
                    left: (bar.left + p).max(bar.left),
                    top: bar.top,
                    right: (bar.left + p + seg).min(bar.right),
                    bottom: bar.bottom,
                },
                mint,
            );
        }
    }
    let mut r = RECT {
        left: px(28.0),
        top: px(150.0),
        right: rc.right - px(28.0),
        bottom: rc.bottom - px(10.0),
    };
    text(
        dc,
        &detail,
        &mut r,
        13.0 * k,
        400,
        rgb(0x8A, 0x9A, 0x92),
        DT_LEFT | DT_TOP | DT_WORDBREAK,
    );

    let _ = BitBlt(hdc, 0, 0, rc.right, rc.bottom, dc, 0, 0, SRCCOPY);
    SelectObject(dc, old);
    let _ = DeleteObject(bmp);
    let _ = DeleteDC(dc);
    let _ = EndPaint(hwnd, &ps);
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_TIMER => {
            if wp.0 == 2 {
                let _ = DestroyWindow(h);
                return LRESULT(0);
            }
            ui().tick = ui().tick.wrapping_add(1);
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_APP_UPDATE => {
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_APP_DONE => {
            let s = ui().status.lock().unwrap().clone();
            let _ = InvalidateRect(h, None, FALSE);
            if let Some(e) = s.error {
                let _ = KillTimer(h, 1);
                let t = wide(&format!("{e}\n\nTry again?"));
                let c = wide("Refract-screener Setup");
                let r = MessageBoxW(
                    h,
                    PCWSTR(t.as_ptr()),
                    PCWSTR(c.as_ptr()),
                    MB_RETRYCANCEL | MB_ICONWARNING,
                );
                if r == IDRETRY {
                    SetTimer(h, 1, 33, None);
                    start_worker(h);
                } else {
                    let _ = DestroyWindow(h);
                }
            } else {
                let _ = KillTimer(h, 1);
                SetTimer(h, 2, 3500, None);
            }
            LRESULT(0)
        }
        WM_DPICHANGED => {
            ui().dpi = (wp.0 & 0xFFFF) as f32 / 96.0;
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(
                h,
                None,
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            LRESULT(0)
        }
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}
