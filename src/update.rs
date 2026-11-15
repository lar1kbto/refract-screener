//! Auto-update. On start, every hour and after the PC wakes up, the app quietly asks the update server (`update_url`)
//! for the current version. A newer build is downloaded in the background while a
//! small card in the corner shows the progress; when it is verified (size + SHA-256)
//! the card and a notification offer "Restart now". Restarting swaps the EXE
//! (a running EXE can be renamed, not overwritten) and starts the new one.

use crate::gdi::{fill, rgb, round, text};
use crate::win::*;
use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::Mutex;
use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const WM_APP_UPD_FOUND: u32 = WM_APP + 40; // lparam = Box<String> version
pub const WM_APP_UPD_PROGRESS: u32 = WM_APP + 41;
pub const WM_APP_UPD_READY: u32 = WM_APP + 42; // lparam = Box<(String, PathBuf)>
pub const WM_APP_UPD_NONE: u32 = WM_APP + 43; // lparam = Box<Option<String>> error
pub const TIMER_CHECK: usize = 31;
pub const CHECK_EVERY_MS: u32 = 3600 * 1000;

pub fn current() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn parse(v: &str) -> Vec<u32> {
    v.trim()
        .trim_start_matches('v')
        .split(['.', '-', '+'])
        .take(4)
        .map(|p| p.parse().unwrap_or(0))
        .collect()
}

pub fn newer(remote: &str, local: &str) -> bool {
    let (mut a, mut b) = (parse(remote), parse(local));
    let n = a.len().max(b.len());
    a.resize(n, 0);
    b.resize(n, 0);
    a > b
}

#[derive(Clone, Default, PartialEq)]
enum Phase {
    #[default]
    Idle,
    Checking,
    Downloading,
    Ready,
    Failed,
}

#[derive(Default)]
struct State {
    phase: Phase,
    version: String,
    staged: Option<PathBuf>,
    error: String,
    manual: bool,
    window: Option<HWND>,
    hover: i32,
    dpi: f32,
}

thread_local! {
    static STATE: RefCell<State> = RefCell::new(State::default());
}
static PROGRESS: Mutex<(u64, u64)> = Mutex::new((0, 0));

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    STATE.with(|s| f(&mut s.borrow_mut()))
}

pub fn ready_version() -> Option<String> {
    st(|s| (s.phase == Phase::Ready).then(|| s.version.clone()))
}

fn staging_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Refract-screener")
        .join("update")
}

