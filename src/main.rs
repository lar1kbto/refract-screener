//! Refract-screener: screenshots for Windows 11 that replace Snipping Tool.
//!
//!   refract-screener.exe               start (offers to install on first run)
//!   refract-screener.exe --background  start silently in the tray (used by autostart)
//!   refract-screener.exe --capture     capture a region now
//!   refract-screener.exe --fullscreen  capture the monitor under the cursor now
//!   refract-screener.exe --install     install for this user, start with Windows
//!   refract-screener.exe --uninstall   remove (settings and screenshots are kept)
//!   refract-screener.exe --selftest D  capture, annotate, encode, verify losslessness

#![windows_subsystem = "windows"]

mod capture;
mod config;
mod effects;
mod encode;
mod export;
mod gdi;
mod geom;
mod guard;
mod hotkey;
mod icon;
mod install;
mod net;
mod ocr;
mod overlay;
mod paint;
mod settings;
mod shapes;
mod toast;
mod update;
mod win;

use config::*;
use std::cell::RefCell;
use std::path::PathBuf;
use win::*;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::CreateMutexW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

struct App {
    hwnd: HWND,
    cfg: Config,
    cfg_error: Option<String>,
    icon: HICON,
    big_icon: HICON,
    last_saved: Option<PathBuf>,
    taskbar_created: u32,
    asked_cfa: bool,
}

struct SaveNote {
    title: String,
    body: String,
    path: Option<PathBuf>,
    blocked: bool,
}

/// Windows Security > Ransomware protection > Controlled folder access blocks
/// unknown apps from writing to Pictures/Documents/Desktop. Adding the app to the
/// allowed list needs administrator rights, so Windows shows its own prompt.
fn offer_cfa() {
    let r = message_box(
        "Windows \"Controlled folder access\" (ransomware protection) blocked saving to the Pictures folder.\n\n\
         Allow Refract-screener to save there? Windows will ask for administrator permission.\n\n\
         Until then screenshots are saved to your user folder\\Refract Screenshots.",
        MB_YESNO | MB_ICONWARNING,
    );
    if r == IDYES {
        allow_cfa();
    }
}

fn allow_cfa() {
    let exe = exe_path().display().to_string().replace('\'', "''");
    let args = W::new(&format!(
        "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -Command \"Add-MpPreference -ControlledFolderAccessAllowedApplications '{exe}'\""
    ));
    let op = W::new("runas");
    let ps = W::new("powershell.exe");
    unsafe {
        ShellExecuteW(None, op.p(), ps.p(), args.p(), PCWSTR::null(), SW_HIDE);
    }
    log("asked Windows to allow the app in Controlled folder access");
}

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| a.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

const TIMER_DELAY: usize = 7;

fn main() {
    unsafe {
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
    }
    let args: Vec<String> = std::env::args().skip(1).collect();
    let has = |a: &str| args.iter().any(|x| x.eq_ignore_ascii_case(a));

    if has("--selftest") {
        let dir = args
            .iter()
            .skip_while(|a| !a.eq_ignore_ascii_case("--selftest"))
            .nth(1)
            .cloned()
            .unwrap_or_else(|| ".".into());
        std::process::exit(selftest(PathBuf::from(dir)));
    }
    if has("--demo-shots") {
        let rest: Vec<String> = args
            .iter()
            .skip_while(|a| !a.eq_ignore_ascii_case("--demo-shots"))
            .skip(1)
            .cloned()
            .collect();
        let (Some(dir), Some(bg)) = (rest.first(), rest.get(1)) else {
            std::process::exit(2)
        };
        std::process::exit(demo_shots(PathBuf::from(dir), PathBuf::from(bg)));
    }
    if has("--guard") {
        guard::run();
        return;
    }
    if has("--uninstall") {
        install::uninstall();
        if !has("--quiet") {
            message_box("Refract-screener was removed.\nPrintScreen opens Snipping Tool again. Your screenshots and settings were kept.", MB_ICONINFORMATION);
        }
        return;
    }
    if has("--install") {
        match install::install() {
            Ok(exe) => {
                install::launch(&exe, &["--welcome"]);
                std::process::exit(0);
            }
            Err(e) => {
                message_box(&e, MB_ICONERROR);
                std::process::exit(1);
            }
        }
    }

    if let Some(pid) = args
        .iter()
        .position(|a| a == "--wait-pid")
        .and_then(|i| args.get(i + 1))
        .and_then(|p| p.parse::<u32>().ok())
    {
        wait_for_exit(pid);
    }
    // refract-screener:... links from notification buttons.
    let link = args
        .iter()
        .position(|a| a == "--activate")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.trim_end_matches('/').to_ascii_lowercase());
    let link_action: Option<usize> = match link.as_deref() {
        Some("refract-screener:update") => Some(REMOTE_APPLY_UPDATE),
        Some("refract-screener:show-update") => Some(REMOTE_SHOW_UPDATE),
        Some("refract-screener:settings") => Some(REMOTE_SHOW_SETTINGS),
        Some("refract-screener:check-update") => Some(REMOTE_CHECK_UPDATE),
        Some(_) => Some(0),
        None if has("--settings") => Some(REMOTE_SHOW_SETTINGS),
        None => None,
    };
    let action = if has("--fullscreen") {
        Some(hotkey::Action::Fullscreen)
    } else if has("--window") {
        Some(hotkey::Action::Window)
    } else if has("--capture") {
        Some(hotkey::Action::Region)
    } else {
        None
    };

    // One instance only: a second launch just asks the first one to capture.
    let mutex_name = W::new("Local\\RefractScreenerInstance");
    let _mutex = unsafe { CreateMutexW(None, TRUE, mutex_name.p()) };
    if unsafe { GetLastError() } == ERROR_ALREADY_EXISTS {
        match find_window(MAIN_CLASS) {
            Some(h) if link_action.is_some() => unsafe {
                let _ = AllowSetForegroundWindow(ASFW_ANY);
                let _ = PostMessageW(h, WM_APP_REMOTE, WPARAM(link_action.unwrap()), LPARAM(0));
            },
            Some(h) if !has("--background") => {
                // A capture flag captures; a plain launch (Start menu, the EXE) opens Settings.
                let code = action.map(|a| a as usize).unwrap_or(REMOTE_SHOW_SETTINGS);
                log(&format!("second instance: asking {:?} for {code}", h.0));
                unsafe {
                    let _ = AllowSetForegroundWindow(ASFW_ANY);
                    let _ = PostMessageW(h, WM_APP_REMOTE, WPARAM(code), LPARAM(0));
                }
            }
            Some(_) => {}
            None => log("second instance: running instance window not found"),
        }
        return;
    }

    // First run of a downloaded copy: offer to install.
    let portable = !install::is_installed_copy();
    if portable
        && !has("--background")
        && action.is_none()
        && link_action.is_none()
        && !install::installed_exe().exists()
    {
        let r = message_box(
            "Install Refract-screener?\n\n\
             - starts with Windows and lives in the tray\n\
             - PrintScreen and Win+Shift+S open Refract instead of Snipping Tool\n\
             - screenshots go to Pictures\\Refract Screenshots\n\n\
             No: just run it this time.",
            MB_YESNOCANCEL | MB_ICONQUESTION,
        );
        if r == IDCANCEL {
            return;
        }
        if r == IDYES {
            drop(_mutex);
            match install::install() {
                Ok(exe) => {
                    install::launch(&exe, &["--welcome"]);
                    return;
                }
                Err(e) => {
                    message_box(&e, MB_ICONERROR);
                }
            }
        }
    }

    let loaded = config::load();
    toast::register();
    update::register_protocol();
    update::cleanup();
    install::refresh_registration();
    let _ = shots_dir(&loaded.cfg.save_dir);
    let hwnd = match create_main_window() {
        Ok(h) => h,
        Err(_) => return,
    };
    let small = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16);
    let app_state = App {
        hwnd,
        cfg: loaded.cfg,
        cfg_error: loaded.error,
        icon: icon::make(small),
        big_icon: icon::make((small * 2).max(32)),
        last_saved: None,
        taskbar_created: unsafe { RegisterWindowMessageW(W::new("TaskbarCreated").p()) },
        asked_cfa: false,
    };
    APP.with(|a| *a.borrow_mut() = Some(app_state));
    log(&format!(
        "started {} (portable: {portable})",
        exe_path().display()
    ));
    tray_add();
    apply_hotkeys();
    hotkey::start(hwnd);
    enforce_snipping();

    if let Some(e) = app(|a| a.cfg_error.clone()).flatten() {
        notify(
            "Settings file has an error",
            &format!("Using defaults until it is fixed.\n{e}"),
            false,
        );
    } else if has("--updated") {
        notify(
            &format!("Refract-screener updated to {}", update::current()),
            "PrintScreen or Win+Shift+S to capture.",
            false,
        );
    } else if has("--welcome") {
        notify(
            "Refract-screener is ready",
            "PrintScreen or Win+Shift+S to capture. Settings: click the tray icon or open Refract-screener from Start.",
            false,
        );
    }
    if let Some(a) = action {
        unsafe {
            let _ = PostMessageW(hwnd, WM_APP_HOTKEY, WPARAM(a as usize), LPARAM(0));
        }
    }

    let plain_start = !has("--background") && action.is_none() && link_action.is_none();
    if link_action == Some(REMOTE_SHOW_SETTINGS) || plain_start {
        settings::show();
    }
    if app(|a| a.cfg.auto_update).unwrap_or(true) {
        unsafe {
            SetTimer(hwnd, update::TIMER_CHECK, 12_000, None);
        }
    }
    unsafe {
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
    tray_remove();
}

