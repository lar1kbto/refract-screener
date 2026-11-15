//! Win32 odds and ends.

use std::path::PathBuf;
use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

pub const APP_NAME: &str = "Refract-screener";
pub const MAIN_CLASS: &str = "RefractScreenerMain";
pub const OVERLAY_CLASS: &str = "RefractScreenerOverlay";

// Messages to the main (tray) window.
pub const WM_APP_HOTKEY: u32 = WM_APP + 1; // wparam = hotkey action
pub const WM_APP_TRAY: u32 = WM_APP + 2;
pub const WM_APP_NOTIFY: u32 = WM_APP + 3; // lparam = Box<(String, String)>
pub const WM_APP_OVERLAY_DONE: u32 = WM_APP + 4;
pub const WM_APP_REMOTE: u32 = WM_APP + 5; // second instance asked us to act; wparam = action
                                           // Messages to the overlay window.
pub const WM_APP_OCR: u32 = WM_APP + 20; // lparam = Box<OcrReply>
pub const WM_APP_TRANSLATED: u32 = WM_APP + 21; // lparam = Box<TranslateReply>

pub fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Keeps a wide string alive for the duration of a call.
pub struct W(pub Vec<u16>);
impl W {
    pub fn new(s: &str) -> W {
        W(wide(s))
    }
    pub fn p(&self) -> PCWSTR {
        PCWSTR(self.0.as_ptr())
    }
}

pub fn hwnd_to(h: HWND) -> isize {
    h.0 as isize
}
pub fn hwnd_from(v: isize) -> HWND {
    HWND(v as *mut core::ffi::c_void)
}

pub fn lparam_xy(l: LPARAM) -> (i32, i32) {
    let v = l.0 as u32;
    (
        (v & 0xFFFF) as i16 as i32,
        ((v >> 16) & 0xFFFF) as i16 as i32,
    )
}

pub fn known_folder(id: &windows::core::GUID) -> Option<PathBuf> {
    unsafe {
        let p: PWSTR = SHGetKnownFolderPath(id, KNOWN_FOLDER_FLAG(0), None).ok()?;
        let s = p.to_string().ok();
        CoTaskMemFree(Some(p.0 as *const _));
        s.map(PathBuf::from)
    }
}

pub fn pictures_dir() -> PathBuf {
    known_folder(&FOLDERID_Pictures)
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var("USERPROFILE").unwrap_or_else(|_| ".".into()))
                .join("Pictures")
        })
        .join("Refract Screenshots")
}

/// Screenshot folder: config.save_dir, or Pictures\Refract Screenshots. Created if missing.
pub fn shots_dir(save_dir: &str) -> PathBuf {
    let d = if save_dir.trim().is_empty() {
        pictures_dir()
    } else {
        PathBuf::from(
            std::env::var("USERPROFILE")
                .map(|h| save_dir.replace('~', &h))
                .unwrap_or_else(|_| save_dir.to_string()),
        )
    };
    let _ = std::fs::create_dir_all(&d);
    d
}

pub fn exe_path() -> PathBuf {
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("refract-screener.exe"))
}

/// Expands the file name template.
pub fn file_name(template: &str, w: i32, h: i32) -> String {
    let t = unsafe { GetLocalTime() };
    let mut s = template.to_string();
    for (k, v) in [
        ("{yyyy}", format!("{:04}", t.wYear)),
        ("{MM}", format!("{:02}", t.wMonth)),
        ("{dd}", format!("{:02}", t.wDay)),
        ("{HH}", format!("{:02}", t.wHour)),
        ("{mm}", format!("{:02}", t.wMinute)),
        ("{ss}", format!("{:02}", t.wSecond)),
        ("{ms}", format!("{:03}", t.wMilliseconds)),
        ("{w}", w.to_string()),
        ("{h}", h.to_string()),
    ] {
        s = s.replace(k, &v);
    }
    let s: String = s
        .chars()
        .map(|c| {
            if "<>:\"/\\|?*".contains(c) || (c as u32) < 32 {
                '_'
            } else {
                c
            }
        })
        .collect();
    if s.trim().is_empty() {
        "Refract".into()
    } else {
        s
    }
}

/// A path that does not exist yet: name.ext, name (2).ext, ...
pub fn unique_path(dir: &std::path::Path, name: &str, ext: &str) -> PathBuf {
    let first = dir.join(format!("{name}.{ext}"));
    if !first.exists() {
        return first;
    }
    for i in 2..10000 {
        let p = dir.join(format!("{name} ({i}).{ext}"));
        if !p.exists() {
            return p;
        }
    }
    first
}

pub fn shell_open(target: &str) {
    let t = W::new(target);
    let op = W::new("open");
    unsafe {
        ShellExecuteW(
            None,
            op.p(),
            t.p(),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn shell_select(path: &std::path::Path) {
    let args = W::new(&format!("/select,\"{}\"", path.display()));
    let exe = W::new("explorer.exe");
    let op = W::new("open");
    unsafe {
        ShellExecuteW(
            None,
            op.p(),
            exe.p(),
            args.p(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}

pub fn message_box(text: &str, flags: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let t = W::new(text);
    let c = W::new(APP_NAME);
    unsafe { MessageBoxW(None, t.p(), c.p(), flags | MB_SETFOREGROUND | MB_TOPMOST) }
}

/// Finds a top-level window of ours by class name. EnumWindows is used because
/// FindWindow can miss windows in some sandboxed / mixed-integrity setups.
pub fn find_window(class: &str) -> Option<HWND> {
    struct Ctx {
        want: Vec<u16>,
        found: Option<HWND>,
    }
    unsafe extern "system" fn cb(h: HWND, l: LPARAM) -> BOOL {
        let ctx = &mut *(l.0 as *mut Ctx);
        let mut buf = [0u16; 128];
        let n = GetClassNameW(h, &mut buf) as usize;
        if buf[..n] == ctx.want[..] {
            ctx.found = Some(h);
            return FALSE;
        }
        TRUE
    }
    let t = W::new(class);
    if let Ok(h) = unsafe { FindWindowW(t.p(), PCWSTR::null()) } {
        if !h.0.is_null() {
            return Some(h);
        }
    }
    let mut ctx = Ctx {
        want: class.encode_utf16().collect(),
        found: None,
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.found
}

/// Appends a line to %APPDATA%\Refract-screener\log.txt (kept under 256 KB).
pub fn log(msg: &str) {
    use std::io::Write;
    let dir = crate::config::app_dir();
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("log.txt");
    if std::fs::metadata(&path)
        .map(|m| m.len() > 256 * 1024)
        .unwrap_or(false)
    {
        let _ = std::fs::rename(&path, dir.join("log.old.txt"));
    }
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
    {
        let t = unsafe { windows::Win32::System::SystemInformation::GetLocalTime() };
        let _ = writeln!(
            f,
            "{:04}-{:02}-{:02} {:02}:{:02}:{:02}.{:03} [{}] {msg}",
            t.wYear,
            t.wMonth,
            t.wDay,
            t.wHour,
            t.wMinute,
            t.wSecond,
            t.wMilliseconds,
            std::process::id()
        );
    }
}