/// Removes what a previous update left behind.
pub fn cleanup() {
    let old = exe_path().with_extension("exe.old");
    for _ in 0..20 {
        if !old.exists() || std::fs::remove_file(&old).is_ok() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(150));
    }
    if let Ok(rd) = std::fs::read_dir(staging_dir()) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            let keep = parse(
                n.trim_start_matches("refract-screener-")
                    .trim_end_matches(".exe"),
            );
            if !newer(
                &keep
                    .iter()
                    .map(|x| x.to_string())
                    .collect::<Vec<_>>()
                    .join("."),
                current(),
            ) {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Starts a check in the background. `manual` = asked from the tray menu.
pub fn check(main: HWND, base: String, manual: bool) {
    let busy = st(|s| {
        if matches!(s.phase, Phase::Checking | Phase::Downloading) {
            return true;
        }
        if s.phase == Phase::Ready {
            if manual {
                show_window(main);
            }
            return true;
        }
        s.phase = Phase::Checking;
        s.manual = manual;
        false
    });
    if busy {
        return;
    }
    let h = hwnd_to(main);
    std::thread::spawn(move || {
        let post = |msg: u32, l: isize| unsafe {
            let _ = PostMessageW(hwnd_from(h), msg, WPARAM(0), LPARAM(l));
        };
        let fail = |e: Option<String>| post(WM_APP_UPD_NONE, Box::into_raw(Box::new(e)) as isize);
        let manifest =
            match crate::net::http_get(&format!("{base}/api/screens/manifest"), |_, _| {}) {
                Ok(m) => m,
                Err(e) => return fail(Some(e)),
            };
        let Ok(v) = serde_json::from_slice::<serde_json::Value>(&manifest) else {
            return fail(Some("unexpected answer from the cloud".into()));
        };
        let version = v["version"].as_str().unwrap_or("").to_string();
        let sha = v["sha256"].as_str().unwrap_or("").to_ascii_lowercase();
        let size = v["size"].as_u64().unwrap_or(0);
        let url = v["url"].as_str().unwrap_or("").to_string();
        if version.is_empty() || !newer(&version, current()) {
            return fail(None);
        }
        if sha.len() != 64 || size == 0 || !url.starts_with('/') {
            return fail(Some("the update is not available right now".into()));
        }
        let dir = staging_dir();
        let _ = std::fs::create_dir_all(&dir);
        let target = dir.join(format!("refract-screener-{version}.exe"));
        // Already downloaded earlier?
        if let Ok(d) = std::fs::read(&target) {
            if d.len() as u64 == size && crate::net::sha256(&d) == sha {
                post(
                    WM_APP_UPD_READY,
                    Box::into_raw(Box::new((version, target))) as isize,
                );
                return;
            }
        }
        *PROGRESS.lock().unwrap() = (0, size);
        post(
            WM_APP_UPD_FOUND,
            Box::into_raw(Box::new(version.clone())) as isize,
        );
        let mut last = std::time::Instant::now();
        let data = crate::net::http_get(&format!("{base}{url}"), |done, total| {
            *PROGRESS.lock().unwrap() = (done, if total > 0 { total } else { size });
            if last.elapsed().as_millis() > 80 {
                last = std::time::Instant::now();
                post(WM_APP_UPD_PROGRESS, 0);
            }
        });
        let data = match data {
            Ok(d) => d,
            Err(e) => return fail(Some(e)),
        };
        if data.len() as u64 != size || crate::net::sha256(&data) != sha {
            return fail(Some("the download was damaged (checksum mismatch)".into()));
        }
        let tmp = target.with_extension("part");
        if std::fs::write(&tmp, &data).is_err() || std::fs::rename(&tmp, &target).is_err() {
            return fail(Some("cannot write the update to disk".into()));
        }
        post(
            WM_APP_UPD_READY,
            Box::into_raw(Box::new((version, target))) as isize,
        );
    });
}

/// Handles the worker's messages on the main thread. Returns true when handled.
pub fn on_message(main: HWND, msg: u32, lp: LPARAM) -> bool {
    match msg {
        WM_APP_UPD_FOUND => {
            let v = unsafe { *Box::from_raw(lp.0 as *mut String) };
            log(&format!("update {v} found, downloading"));
            st(|s| {
                s.phase = Phase::Downloading;
                s.version = v;
            });
            show_window(main);
        }
        WM_APP_UPD_PROGRESS => repaint(),
        WM_APP_UPD_READY => {
            let (v, path) = unsafe { *Box::from_raw(lp.0 as *mut (String, PathBuf)) };
            log(&format!("update {v} ready at {}", path.display()));
            st(|s| {
                s.phase = Phase::Ready;
                s.version = v.clone();
                s.staged = Some(path);
            });
            show_window(main);
            let shown = crate::toast::show(&crate::toast::Toast {
                title: &format!("Refract-screener {v} is ready"),
                body: "Restart the app to finish the update.",
                image: None,
                open: None,
                folder: None,
                launch_uri: Some("refract-screener:show-update"),
                button: Some(("Restart now", "refract-screener:update")),
            });
            let _ = shown;
        }
        WM_APP_UPD_NONE => {
            let err = unsafe { *Box::from_raw(lp.0 as *mut Option<String>) };
            let manual = st(|s| {
                s.phase = if err.is_some() {
                    Phase::Failed
                } else {
                    Phase::Idle
                };
                s.error = err.clone().unwrap_or_default();
                s.manual
            });
            match &err {
                Some(e) => log(&format!("update check failed: {e}")),
                None => log("no update"),
            }
            if manual {
                crate::notify_public(
                    if err.is_some() {
                        "Could not check for updates"
                    } else {
                        "Refract-screener is up to date"
                    },
                    &err.unwrap_or_else(|| format!("Version {}", current())),
                );
            }
            if st(|s| s.window.is_some()) {
                repaint();
            }
        }
        _ => return false,
    }
    true
}

/// Swaps the EXE for the downloaded one and restarts. Returns false if it could not.
pub fn apply() -> bool {
    let Some(staged) = st(|s| s.staged.clone()) else {
        return false;
    };
    let exe = exe_path();
    let old = exe.with_extension("exe.old");
    let _ = std::fs::remove_file(&old);
    if std::fs::rename(&exe, &old).is_err() {
        log("update: cannot rename the running exe");
        return false;
    }
    if let Err(e) = std::fs::copy(&staged, &exe) {
        log(&format!("update: copy failed: {e}"));
        let _ = std::fs::rename(&old, &exe);
        return false;
    }
    log(&format!(
        "update: restarting into {}",
        st(|s| s.version.clone())
    ));
    crate::install::launch_after_exit(&exe, &["--background", "--updated"]);
    true
}

// ------------------------------------------------------------------ progress card

fn repaint() {
    if let Some(h) = st(|s| s.window) {
        unsafe {
            let _ = InvalidateRect(h, None, FALSE);
        }
    }
}

pub fn show_window(main: HWND) {
    if let Some(h) = st(|s| s.window) {
        repaint();
        unsafe {
            let _ = ShowWindow(h, SW_SHOWNOACTIVATE);
        }
        return;
    }
    let _ = main;
    unsafe {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let inst = GetModuleHandleW(None).unwrap_or_default();
        ONCE.call_once(|| {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                lpfnWndProc: Some(card_proc),
                hInstance: inst.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                lpszClassName: w!("RefractScreenerUpdate"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
        });
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let k = dx as f32 / 96.0;
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let (w, h) = ((380.0 * k) as i32, (156.0 * k) as i32);
        let m = (16.0 * k) as i32;
        let x = mi.rcWork.right - w - m;
        let y = mi.rcWork.bottom - h - m;
        let Ok(hwnd) = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            w!("RefractScreenerUpdate"),
            w!("Refract-screener update"),
            WS_POPUP,
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
        // Rounded corners on Windows 11 (ignored on 10).
        let pref: i32 = 2; // DWMWCP_ROUND
        let _ = windows::Win32::Graphics::Dwm::DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const _ as *const _,
            4,
        );
        st(|s| {
            s.window = Some(hwnd);
            s.dpi = k;
        });
        let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
        SetTimer(hwnd, 1, 40, None);
    }
}

fn close_window() {
    if let Some(h) = st(|s| s.window.take()) {
        unsafe {
            let _ = DestroyWindow(h);
        }
    }
}

/// Button rectangles: 1 = Restart now, 2 = Later, 3 = close (x).
fn buttons(rc: &RECT, k: f32) -> [(i32, RECT); 3] {
    let px = |v: f32| (v * k) as i32;
    let y0 = rc.bottom - px(48.0);
    let restart = RECT {
        left: rc.right - px(146.0),
        top: y0,
        right: rc.right - px(18.0),
        bottom: y0 + px(32.0),
    };
    let later = RECT {
        left: restart.left - px(92.0),
        top: y0,
        right: restart.left - px(10.0),
        bottom: y0 + px(32.0),
    };
    let close = RECT {
        left: rc.right - px(36.0),
        top: px(10.0),
        right: rc.right - px(10.0),
        bottom: px(36.0),
    };
    [(1, restart), (2, later), (3, close)]
}

unsafe fn paint(hwnd: HWND) {
    crate::gdi::paint(hwnd, |dc, rc| draw(dc, rc));
}

unsafe fn draw(dc: HDC, rc: RECT) {
    let (phase, version, error, hover, k) = st(|s| {
        (
            s.phase.clone(),
            s.version.clone(),
            s.error.clone(),
            s.hover,
            s.dpi.max(1.0),
        )
    });
    let px = |v: f32| (v * k) as i32;
    let mint = rgb(0x6F, 0xF2, 0xB8);
    let ink = rgb(0xEE, 0xF5, 0xF1);
    let dim = rgb(0x8A, 0x9A, 0x92);

    fill(dc, rc, rgb(0x2A, 0x33, 0x2F));
    fill(
        dc,
        RECT {
            left: 1,
            top: 1,
            right: rc.right - 1,
            bottom: rc.bottom - 1,
        },
        rgb(0x0E, 0x13, 0x11),
    );
    fill(
        dc,
        RECT {
            left: 1,
            top: 1,
            right: rc.right - 1,
            bottom: 1 + px(2.0),
        },
        if phase == Phase::Failed {
            rgb(0xFF, 0x8A, 0x80)
        } else {
            mint
        },
    );

    crate::gdi::logo(dc, px(16.0), px(16.0), px(36.0));
    let mut r = RECT {
        left: px(62.0),
        top: px(15.0),
        right: rc.right - px(44.0),
        bottom: px(39.0),
    };
    text(
        dc,
        "Refract-screener update",
        &mut r,
        15.0 * k,
        600,
        ink,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let mut r = RECT {
        left: px(62.0),
        top: px(37.0),
        right: rc.right - px(18.0),
        bottom: px(57.0),
    };
    text(
        dc,
        &format!("{}  \u{2192}  {}", current(), version),
        &mut r,
        13.0 * k,
        400,
        dim,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );

    let (done, total) = *PROGRESS.lock().unwrap();
    let bar = RECT {
        left: px(18.0),
        top: px(72.0),
        right: rc.right - px(18.0),
        bottom: px(76.0),
    };
    let line = match phase {
        Phase::Downloading | Phase::Checking => {
            fill(dc, bar, rgb(0x1B, 0x26, 0x21));
            let f = if total > 0 {
                done as f64 / total as f64
            } else {
                0.0
            };
            fill(
                dc,
                RECT {
                    left: bar.left,
                    top: bar.top,
                    right: bar.left + ((bar.right - bar.left) as f64 * f.min(1.0)) as i32,
                    bottom: bar.bottom,
                },
                mint,
            );
            format!(
                "Downloading  {:.1} of {:.1} MB",
                done as f64 / 1048576.0,
                total as f64 / 1048576.0
            )
        }
        Phase::Ready => {
            fill(dc, bar, mint);
            "Downloaded and verified. Restart to finish.".to_string()
        }
        Phase::Failed => format!("Update failed: {error}"),
        Phase::Idle => String::new(),
    };
    let mut r = RECT {
        left: px(18.0),
        top: px(84.0),
        right: rc.right - px(18.0),
        bottom: px(104.0),
    };
    text(
        dc,
        &line,
        &mut r,
        12.5 * k,
        400,
        if phase == Phase::Failed {
            rgb(0xFF, 0x8A, 0x80)
        } else {
            dim
        },
        DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
    );

    for (id, b) in buttons(&rc, k) {
        match id {
            1 if phase == Phase::Ready => {
                round(
                    dc,
                    b,
                    px(10.0),
                    if hover == 1 {
                        rgb(0x8C, 0xF7, 0xC8)
                    } else {
                        mint
                    },
                );
                let mut t = b;
                text(
                    dc,
                    "Restart now",
                    &mut t,
                    13.5 * k,
                    600,
                    rgb(0x0A, 0x1A, 0x12),
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                );
            }
            2 if phase == Phase::Ready => {
                round(
                    dc,
                    b,
                    px(10.0),
                    if hover == 2 {
                        rgb(0x24, 0x30, 0x2B)
                    } else {
                        rgb(0x18, 0x21, 0x1D)
                    },
                );
                let mut t = b;
                text(
                    dc,
                    "Later",
                    &mut t,
                    13.5 * k,
                    500,
                    ink,
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                );
            }
            3 => {
                if hover == 3 {
                    round(dc, b, px(8.0), rgb(0x1E, 0x28, 0x24));
                }
                let mut t = b;
                text(
                    dc,
                    "\u{2715}",
                    &mut t,
                    12.0 * k,
                    400,
                    dim,
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                );
            }
            _ => {}
        }
    }
}

fn hit(hwnd: HWND, lp: LPARAM) -> i32 {
    let (x, y) = lparam_xy(lp);
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let k = st(|s| s.dpi.max(1.0));
    let ready = st(|s| s.phase == Phase::Ready);
    for (id, b) in buttons(&rc, k) {
        if (id == 3 || ready) && x >= b.left && x < b.right && y >= b.top && y < b.bottom {
            return id;
        }
    }
    0
}

unsafe extern "system" fn card_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h);
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_MOUSEACTIVATE => LRESULT(MA_NOACTIVATE as isize),
        WM_TIMER => {
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let id = hit(h, lp);
            if st(|s| std::mem::replace(&mut s.hover, id)) != id {
                let _ = InvalidateRect(h, None, FALSE);
            }
            let mut tme = windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<windows::Win32::UI::Input::KeyboardAndMouse::TRACKMOUSEEVENT>() as u32,
                dwFlags: windows::Win32::UI::Input::KeyboardAndMouse::TME_LEAVE,
                hwndTrack: h,
                dwHoverTime: 0,
            };
            let _ = windows::Win32::UI::Input::KeyboardAndMouse::TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            st(|s| s.hover = 0);
            let _ = InvalidateRect(h, None, FALSE);
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            match hit(h, lp) {
                1 => {
                    close_window();
                    crate::restart_for_update();
                }
                2 | 3 => close_window(),
                _ => {}
            }
            LRESULT(0)
        }
        WM_SETCURSOR => {
            let id = st(|s| s.hover);
            if let Ok(c) = LoadCursorW(None, if id != 0 { IDC_HAND } else { IDC_ARROW }) {
                SetCursor(c);
            }
            LRESULT(1)
        }
        WM_DESTROY => {
            st(|s| {
                if s.window == Some(h) {
                    s.window = None;
                }
            });
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// refract-screener: links (used by notification buttons).
pub fn register_protocol() {
    use windows::Win32::System::Registry::*;
    let exe = exe_path();
    let set = |path: &str, name: Option<&str>, val: &str| unsafe {
        let p = W::new(path);
        let mut k = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            p.p(),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut k,
            None,
        ) == ERROR_SUCCESS
        {
            let n = name.map(W::new);
            let v = wide(val);
            let b = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
            let _ = RegSetValueExW(
                k,
                n.as_ref().map(|n| n.p()).unwrap_or(PCWSTR::null()),
                0,
                REG_SZ,
                Some(b),
            );
            let _ = RegCloseKey(k);
        }
    };
    set(
        r"Software\Classes\refract-screener",
        None,
        "URL:Refract-screener",
    );
    set(
        r"Software\Classes\refract-screener",
        Some("URL Protocol"),
        "",
    );
    set(
        r"Software\Classes\refract-screener\shell\open\command",
        None,
        &format!("\"{}\" --activate \"%1\"", exe.display()),
    );
}