fn create_main_window() -> windows::core::Result<HWND> {
    unsafe {
        let class = W::new(MAIN_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(main_proc),
            hInstance: GetModuleHandleW(None)?.into(),
            lpszClassName: class.p(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let title = W::new(APP_NAME);
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            class.p(),
            title.p(),
            WS_OVERLAPPED,
            0,
            0,
            0,
            0,
            None,
            None,
            GetModuleHandleW(None)?,
            None,
        )
    }
}

fn apply_hotkeys() {
    let Some(cfg) = app(|a| a.cfg.clone()) else {
        return;
    };
    hotkey::set_combos(hotkey::combos(&cfg));
    guard::reload();
}

/// Keeps Snipping Tool switched off while "Replace Snipping Tool" is on: the
/// Windows PrintScreen setting (re-applied on every start, Windows updates and
/// other tools may flip it back) and the background key guard.
fn enforce_snipping() {
    if app(|a| a.cfg.replace_snipping_tool).unwrap_or(false) {
        if !install::prtsc_taken() {
            install::take_prtsc();
        }
        guard::ensure();
    }
}

// ---------------------------------------------------------------- tray

fn tray_data(a: &App) -> NOTIFYICONDATAW {
    let mut nid = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: a.hwnd,
        uID: 1,
        uFlags: NIF_ICON | NIF_MESSAGE | NIF_TIP | NIF_SHOWTIP,
        uCallbackMessage: WM_APP_TRAY,
        hIcon: a.icon,
        ..Default::default()
    };
    let tip = wide("Refract-screener  -  click for settings, PrintScreen to capture");
    let n = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..n].copy_from_slice(&tip[..n]);
    nid
}

fn tray_add() {
    app(|a| unsafe {
        let mut nid = tray_data(a);
        let _ = Shell_NotifyIconW(NIM_ADD, &nid);
        nid.Anonymous.uVersion = NOTIFYICON_VERSION_4;
        let _ = Shell_NotifyIconW(NIM_SETVERSION, &nid);
    });
}

fn tray_remove() {
    app(|a| unsafe {
        let nid = tray_data(a);
        let _ = Shell_NotifyIconW(NIM_DELETE, &nid);
    });
}

fn copy_into(dst: &mut [u16], s: &str) {
    let w = wide(s);
    let n = w.len().min(dst.len() - 1);
    dst[..n].copy_from_slice(&w[..n]);
    dst[n] = 0;
}

fn notify(title: &str, body: &str, force: bool) {
    if !force && !app(|a| a.cfg.notify).unwrap_or(true) {
        return;
    }
    let shown = toast::show(&toast::Toast {
        title,
        body,
        image: None,
        open: None,
        folder: None,
        launch_uri: None,
        button: None,
    });
    if !shown {
        balloon(title, body);
    }
}

pub fn settings_snapshot() -> settings::Snapshot {
    let cfg = app(|a| a.cfg.clone()).unwrap_or_default();
    let cfg_replace = cfg.replace_snipping_tool;
    settings::Snapshot {
        dir: shots_dir(&cfg.save_dir),
        cfg,
        autostart: install::autostart_enabled(),
        prtsc: cfg_replace,
        update_ready: update::ready_version(),
    }
}

pub fn menu_action(id: usize) {
    on_menu(id);
    settings::refresh();
}

pub fn notify_public(title: &str, body: &str) {
    notify(title, body, true);
}

const REMOTE_APPLY_UPDATE: usize = 10;
const REMOTE_SHOW_UPDATE: usize = 11;
const REMOTE_SHOW_SETTINGS: usize = 12;
const REMOTE_CHECK_UPDATE: usize = 13;

fn wait_for_exit(pid: u32) {
    use windows::Win32::System::Threading::*;
    unsafe {
        if let Ok(h) = OpenProcess(PROCESS_SYNCHRONIZE, FALSE, pid) {
            let _ = WaitForSingleObject(h, 15_000);
            let _ = CloseHandle(h);
        }
    }
}

/// "Restart now" on the update card or notification.
pub fn restart_for_update() {
    if overlay::is_open() {
        return;
    }
    guard::stop();
    if update::apply() {
        tray_remove();
        unsafe { PostQuitMessage(0) };
    } else {
        notify(
            "Could not install the update",
            "It will be tried again next time.",
            true,
        );
    }
}

/// Update server: config `update_url`, by default the one baked in at build time
/// (REFRACT_UPDATE_URL). A build without it has no auto-update.
fn update_base() -> Option<String> {
    app(|a| a.cfg.update_url.trim().trim_end_matches('/').to_string())
        .filter(|s| s.starts_with("https://") || s.starts_with("http://"))
}

