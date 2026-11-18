//! Key guard: a tiny background process (`refract-screener.exe --guard`) that keeps
//! PrintScreen and Win+Shift+S away from Snipping Tool for as long as Refract-screener
//! is installed and "Replace Snipping Tool" is on, even when the app itself was
//! closed from the tray. A press is forwarded to the running app, or starts it.
//!
//! Windows has no switch that turns Win+Shift+S off for Snipping Tool, so a
//! low-level keyboard hook that is always present is the only reliable way.

use crate::hotkey;
use crate::win::*;
use windows::Win32::Foundation::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const GUARD_CLASS: &str = "RefractScreenerGuard";
/// Main app -> guard: settings changed, reload hotkeys (or quit if replacing is off).
pub const WM_APP_GUARD_RELOAD: u32 = WM_APP + 60;
/// Main app -> guard: wparam 1 = let every key through (a shortcut is being
/// recorded in Settings), 0 = back to work. The pause ends by itself after a while.
pub const WM_APP_GUARD_PAUSE: u32 = WM_APP + 61;
const TIMER_KILL: usize = 1;
const TIMER_UNPAUSE: usize = 2;

fn load_combos() -> Option<Vec<(hotkey::Combo, hotkey::Action)>> {
    let cfg = crate::config::load().cfg;
    cfg.replace_snipping_tool.then(|| hotkey::combos(&cfg))
}

pub fn run() {
    let name = W::new("Local\\RefractScreenerGuard");
    let _mutex = unsafe { CreateMutexW(None, TRUE, name.p()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        return;
    }
    let Some(combos) = load_combos() else { return };
    unsafe {
        let class = W::new(GUARD_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(proc_),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            lpszClassName: class.p(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class.p(),
            class.p(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        ) else {
            return;
        };
        hotkey::set_combos(combos);
        hotkey::start(hwnd);
        kill_snipping();
        SetTimer(hwnd, TIMER_KILL, 1500, None);
        log("guard started");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        log("guard stopped");
    }
}

fn forward(action: hotkey::Action) {
    match find_window(MAIN_CLASS) {
        Some(main) => unsafe {
            let _ = AllowSetForegroundWindow(ASFW_ANY);
            let _ = PostMessageW(main, WM_APP_REMOTE, WPARAM(action as usize), LPARAM(0));
        },
        None => {
            // The app was closed: start it straight into the capture.
            let flag = match action {
                hotkey::Action::Region => "--capture",
                hotkey::Action::Fullscreen => "--fullscreen",
                hotkey::Action::Window => "--window",
                hotkey::Action::Scroll => "--scroll",
            };
            log(&format!(
                "guard: app not running, starting it for {action:?}"
            ));
            crate::install::launch(&exe_path(), &["--background", flag]);
        }
    }
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_APP_HOTKEY => {
            if let Some(a) = hotkey::Action::from(wp.0) {
                forward(a);
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_UNPAUSE => {
            let _ = KillTimer(h, TIMER_UNPAUSE);
            hotkey::set_paused(false);
            LRESULT(0)
        }
        WM_TIMER => {
            kill_snipping();
            LRESULT(0)
        }
        WM_APP_GUARD_PAUSE => {
            hotkey::set_paused(wp.0 != 0);
            if wp.0 != 0 {
                SetTimer(h, TIMER_UNPAUSE, 60_000, None);
            } else {
                let _ = KillTimer(h, TIMER_UNPAUSE);
            }
            LRESULT(0)
        }
        WM_APP_GUARD_RELOAD => {
            match load_combos() {
                Some(c) => hotkey::set_combos(c),
                None => PostQuitMessage(0),
            }
            LRESULT(0)
        }
        WM_CLOSE | WM_ENDSESSION => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

/// Starts the guard if it is not running yet (called by the app).
pub fn ensure() {
    if find_window(GUARD_CLASS).is_none() {
        crate::install::launch(&exe_path(), &["--guard"]);
    }
}

/// Stops the guard and waits for it (update, uninstall, "Replace Snipping Tool" off).
pub fn stop() {
    for _ in 0..30 {
        match find_window(GUARD_CLASS) {
            Some(h) => unsafe {
                let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
                std::thread::sleep(std::time::Duration::from_millis(60));
            },
            None => return,
        }
    }
}

/// Tells a running guard that the settings changed.
pub fn reload() {
    if let Some(h) = find_window(GUARD_CLASS) {
        unsafe {
            let _ = PostMessageW(h, WM_APP_GUARD_RELOAD, WPARAM(0), LPARAM(0));
        }
    }
}

/// Pauses a running guard while Settings records a shortcut (see WM_APP_GUARD_PAUSE).
pub fn pause(on: bool) {
    if let Some(h) = find_window(GUARD_CLASS) {
        unsafe {
            let _ = PostMessageW(h, WM_APP_GUARD_PAUSE, WPARAM(on as usize), LPARAM(0));
        }
    }
}

/// Windows' own screenshot tools: Snipping Tool (Windows 11 app and the classic
/// one), its capture overlay host, and Snip & Sketch on Windows 10.
const SNIPPING: [&str; 4] = [
    "snippingtool.exe",
    "screenclippinghost.exe",
    "screensketch.exe",
    "snipandsketch.exe",
];

/// Ends every running Windows screenshot tool process of this user.
pub fn kill_snipping() {
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    use windows::Win32::System::Threading::*;
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return;
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut ok = Process32FirstW(snap, &mut e).is_ok();
        while ok {
            let n = e
                .szExeFile
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(e.szExeFile.len());
            let name = String::from_utf16_lossy(&e.szExeFile[..n]).to_ascii_lowercase();
            if SNIPPING.contains(&name.as_str()) {
                if let Ok(h) = OpenProcess(PROCESS_TERMINATE, FALSE, e.th32ProcessID) {
                    if TerminateProcess(h, 0).is_ok() {
                        log(&format!("guard: closed {name} ({})", e.th32ProcessID));
                    }
                    let _ = CloseHandle(h);
                }
            }
            ok = Process32NextW(snap, &mut e).is_ok();
        }
        let _ = CloseHandle(snap);
    }
}