fn check_updates(h: HWND, manual: bool) {
    match update_base() {
        Some(base) => update::check(h, base, manual),
        None if manual => notify(
            "Updates are not configured",
            "This build has no update server. Set \"update_url\" in config.json.",
            true,
        ),
        None => {}
    }
}

fn balloon(title: &str, body: &str) {
    app(|a| unsafe {
        let mut nid = tray_data(a);
        nid.uFlags = NIF_INFO;
        copy_into(&mut nid.szInfoTitle, title);
        copy_into(&mut nid.szInfo, body);
        nid.dwInfoFlags = NIIF_USER | NIIF_LARGE_ICON | NIIF_NOSOUND;
        nid.hBalloonIcon = a.big_icon;
        let _ = Shell_NotifyIconW(NIM_MODIFY, &nid);
    });
}

pub mod menu_id {
    pub const SETTINGS: usize = 9;
    pub const CAPTURE: usize = 1;
    pub const FULL: usize = 2;
    pub const DELAY: usize = 3;
    pub const OPEN_DIR: usize = 10;
    pub const CHANGE_DIR: usize = 11;
    pub const RESET_DIR: usize = 12;
    pub const ALLOW_CFA: usize = 13;
    pub const PNG: usize = 20;
    pub const WEBP: usize = 21;
    pub const COPY: usize = 22;
    pub const AUTOSAVE: usize = 23;
    pub const NOTIFY: usize = 24;
    pub const CURSOR: usize = 25;
    pub const AUTO_OCR: usize = 26;
    pub const SOUND: usize = 29;
    pub const LOUPE: usize = 27;
    pub const SNAP: usize = 28;
    pub const EFFORT: usize = 70; // 70..=76
    pub const LANG: usize = 100; // 100..
    pub const AUTOSTART: usize = 40;
    pub const PRTSC: usize = 41;
    pub const EDIT_CFG: usize = 50;
    pub const RELOAD_CFG: usize = 51;
    pub const API_KEY: usize = 52;
    pub const INSTALL: usize = 60;
    pub const UNINSTALL: usize = 61;
    pub const EXIT: usize = 99;
    pub const CHECK_UPDATE: usize = 80;
    pub const APPLY_UPDATE: usize = 81;
    pub const AUTO_UPDATE: usize = 82;
}

fn show_menu() {
    let Some((hwnd, cfg)) = app(|a| (a.hwnd, a.cfg.clone())) else {
        return;
    };
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let add = |menu: HMENU, id: usize, text: &str, checked: bool, enabled: bool| {
            let mut f = MF_STRING;
            if checked {
                f |= MF_CHECKED;
            }
            if !enabled {
                f |= MF_GRAYED;
            }
            let t = W::new(text);
            let _ = AppendMenuW(menu, f, id, t.p());
        };
        let sep = |menu: HMENU| {
            let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        };
        let sub = |menu: HMENU, child: HMENU, text: &str| {
            let t = W::new(text);
            let _ = AppendMenuW(menu, MF_POPUP, child.0 as usize, t.p());
        };
        use menu_id::*;
        add(m, SETTINGS, "Settings...", false, true);
        sep(m);
        add(m, CAPTURE, "Capture region\tPrintScreen", false, true);
        add(
            m,
            FULL,
            "Capture full screen\tCtrl+PrintScreen",
            false,
            true,
        );
        add(
            m,
            DELAY,
            &format!("Capture in {} seconds", cfg.delay_seconds),
            false,
            true,
        );
        sep(m);

        let dir = shots_dir(&cfg.save_dir);
        let folder = CreatePopupMenu().unwrap_or_default();
        add(
            folder,
            0,
            &shorten(&dir.display().to_string(), 60),
            false,
            false,
        );
        sep(folder);
        add(folder, OPEN_DIR, "Open folder", false, true);
        add(folder, CHANGE_DIR, "Change folder...", false, true);
        add(
            folder,
            RESET_DIR,
            "Reset to Pictures\\Refract Screenshots",
            false,
            !cfg.save_dir.is_empty(),
        );
        sep(folder);
        add(
            folder,
            ALLOW_CFA,
            "Allow saving to protected folders (admin)...",
            false,
            true,
        );
        sub(m, folder, "Screenshot folder");
        add(m, OPEN_DIR, "Open screenshots", false, true);
        sep(m);

        let fmt = CreatePopupMenu().unwrap_or_default();
        add(
            fmt,
            PNG,
            "PNG  -  lossless, opens everywhere",
            cfg.format == ImageFormat::Png,
            true,
        );
        add(
            fmt,
            WEBP,
            "WebP  -  lossless, ~25% smaller",
            cfg.format == ImageFormat::WebP,
            true,
        );
        sep(fmt);
        let names = [
            "0 - fastest",
            "1",
            "2",
            "3",
            "4 - balanced",
            "5",
            "6 - smallest file (slow)",
        ];
        for (i, n) in names.iter().enumerate() {
            add(
                fmt,
                EFFORT + i,
                &format!("PNG compression {n}"),
                cfg.png_effort as usize == i,
                cfg.format == ImageFormat::Png,
            );
        }
        sub(m, fmt, "Image format");

        let after = CreatePopupMenu().unwrap_or_default();
        add(
            after,
            COPY,
            "Copy to clipboard",
            cfg.copy_to_clipboard,
            true,
        );
        add(
            after,
            AUTOSAVE,
            "Save to the folder automatically",
            cfg.auto_save,
            true,
        );
        add(after, NOTIFY, "Show notifications", cfg.notify, true);
        add(after, SOUND, "Play sound when saved", cfg.sound, true);
        sep(after);
        add(
            after,
            CURSOR,
            "Include mouse cursor",
            cfg.capture_cursor,
            true,
        );
        add(
            after,
            LOUPE,
            "Magnifier while selecting",
            cfg.magnifier,
            true,
        );
        add(after, SNAP, "Snap to windows", cfg.snap_windows, true);
        add(
            after,
            AUTO_OCR,
            "Recognise text automatically",
            cfg.auto_ocr,
            true,
        );
        sub(m, after, "Capture options");

        let lang = CreatePopupMenu().unwrap_or_default();
        for (i, l) in cfg.translate_languages.iter().enumerate() {
            let name = overlay::lang_name(l);
            add(
                lang,
                LANG + i,
                &format!("{}  {}", l.to_uppercase(), name),
                *l == cfg.translate_to,
                true,
            );
        }
        sep(lang);
        add(
            lang,
            API_KEY,
            if cfg.google_api_key.trim().is_empty() {
                "Google Cloud API key: not set (free endpoint)..."
            } else {
                "Google Cloud API key: set..."
            },
            false,
            true,
        );
        sub(m, lang, "Translate to");
        sep(m);

        add(
            m,
            AUTOSTART,
            "Start with Windows",
            install::autostart_enabled(),
            true,
        );
        add(
            m,
            PRTSC,
            "Replace Snipping Tool (PrintScreen)",
            install::prtsc_taken(),
            true,
        );
        add(m, EDIT_CFG, "All settings (config.json)...", false, true);
        add(m, RELOAD_CFG, "Reload settings", false, true);
        sep(m);
        if let Some(v) = update::ready_version() {
            add(
                m,
                APPLY_UPDATE,
                &format!("Restart to update to {v}"),
                false,
                true,
            );
        }
        add(
            m,
            CHECK_UPDATE,
            &format!("Check for updates (version {})", update::current()),
            false,
            true,
        );
        add(
            m,
            AUTO_UPDATE,
            "Update automatically",
            cfg.auto_update,
            true,
        );
        if install::is_installed_copy() {
            add(m, UNINSTALL, "Uninstall...", false, true);
        } else {
            add(
                m,
                INSTALL,
                "Install (start with Windows, replace Snipping Tool)",
                false,
                true,
            );
        }
        add(m, EXIT, "Exit", false, true);

        let mut ptc = POINT::default();
        let _ = GetCursorPos(&mut ptc);
        let _ = SetForegroundWindow(hwnd);
        let cmd = TrackPopupMenuEx(
            m,
            (TPM_RETURNCMD | TPM_RIGHTBUTTON | TPM_BOTTOMALIGN).0,
            ptc.x,
            ptc.y,
            hwnd,
            None,
        );
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
        let _ = DestroyMenu(m);
        if cmd.0 > 0 {
            on_menu(cmd.0 as usize);
        }
    }
}

fn shorten(s: &str, n: usize) -> String {
    let c: Vec<char> = s.chars().collect();
    if c.len() <= n {
        s.to_string()
    } else {
        format!("...{}", c[c.len() - n + 3..].iter().collect::<String>())
    }
}

fn update_cfg(f: impl FnOnce(&mut Config)) {
    app(|a| {
        f(&mut a.cfg);
        if a.cfg_error.is_none() {
            config::save(&a.cfg);
        }
    });
}

fn on_menu(id: usize) {
    use menu_id::*;
    let hwnd = app(|a| a.hwnd).unwrap_or_default();
    match id {
        SETTINGS => settings::show(),
        CAPTURE => start_capture(hotkey::Action::Region),
        FULL => start_capture(hotkey::Action::Fullscreen),
        DELAY => {
            let secs = app(|a| a.cfg.delay_seconds).unwrap_or(3).clamp(1, 60);
            unsafe {
                SetTimer(hwnd, TIMER_DELAY, secs * 1000, None);
            }
        }
        OPEN_DIR => {
            let dir = shots_dir(&app(|a| a.cfg.save_dir.clone()).unwrap_or_default());
            shell_open(&dir.display().to_string());
        }
        CHANGE_DIR => {
            let current = shots_dir(&app(|a| a.cfg.save_dir.clone()).unwrap_or_default());
            if let Some(d) = pick_folder(hwnd, &current) {
                let s = d.display().to_string();
                update_cfg(|c| c.save_dir = s.clone());
                notify("Screenshot folder changed", &s, true);
            }
        }
        RESET_DIR => update_cfg(|c| c.save_dir.clear()),
        ALLOW_CFA => allow_cfa(),
        PNG => update_cfg(|c| c.format = ImageFormat::Png),
        WEBP => update_cfg(|c| c.format = ImageFormat::WebP),
        COPY => update_cfg(|c| c.copy_to_clipboard = !c.copy_to_clipboard),
        AUTOSAVE => update_cfg(|c| c.auto_save = !c.auto_save),
        NOTIFY => update_cfg(|c| c.notify = !c.notify),
        CURSOR => update_cfg(|c| c.capture_cursor = !c.capture_cursor),
        AUTO_OCR => update_cfg(|c| c.auto_ocr = !c.auto_ocr),
        SOUND => {
            update_cfg(|c| c.sound = !c.sound);
            if app(|a| a.cfg.sound) == Some(true) {
                toast::play_save_sound();
            }
        }
        LOUPE => update_cfg(|c| c.magnifier = !c.magnifier),
        SNAP => update_cfg(|c| c.snap_windows = !c.snap_windows),
        AUTOSTART => install::set_autostart(!install::autostart_enabled()),
        PRTSC => {
            let on = !app(|a| a.cfg.replace_snipping_tool).unwrap_or(true);
            update_cfg(|c| c.replace_snipping_tool = on);
            if on {
                install::take_prtsc();
                apply_hotkeys();
                guard::ensure();
                guard::kill_snipping();
                notify(
                    "Snipping Tool is off",
                    "PrintScreen and Win+Shift+S open Refract, even when Refract is closed.",
                    true,
                );
            } else {
                guard::stop();
                install::restore_prtsc();
                apply_hotkeys();
                notify(
                    "Snipping Tool is back",
                    "PrintScreen and Win+Shift+S open Snipping Tool again.",
                    true,
                );
            }
        }
        API_KEY => {
            message_box(
                "Translation uses the free Google Translate endpoint by default.\n\n\
                 For the official Google Cloud Translation API, put your key into\n\
                 \"google_api_key\" in config.json (All settings...), then choose Reload settings.",
                MB_ICONINFORMATION,
            );
            open_config();
        }
        EDIT_CFG => open_config(),
        RELOAD_CFG => {
            let l = config::load();
            let err = l.error.clone();
            app(|a| {
                a.cfg = l.cfg;
                a.cfg_error = l.error;
            });
            apply_hotkeys();
            match err {
                Some(e) => notify("Settings file has an error", &e, true),
                None => notify("Settings reloaded", "", true),
            }
        }
        INSTALL => match install::install() {
            Ok(exe) => {
                // The installed copy takes over the tray.
                tray_remove();
                install::launch(&exe, &["--welcome"]);
                unsafe { PostQuitMessage(0) };
            }
            Err(e) => {
                message_box(&e, MB_ICONERROR);
            }
        },
        UNINSTALL => {
            if message_box(
                "Uninstall Refract-screener?\nScreenshots and settings are kept.",
                MB_YESNO | MB_ICONQUESTION,
            ) == IDYES
            {
                tray_remove();
                let exe = exe_path();
                install::launch(&exe, &["--uninstall"]);
                unsafe { PostQuitMessage(0) };
            }
        }
        EXIT => unsafe { PostQuitMessage(0) },
        CHECK_UPDATE => check_updates(hwnd, true),
        APPLY_UPDATE => restart_for_update(),
        AUTO_UPDATE => {
            update_cfg(|c| c.auto_update = !c.auto_update);
            if app(|a| a.cfg.auto_update) == Some(true) {
                check_updates(hwnd, false);
            }
        }
        x if (LANG..LANG + 64).contains(&x) => {
            let i = x - LANG;
            update_cfg(|c| {
                if let Some(l) = c.translate_languages.get(i) {
                    c.translate_to = l.clone();
                }
            });
        }
        x if (EFFORT..=EFFORT + 6).contains(&x) => {
            update_cfg(|c| c.png_effort = (x - EFFORT) as u8)
        }
        _ => {}
    }
}

fn open_config() {
    let path = config::config_path();
    if !path.exists() {
        if let Some(c) = app(|a| a.cfg.clone()) {
            config::save(&c);
        }
    }
    let p = W::new(&path.display().to_string());
    let op = W::new("open");
    unsafe {
        let r = ShellExecuteW(
            None,
            op.p(),
            p.p(),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
        if (r.0 as isize) <= 32 {
            let np = W::new("notepad.exe");
            ShellExecuteW(None, op.p(), np.p(), p.p(), PCWSTR::null(), SW_SHOWNORMAL);
        }
    }
}

// ---------------------------------------------------------------- dialogs

unsafe fn shell_item(path: &std::path::Path) -> Option<IShellItem> {
    let p = W::new(&path.display().to_string());
    SHCreateItemFromParsingName(p.p(), None).ok()
}

unsafe fn item_path(item: &IShellItem) -> Option<PathBuf> {
    let p: PWSTR = item.GetDisplayName(SIGDN_FILESYSPATH).ok()?;
    let s = p.to_string().ok();
    CoTaskMemFree(Some(p.0 as *const _));
    s.map(PathBuf::from)
}

fn pick_folder(owner: HWND, start: &std::path::Path) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileOpenDialog =
            CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let opts = dlg.GetOptions().ok()?;
        dlg.SetOptions(opts | FOS_PICKFOLDERS | FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST)
            .ok()?;
        let t = W::new("Folder for screenshots");
        let _ = dlg.SetTitle(t.p());
        if let Some(it) = shell_item(start) {
            let _ = dlg.SetFolder(&it);
        }
        dlg.Show(owner).ok()?;
        item_path(&dlg.GetResult().ok()?)
    }
}

fn save_dialog(
    owner: HWND,
    dir: &std::path::Path,
    name: &str,
    fmt: ImageFormat,
) -> Option<PathBuf> {
    unsafe {
        let dlg: IFileSaveDialog =
            CoCreateInstance(&FileSaveDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        let n1 = W::new("PNG image (lossless)");
        let s1 = W::new("*.png");
        let n2 = W::new("WebP image (lossless, smaller)");
        let s2 = W::new("*.webp");
        let specs = [
            COMDLG_FILTERSPEC {
                pszName: n1.p(),
                pszSpec: s1.p(),
            },
            COMDLG_FILTERSPEC {
                pszName: n2.p(),
                pszSpec: s2.p(),
            },
        ];
        dlg.SetFileTypes(&specs).ok()?;
        let _ = dlg.SetFileTypeIndex(if fmt == ImageFormat::WebP { 2 } else { 1 });
        let ext = W::new(fmt.ext());
        let _ = dlg.SetDefaultExtension(ext.p());
        let nm = W::new(name);
        let _ = dlg.SetFileName(nm.p());
        if let Some(it) = shell_item(dir) {
            let _ = dlg.SetFolder(&it);
        }
        dlg.Show(owner).ok()?;
        item_path(&dlg.GetResult().ok()?)
    }
}

// ---------------------------------------------------------------- capture flow

fn start_capture(action: hotkey::Action) {
    if overlay::is_open() {
        log("capture ignored: overlay already open");
        return;
    }
    log(&format!("capture {action:?}"));
    // Pick up manual edits of config.json.
    let l = config::load();
    if l.error.is_none() {
        app(|a| {
            a.cfg = l.cfg;
            a.cfg_error = None;
        });
        apply_hotkeys();
    }
    let Some((hwnd, cfg)) = app(|a| (a.hwnd, a.cfg.clone())) else {
        return;
    };
    let shot = match capture::grab(cfg.capture_cursor, &[hwnd]) {
        Ok(s) => s,
        Err(e) => {
            log(&format!("capture failed: {e}"));
            notify("Capture failed", &e.message(), true);
            return;
        }
    };
    match action {
        hotkey::Action::Region => {
            if let Err(e) = overlay::open(hwnd, cfg, shot) {
                log(&format!("overlay failed: {e}"));
                notify("Cannot open the capture window", &e.message(), true);
            }
        }
        hotkey::Action::Fullscreen | hotkey::Action::Window => {
            let rect = if action == hotkey::Action::Window {
                shot.foreground
            } else {
                let mut p = POINT::default();
                unsafe {
                    let _ = GetCursorPos(&mut p);
                }
                shot.monitor_at(p.x - shot.ox, p.y - shot.oy)
                    .map(|m| m.rect)
            }
            .unwrap_or(shot.full());
            let img = encode::Image {
                w: rect.w() as u32,
                h: rect.h() as u32,
                bgra: shot.crop(&rect),
            };
            deliver(img, overlay::Finish::Save);
        }
    }
}

fn on_overlay_done() {
    let Some(mut ov) = overlay::take() else {
        return;
    };
    let done = ov.done.take();
    // Keep the tool, styles and target language the user picked.
    let (tool, styles, lang) = (
        ov.cfg.tool,
        ov.cfg.styles.clone(),
        ov.cfg.translate_to.clone(),
    );
    drop(ov);
    update_cfg(|c| {
        c.tool = tool;
        c.styles = styles;
        c.translate_to = lang;
    });
    if let Some(d) = done {
        if let Some(img) = d.image {
            deliver(img, d.how);
        }
    }
}

fn deliver(img: encode::Image, how: overlay::Finish) {
    use overlay::Finish;
    let Some((hwnd, cfg)) = app(|a| (a.hwnd, a.cfg.clone())) else {
        return;
    };
    if how == Finish::Cancel {
        return;
    }
    let copy = how == Finish::Copy || cfg.copy_to_clipboard;
    let copied = if copy {
        let png = encode::png(&img, 1).ok();
        encode::copy_image(hwnd, &img, png.as_deref())
    } else {
        false
    };
    let (w, h) = (img.w as i32, img.h as i32);
    let dir = shots_dir(&cfg.save_dir);
    let name = file_name(&cfg.file_name, w, h);
    let path = match how {
        Finish::SaveAs => save_dialog(hwnd, &dir, &name, cfg.format),
        Finish::Save => Some(unique_path(&dir, &name, cfg.format.ext())),
        _ if cfg.auto_save => Some(unique_path(&dir, &name, cfg.format.ext())),
        _ => None,
    };
    if path.is_none() && !copied {
        return;
    }

    // Feedback right away, like Snipping Tool; the lossless file is written meanwhile.
    if path.is_some() && cfg.sound {
        toast::play_save_sound();
    }
    if cfg.notify {
        let thumb = toast::thumbnail(&img);
        let folder = path
            .as_ref()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()));
        let folder_name = folder
            .as_ref()
            .and_then(|d| d.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let (title, body) = match (copied, path.is_some()) {
            (true, true) => (
                "Screenshot copied to clipboard".to_string(),
                format!("Automatically saved to {folder_name}. Select here to open it."),
            ),
            (false, true) => (
                "Screenshot saved".to_string(),
                format!("Saved to {folder_name}. Select here to open it."),
            ),
            _ => (
                "Screenshot copied to clipboard".to_string(),
                format!("{w} \u{00D7} {h}. Paste it anywhere with Ctrl+V."),
            ),
        };
        let shown = toast::show(&toast::Toast {
            title: &title,
            body: &body,
            image: thumb.as_deref(),
            open: path.as_deref(),
            folder: folder.as_deref(),
            launch_uri: None,
            button: None,
        });
        if !shown {
            balloon(&title, &body);
        }
    }
    let Some(path) = path else { return };
    app(|a| a.last_saved = Some(path.clone()));
    let effort = cfg.png_effort;
    let main = hwnd_to(hwnd);
    std::thread::spawn(move || {
        let mut path = path;
        let mut blocked = false;
        let mut res = encode::save(&img, &path, effort);
        if let Err(e) = &res {
            log(&format!("save to {} failed: {}", path.display(), e.msg));
            if e.blocked {
                // Probably Controlled folder access: keep the shot somewhere safe.
                blocked = true;
                let home = std::env::var("USERPROFILE")
                    .map(PathBuf::from)
                    .unwrap_or_else(|_| std::env::temp_dir());
                let dir = home.join("Refract Screenshots");
                let name = path
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "Refract".into());
                let ext = path
                    .extension()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| "png".into());
                path = unique_path(&dir, &name, &ext);
                res = encode::save(&img, &path, effort);
            }
        }
        let note = match res {
            Ok(bytes) => {
                log(&format!("saved {} ({})", path.display(), human(bytes)));
                SaveNote {
                    title: if blocked {
                        "Saved to your user folder".to_string()
                    } else {
                        String::new()
                    },
                    body: if blocked {
                        format!(
                            "Windows blocked the Pictures folder. Saved to {}",
                            path.display()
                        )
                    } else {
                        String::new()
                    },
                    path: Some(path),
                    blocked,
                }
            }
            Err(e) => {
                log(&format!("save failed: {}", e.msg));
                SaveNote {
                    title: "Could not save the screenshot".to_string(),
                    body: e.msg,
                    path: None,
                    blocked,
                }
            }
        };
        let b = Box::new(note);
        unsafe {
            let p = Box::into_raw(b);
            if PostMessageW(
                hwnd_from(main),
                WM_APP_NOTIFY,
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

fn human(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.1} MB", b as f64 / 1048576.0)
    } else {
        format!("{} KB", (b + 1023) / 1024)
    }
}

unsafe extern "system" fn main_proc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        update::WM_APP_UPD_FOUND..=update::WM_APP_UPD_NONE => {
            update::on_message(h, msg, lp);
            LRESULT(0)
        }
        WM_TIMER if wp.0 == update::TIMER_CHECK => {
            let _ = KillTimer(h, update::TIMER_CHECK);
            if app(|a| a.cfg.auto_update).unwrap_or(false) {
                check_updates(h, false);
                SetTimer(h, update::TIMER_CHECK, update::CHECK_EVERY_MS, None);
            }
            LRESULT(0)
        }
        WM_APP_REMOTE if wp.0 == REMOTE_APPLY_UPDATE => {
            restart_for_update();
            LRESULT(0)
        }
        WM_APP_REMOTE if wp.0 == REMOTE_SHOW_UPDATE => {
            update::show_window(h);
            LRESULT(0)
        }
        WM_APP_REMOTE if wp.0 == REMOTE_SHOW_SETTINGS => {
            settings::show();
            LRESULT(0)
        }
        WM_APP_REMOTE if wp.0 == REMOTE_CHECK_UPDATE => {
            check_updates(h, true);
            LRESULT(0)
        }
        WM_APP_REMOTE if wp.0 == 0 => LRESULT(0),
        WM_APP_HOTKEY | WM_APP_REMOTE => {
            if let Some(a) = hotkey::Action::from(wp.0) {
                start_capture(a);
            }
            LRESULT(0)
        }
        WM_APP_OVERLAY_DONE => {
            on_overlay_done();
            LRESULT(0)
        }
        WM_APP_NOTIFY => {
            let n = *Box::from_raw(lp.0 as *mut SaveNote);
            if n.path.is_some() {
                app(|a| a.last_saved = n.path.clone());
            }
            if !n.title.is_empty() {
                if n.blocked {
                    let p = n.path.clone();
                    let shown = toast::show(&toast::Toast {
                        title: &n.title,
                        body: &n.body,
                        image: None,
                        open: p.as_deref(),
                        folder: p.as_deref().and_then(|x| x.parent()),
                        launch_uri: None,
                        button: None,
                    });
                    if !shown {
                        balloon(&n.title, &n.body);
                    }
                } else {
                    notify(&n.title, &n.body, true);
                }
            }
            if n.blocked && app(|a| std::mem::replace(&mut a.asked_cfa, true)) == Some(false) {
                offer_cfa();
            }
            LRESULT(0)
        }
        WM_APP_TRAY => {
            let ev = (lp.0 & 0xFFFF) as u32;
            match ev {
                WM_CONTEXTMENU | WM_RBUTTONUP => show_menu(),
                NIN_SELECT | 0x401 | WM_LBUTTONUP => settings::show(),
                NIN_BALLOONUSERCLICK => {
                    if let Some(p) = app(|a| a.last_saved.clone()).flatten() {
                        shell_select(&p);
                    }
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_DELAY => {
            let _ = KillTimer(h, TIMER_DELAY);
            start_capture(hotkey::Action::Region);
            LRESULT(0)
        }
        WM_POWERBROADCAST if wp.0 == 0x12 /* PBT_APMRESUMEAUTOMATIC */ => {
            let _ = SetTimer(h, update::TIMER_CHECK, 30_000, None);
            LRESULT(1)
        }
        WM_CLOSE => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => {
            if app(|a| a.taskbar_created).is_some_and(|m| m == msg && m != 0) {
                tray_add();
                return LRESULT(0);
            }
            DefWindowProcW(h, msg, wp, lp)
        }
    }
}

// ---------------------------------------------------------------- self test

/// Captures the screen, draws every tool, renders, encodes PNG and WebP and checks
/// that decoding them gives back exactly the same pixels. Prints a report.
fn selftest(dir: PathBuf) -> i32 {
    use geom::*;
    use shapes::*;
    let _ = std::fs::create_dir_all(&dir);
    let mut log = String::new();
    let mut fail = 0;
    let mut out = |s: String| {
        log.push_str(&s);
        log.push('\n');
    };
    let shot = match capture::grab(false, &[]) {
        Ok(s) => s,
        Err(e) => {
            let _ = std::fs::write(dir.join("selftest.txt"), format!("capture failed: {e}"));
            return 1;
        }
    };
    out(format!(
        "virtual screen {}x{} at ({},{}), {} monitors, {} windows",
        shot.w,
        shot.h,
        shot.ox,
        shot.oy,
        shot.monitors.len(),
        shot.windows.len()
    ));
    for m in &shot.monitors {
        out(format!("  monitor {:?} scale {}", m.rect, m.scale));
    }
    let m0 = shot.monitors.first().map(|m| m.rect).unwrap_or(shot.full());
    let sel = RectI::new(
        m0.l + m0.w() / 8,
        m0.t + m0.h() / 8,
        m0.r - m0.w() / 8,
        m0.b - m0.h() / 8,
    );
    let f = sel.f();
    let at = |x: f32, y: f32| p(f.l + f.w() * x, f.t + f.h() * y);
    let cfg = Config::default();
    let st = |t: Tool| cfg.style(t);
    let shapes_list = vec![
        Shape {
            kind: Kind::Blur(at(0.05, 0.05), at(0.3, 0.25)),
            style: st(Tool::Blur),
        },
        Shape {
            kind: Kind::Pixelate(at(0.35, 0.05), at(0.6, 0.25)),
            style: st(Tool::Pixelate),
        },
        Shape {
            kind: Kind::Marker(vec![at(0.05, 0.35), at(0.4, 0.36)]),
            style: st(Tool::Marker),
        },
        Shape {
            kind: Kind::Arrow(at(0.1, 0.8), at(0.45, 0.45)),
            style: st(Tool::Arrow),
        },
        Shape {
            kind: Kind::Rect(at(0.5, 0.4), at(0.7, 0.6)),
            style: st(Tool::Rect),
        },
        Shape {
            kind: Kind::Ellipse(at(0.72, 0.4), at(0.95, 0.65)),
            style: Style {
                fill: Fill::Soft,
                ..st(Tool::Ellipse)
            },
        },
        Shape {
            kind: Kind::Pen(vec![
                at(0.5, 0.8),
                at(0.55, 0.72),
                at(0.6, 0.85),
                at(0.65, 0.7),
            ]),
            style: st(Tool::Pen),
        },
        Shape {
            kind: Kind::Line(at(0.7, 0.9), at(0.95, 0.75)),
            style: st(Tool::Line),
        },
        Shape {
            kind: Kind::Text {
                at: at(0.62, 0.08),
                text: "Refract \u{2014} \u{043f}\u{0440}\u{0438}\u{0432}\u{0435}\u{0442}".into(),
                ext: Default::default(),
            },
            style: Style {
                fill: Fill::Solid,
                ..st(Tool::Text)
            },
        },
        Shape {
            kind: Kind::Counter {
                at: at(0.3, 0.6),
                n: 1,
            },
            style: st(Tool::Counter),
        },
        Shape {
            kind: Kind::Counter {
                at: at(0.38, 0.68),
                n: 12,
            },
            style: st(Tool::Counter),
        },
    ];
    let painter = match paint::Painter::new(&cfg.font, cfg.corner_radius) {
        Ok(p) => p,
        Err(e) => {
            out(format!("painter failed: {e}"));
            let _ = std::fs::write(dir.join("selftest.txt"), log);
            return 1;
        }
    };
    let mut fx = export::FxCache::default();
    let t = std::time::Instant::now();
    let plain = export::render(&painter, &shot, sel, &[], &mut fx, None).unwrap();
    let annotated = match export::render(&painter, &shot, sel, &shapes_list, &mut fx, None) {
        Ok(i) => i,
        Err(e) => {
            out(format!("render failed: {e}"));
            let _ = std::fs::write(dir.join("selftest.txt"), log);
            return 1;
        }
    };
    out(format!(
        "render {}x{} with {} shapes: {} ms",
        annotated.w,
        annotated.h,
        shapes_list.len(),
        t.elapsed().as_millis()
    ));

    // The plain crop must be byte-identical to the captured pixels.
    let identical = plain.bgra == shot.crop(&sel);
    out(format!("plain crop identical to capture: {identical}"));
    if !identical {
        fail += 1;
    }

    let raw_size = (plain.w * plain.h * 3) as u64;
    for (name, img) in [("plain", &plain), ("annotated", &annotated)] {
        for effort in [0u8, 2, 4, 6] {
            let t = std::time::Instant::now();
            let data = encode::png(img, effort).unwrap();
            let ms = t.elapsed().as_millis();
            let ok = decode_wic(&data)
                .map(|d| same_rgb(&d, &img.bgra))
                .unwrap_or(false);
            if !ok {
                fail += 1;
            }
            out(format!(
                "{name} PNG effort {effort}: {} KB ({:.1}% of raw) in {ms} ms, lossless={ok}",
                data.len() / 1024,
                data.len() as f64 * 100.0 / raw_size as f64
            ));
            if effort == 4 {
                let _ = std::fs::write(dir.join(format!("{name}.png")), &data);
            }
        }
        let t = std::time::Instant::now();
        let data = encode::webp(img).unwrap();
        let ms = t.elapsed().as_millis();
        let ok = webp::Decoder::new(&data)
            .decode()
            .map(|d| {
                let px: &[u8] = &d;
                let ch = if d.is_alpha() { 4 } else { 3 };
                px.chunks_exact(ch)
                    .zip(img.bgra.chunks_exact(4))
                    .all(|(a, b)| a[0] == b[2] && a[1] == b[1] && a[2] == b[0])
            })
            .unwrap_or(false);
        if !ok {
            fail += 1;
        }
        out(format!(
            "{name} WebP lossless: {} KB ({:.1}% of raw) in {ms} ms, lossless={ok}",
            data.len() / 1024,
            data.len() as f64 * 100.0 / raw_size as f64
        ));
        let _ = std::fs::write(dir.join(format!("{name}.webp")), &data);
        // Reference: what a plain (non-optimised) encoder produces.
        if let Some(d) = encode_wic_png(img) {
            out(format!(
                "{name} PNG via Windows WIC (reference): {} KB",
                d.len() / 1024
            ));
        }
    }

    // OCR + translation on a picture with real text, then UI previews.
    let text_shape = Shape {
        kind: Kind::Text {
            at: at(0.04, 0.45),
            text: "The quick brown fox jumps over the lazy dog.\nScreenshots stay pixel perfect and small.\nPress Ctrl+C to copy the recognised text.".into(),
            ext: Default::default(),
        },
        style: Style { color: Rgb(0xFFFFFF), fill: Fill::Solid, width: 30.0, ..st(Tool::Text) },
    };
    let mut demo = shapes_list.clone();
    demo.retain(|s| !matches!(s.kind, Kind::Marker(_) | Kind::Arrow(..)));
    demo.push(text_shape);
    demo.push(Shape {
        kind: Kind::Arrow(at(0.62, 0.95), at(0.5, 0.62)),
        style: st(Tool::Arrow),
    });
    let text_img = export::render(&painter, &shot, sel, &demo, &mut fx, None).unwrap();
    // Bake the demo into the screenshot itself, as if it had been on screen.
    let mut shot = shot;
    for y in 0..sel.h() {
        let d = (((sel.t + y) * shot.w + sel.l) * 4) as usize;
        let s = (y * sel.w() * 4) as usize;
        shot.px[d..d + (sel.w() * 4) as usize]
            .copy_from_slice(&text_img.bgra[s..s + (sel.w() * 4) as usize]);
    }
    let mut fx = export::FxCache::default();
    let demo: Vec<Shape> = vec![
        Shape {
            kind: Kind::Arrow(at(0.62, 0.95), at(0.5, 0.62)),
            style: st(Tool::Arrow),
        },
        Shape {
            kind: Kind::Counter {
                at: at(0.66, 0.97),
                n: 1,
            },
            style: st(Tool::Counter),
        },
    ];
    let px = text_img.bgra.clone();
    let lang = cfg.ocr_language.clone();
    let t = std::time::Instant::now();
    let ocr_res = std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
        }
        ocr::recognize(&px, sel, &lang)
    })
    .join()
    .unwrap_or(Err("ocr thread panicked".into()));
    let mut ocr_ok = None;
    match ocr_res {
        Ok(r) => {
            out(format!(
                "ocr ({}) in {} ms: {} words, {} lines, {} blocks",
                r.lang,
                t.elapsed().as_millis(),
                r.words.len(),
                r.lines.len(),
                r.blocks.len()
            ));
            for l in &r.lines {
                out(format!("  line: {:?}", l.text));
            }
            if !r.lines.iter().any(|l| l.text.contains("quick brown fox")) {
                fail += 1;
                out("  expected text not found".into());
            }
            let texts: Vec<String> = r.blocks.iter().map(|b| b.text.clone()).collect();
            let tr = std::thread::spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::WinRT::RoInitialize(
                        windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
                    );
                }
                ocr::translate_now(&texts, "ru", "")
            })
            .join()
            .unwrap_or(Err("translate thread panicked".into()));
            let mut r = Box::new(r);
            match tr {
                Ok(v) => {
                    for (b, t) in r.blocks.iter_mut().zip(v) {
                        out(format!(
                            "  block {:?} -> {:?} (bg {}, fg {})",
                            b.text,
                            t,
                            b.bg.hex(),
                            b.fg.hex()
                        ));
                        b.translated = Some(t);
                    }
                    let timg =
                        export::render(&painter, &shot, sel, &demo, &mut fx, Some(&r.blocks))
                            .unwrap();
                    let _ =
                        std::fs::write(dir.join("translated.png"), encode::png(&timg, 2).unwrap());
                }
                Err(e) => {
                    fail += 1;
                    out(format!("translate failed: {e}"));
                }
            }
            ocr_ok = Some(r);
        }
        Err(e) => {
            fail += 1;
            out(format!("ocr failed: {e}"));
        }
    }
    if let Err(e) = overlay::preview(cfg.clone(), shot, demo, sel, ocr_ok, &dir) {
        fail += 1;
        out(format!("ui preview failed: {e}"));
    }
    out(format!("failures: {fail}"));
    let _ = std::fs::write(dir.join("selftest.txt"), &log);
    if fail == 0 {
        0
    } else {
        2
    }
}

fn same_rgb(decoded_bgra: &[u8], src_bgra: &[u8]) -> bool {
    decoded_bgra.len() == src_bgra.len()
        && decoded_bgra
            .chunks_exact(4)
            .zip(src_bgra.chunks_exact(4))
            .all(|(a, b)| a[..3] == b[..3])
}

fn decode_wic(data: &[u8]) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Imaging::*;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(data).ok()?;
        let dec = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).ok()?;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut buf).ok()?;
        Some(buf)
    }
}

fn encode_wic_png(img: &encode::Image) -> Option<Vec<u8>> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::*;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let tmp = std::env::temp_dir().join(format!("refract-wic-{}.png", std::process::id()));
        let stream = wic.CreateStream().ok()?;
        let p = W::new(&tmp.display().to_string());
        stream.InitializeFromFilename(p.p(), GENERIC_WRITE.0).ok()?;
        let enc = wic
            .CreateEncoder(&GUID_ContainerFormatPng, std::ptr::null())
            .ok()?;
        enc.Initialize(&stream, WICBitmapEncoderNoCache).ok()?;
        let mut frame: Option<IWICBitmapFrameEncode> = None;
        let mut bag: Option<IPropertyBag2> = None;
        enc.CreateNewFrame(&mut frame, &mut bag).ok()?;
        let frame = frame?;
        frame.Initialize(bag.as_ref()).ok()?;
        frame.SetSize(img.w, img.h).ok()?;
        let mut fmt = GUID_WICPixelFormat32bppBGRA;
        frame.SetPixelFormat(&mut fmt).ok()?;
        frame.WritePixels(img.h, img.w * 4, &img.bgra).ok()?;
        frame.Commit().ok()?;
        enc.Commit().ok()?;
        drop(stream);
        let d = std::fs::read(&tmp).ok();
        let _ = std::fs::remove_file(&tmp);
        d
    }
}

fn load_png(path: &std::path::Path) -> Option<(u32, u32, Vec<u8>)> {
    use windows::Win32::Graphics::Imaging::*;
    let data = std::fs::read(path).ok()?;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(&data).ok()?;
        let dec = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &frame,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        conv.GetSize(&mut w, &mut h).ok()?;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut buf).ok()?;
        for a in buf.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        Some((w, h, buf))
    }
}

/// Renders the overlay UI over a given picture (README screenshots):
///   refract-screener.exe --demo-shots <out dir> <background.png>
fn demo_shots(dir: PathBuf, bg: PathBuf) -> i32 {
    use geom::*;
    use shapes::*;
    let Some((w, h, px)) = load_png(&bg) else {
        return 1;
    };
    let _ = std::fs::create_dir_all(&dir);
    let full = RectI::new(0, 0, w as i32, h as i32);
    let shot = capture::Shot {
        ox: 0,
        oy: 0,
        w: w as i32,
        h: h as i32,
        px,
        windows: vec![RectI::new(230, 90, 1371, 801)],
        monitors: vec![capture::Monitor {
            rect: full,
            work: RectI::new(0, 0, w as i32, h as i32 - 48),
            scale: 1.0,
        }],
        foreground: None,
    };
    let sel = RectI::new(262, 158, 1338, 680);
    let cfg = Config::default();
    let st = |t: Tool| cfg.style(t);
    let shapes = vec![
        Shape {
            kind: Kind::Blur(p(316.0, 586.0), p(740.0, 618.0)),
            style: Style {
                width: 10.0,
                ..st(Tool::Blur)
            },
        },
        Shape {
            kind: Kind::Rect(p(290.0, 366.0), p(872.0, 430.0)),
            style: st(Tool::Rect),
        },
        Shape {
            kind: Kind::Arrow(p(1080.0, 310.0), p(885.0, 222.0)),
            style: st(Tool::Arrow),
        },
        Shape {
            kind: Kind::Counter {
                at: p(1120.0, 330.0),
                n: 1,
            },
            style: st(Tool::Counter),
        },
        Shape {
            kind: Kind::Counter {
                at: p(905.0, 398.0),
                n: 2,
            },
            style: st(Tool::Counter),
        },
        Shape {
            kind: Kind::Text {
                at: p(940.0, 470.0),
                text: "Sharper, a third smaller".into(),
                ext: Default::default(),
            },
            style: Style {
                fill: Fill::Solid,
                width: 22.0,
                ..st(Tool::Text)
            },
        },
    ];
    let px = shot.crop(&sel);
    let ocr_res = std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::WinRT::RoInitialize(
                windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
            );
        }
        let mut r = ocr::recognize(&px, sel, "")?;
        let texts: Vec<String> = r.blocks.iter().map(|b| b.text.clone()).collect();
        if let Ok(tr) = ocr::translate_now(&texts, "ru", "") {
            for (b, t) in r.blocks.iter_mut().zip(tr) {
                b.translated = Some(t);
            }
        }
        Ok::<_, String>(r)
    })
    .join()
    .ok()
    .and_then(|r| r.ok())
    .map(Box::new);
    match overlay::preview(cfg, shot, shapes, sel, ocr_res, &dir) {
        Ok(()) => 0,
        Err(_) => 1,
    }
}
