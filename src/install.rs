//! Per-user install (no admin rights): copies the EXE to
//! %LOCALAPPDATA%\Programs\Refract-screener, starts it with Windows, takes the
//! PrintScreen key from Snipping Tool, adds a Start menu shortcut and an entry in
//! Settings > Apps. Uninstall restores everything it changed.

use crate::win::*;
use std::path::PathBuf;
use windows::core::{Interface, PCWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::System::Com::*;
use windows::Win32::System::Registry::*;
use windows::Win32::UI::Shell::*;
use windows::Win32::UI::WindowsAndMessaging::*;

const RUN_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
const UNINSTALL_KEY: &str = r"Software\Microsoft\Windows\CurrentVersion\Uninstall\Refract-screener";
const KEYBOARD_KEY: &str = r"Control Panel\Keyboard";
const PRTSC_VALUE: &str = "PrintScreenKeyForSnippingEnabled";
const OUR_KEY: &str = r"Software\Refract-screener";

pub fn install_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Programs")
        .join("Refract-screener")
}

pub fn installed_exe() -> PathBuf {
    install_dir().join("refract-screener.exe")
}

pub fn is_installed_copy() -> bool {
    let a = exe_path().canonicalize().ok();
    let b = installed_exe().canonicalize().ok();
    a.is_some() && a == b
}

fn start_menu_link() -> Option<PathBuf> {
    known_folder(&FOLDERID_Programs).map(|p| p.join("Refract-screener.lnk"))
}

// ---------- registry ----------

fn open(root: HKEY, path: &str, create: bool) -> Option<HKEY> {
    let p = W::new(path);
    let mut k = HKEY::default();
    unsafe {
        let r = if create {
            RegCreateKeyExW(
                root,
                p.p(),
                0,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_READ | KEY_WRITE,
                None,
                &mut k,
                None,
            )
        } else {
            RegOpenKeyExW(root, p.p(), 0, KEY_READ | KEY_WRITE, &mut k)
        };
        (r == ERROR_SUCCESS).then_some(k)
    }
}

fn set_str(k: HKEY, name: &str, val: &str) {
    let n = W::new(name);
    let v = wide(val);
    let bytes = unsafe { std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2) };
    unsafe {
        let _ = RegSetValueExW(k, n.p(), 0, REG_SZ, Some(bytes));
    }
}

fn set_dword(k: HKEY, name: &str, val: u32) {
    let n = W::new(name);
    unsafe {
        let _ = RegSetValueExW(k, n.p(), 0, REG_DWORD, Some(&val.to_le_bytes()));
    }
}

fn get_dword(k: HKEY, name: &str) -> Option<u32> {
    let n = W::new(name);
    let mut data = [0u8; 4];
    let mut len = 4u32;
    let mut ty = REG_VALUE_TYPE::default();
    unsafe {
        let r = RegQueryValueExW(
            k,
            n.p(),
            None,
            Some(&mut ty),
            Some(data.as_mut_ptr()),
            Some(&mut len),
        );
        (r == ERROR_SUCCESS && ty == REG_DWORD).then(|| u32::from_le_bytes(data))
    }
}

fn get_str(k: HKEY, name: &str) -> Option<String> {
    let n = W::new(name);
    let mut buf = vec![0u16; 2048];
    let mut len = (buf.len() * 2) as u32;
    unsafe {
        let r = RegQueryValueExW(
            k,
            n.p(),
            None,
            None,
            Some(buf.as_mut_ptr() as *mut u8),
            Some(&mut len),
        );
        if r != ERROR_SUCCESS {
            return None;
        }
    }
    let n = (len as usize / 2).min(buf.len());
    Some(
        String::from_utf16_lossy(&buf[..n])
            .trim_end_matches('\0')
            .to_string(),
    )
}

fn del(k: HKEY, name: &str) {
    let n = W::new(name);
    unsafe {
        let _ = RegDeleteValueW(k, n.p());
    }
}

fn close(k: HKEY) {
    unsafe {
        let _ = RegCloseKey(k);
    }
}

// ---------- autostart ----------

fn run_command(exe: &std::path::Path) -> String {
    format!("\"{}\" --background", exe.display())
}

pub fn autostart_enabled() -> bool {
    open(HKEY_CURRENT_USER, RUN_KEY, false)
        .map(|k| {
            let v = get_str(k, APP_NAME);
            close(k);
            v.is_some()
        })
        .unwrap_or(false)
}

/// Points autostart at the running EXE (installed copy or portable).
pub fn set_autostart(on: bool) {
    if let Some(k) = open(HKEY_CURRENT_USER, RUN_KEY, true) {
        if on {
            set_str(k, APP_NAME, &run_command(&exe_path()));
        } else {
            del(k, APP_NAME);
        }
        close(k);
    }
}

// ---------- PrintScreen key ----------

pub fn prtsc_taken() -> bool {
    open(HKEY_CURRENT_USER, KEYBOARD_KEY, false)
        .map(|k| {
            let v = get_dword(k, PRTSC_VALUE);
            close(k);
            v == Some(0)
        })
        .unwrap_or(false)
}

/// Turns off "Use the Print screen key to open screen capture" so Windows does not
/// start Snipping Tool, remembering the previous value for uninstall.
pub fn take_prtsc() {
    let Some(kb) = open(HKEY_CURRENT_USER, KEYBOARD_KEY, true) else {
        return;
    };
    let old = get_dword(kb, PRTSC_VALUE);
    if let Some(ours) = open(HKEY_CURRENT_USER, OUR_KEY, true) {
        if get_str(ours, "PrtScBackup").is_none() {
            set_str(
                ours,
                "PrtScBackup",
                &old.map(|v| v.to_string())
                    .unwrap_or_else(|| "absent".into()),
            );
        }
        close(ours);
    }
    set_dword(kb, PRTSC_VALUE, 0);
    close(kb);
}

pub fn restore_prtsc() {
    let backup = open(HKEY_CURRENT_USER, OUR_KEY, false).and_then(|k| {
        let v = get_str(k, "PrtScBackup");
        if v.is_some() {
            del(k, "PrtScBackup");
        }
        close(k);
        v
    });
    if let Some(kb) = open(HKEY_CURRENT_USER, KEYBOARD_KEY, true) {
        match backup.as_deref() {
            Some("absent") => del(kb, PRTSC_VALUE),
            Some(v) => set_dword(kb, PRTSC_VALUE, v.parse().unwrap_or(1)),
            None => set_dword(kb, PRTSC_VALUE, 1),
        }
        close(kb);
    }
}

// ---------- shortcut ----------

fn make_shortcut(
    link: &std::path::Path,
    target: &std::path::Path,
    args: &str,
) -> windows::core::Result<()> {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let sl: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)?;
        let t = W::new(&target.display().to_string());
        let a = W::new(args);
        let d = W::new("Screenshots, annotations, text recognition and translation");
        let wd = W::new(
            &target
                .parent()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
        );
        sl.SetPath(t.p())?;
        sl.SetArguments(a.p())?;
        sl.SetDescription(d.p())?;
        sl.SetWorkingDirectory(wd.p())?;
        // AppUserModelID on the shortcut: Windows 10 shows notifications of
        // unpackaged apps only for an app with such a Start menu shortcut.
        {
            use windows::Win32::Storage::EnhancedStorage::PKEY_AppUserModel_ID;
            use windows::Win32::System::Com::StructuredStorage::{
                PropVariantChangeType, PROPVAR_CHANGE_FLAGS,
            };
            use windows::Win32::System::Variant::VT_LPWSTR;
            use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;
            let store: IPropertyStore = sl.cast()?;
            let src = windows::core::PROPVARIANT::from(crate::toast::AUMID);
            let mut id = windows::core::PROPVARIANT::default();
            PropVariantChangeType(&mut id, &src, PROPVAR_CHANGE_FLAGS(0), VT_LPWSTR)?;
            store.SetValue(&PKEY_AppUserModel_ID, &id)?;
            store.Commit()?;
        }
        let pf: IPersistFile = sl.cast()?;
        let l = W::new(&link.display().to_string());
        pf.Save(l.p(), true)?;
    }
    Ok(())
}

// ---------- running instance ----------

/// Asks a running instance to quit and waits for it.
pub fn stop_running() {
    let class = W::new(MAIN_CLASS);
    for _ in 0..50 {
        let _ = &class;
        match find_window(MAIN_CLASS) {
            Some(h) => unsafe {
                let _ = PostMessageW(h, WM_CLOSE, WPARAM(0), LPARAM(0));
                std::thread::sleep(std::time::Duration::from_millis(100));
            },
            _ => {
                // Give the old process a moment to release the EXE file.
                std::thread::sleep(std::time::Duration::from_millis(250));
                return;
            }
        }
    }
}

// ---------- install / uninstall ----------

pub fn install() -> Result<PathBuf, String> {
    let src = exe_path();
    let dir = install_dir();
    let dst = installed_exe();
    std::fs::create_dir_all(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    if !is_installed_copy() {
        crate::guard::stop();
        stop_running();
        let mut last = None;
        for _ in 0..20 {
            match std::fs::copy(&src, &dst) {
                Ok(_) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    std::thread::sleep(std::time::Duration::from_millis(200));
                }
            }
        }
        if let Some(e) = last {
            return Err(format!("Cannot copy to {}: {e}", dst.display()));
        }
    }

    if let Some(k) = open(HKEY_CURRENT_USER, RUN_KEY, true) {
        set_str(k, APP_NAME, &run_command(&dst));
        close(k);
    }
    take_prtsc();
    if let Some(link) = start_menu_link() {
        let _ = make_shortcut(&link, &dst, "");
    }
    if let Some(k) = open(HKEY_CURRENT_USER, UNINSTALL_KEY, true) {
        set_str(k, "DisplayName", "Refract-screener");
        set_str(k, "DisplayVersion", env!("CARGO_PKG_VERSION"));
        set_str(k, "Publisher", "Refract");
        set_str(k, "DisplayIcon", &format!("\"{}\"", dst.display()));
        set_str(k, "InstallLocation", &dir.display().to_string());
        set_str(
            k,
            "UninstallString",
            &format!("\"{}\" --uninstall", dst.display()),
        );
        set_str(
            k,
            "QuietUninstallString",
            &format!("\"{}\" --uninstall --quiet", dst.display()),
        );
        set_dword(k, "NoModify", 1);
        set_dword(k, "NoRepair", 1);
        let size = std::fs::metadata(&dst)
            .map(|m| (m.len() / 1024) as u32)
            .unwrap_or(0);
        set_dword(k, "EstimatedSize", size);
        close(k);
    }
    Ok(dst)
}

pub fn uninstall() {
    crate::guard::stop();
    stop_running();
    if let Some(k) = open(HKEY_CURRENT_USER, RUN_KEY, false) {
        del(k, APP_NAME);
        close(k);
    }
    restore_prtsc();
    if let Some(link) = start_menu_link() {
        let _ = std::fs::remove_file(link);
    }
    unsafe {
        let p = W::new(UNINSTALL_KEY);
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, p.p());
        let o = W::new(OUR_KEY);
        let _ = RegDeleteTreeW(HKEY_CURRENT_USER, o.p());
    }
    // The running EXE cannot delete itself: a detached cmd removes the folder
    // once this process has exited. Settings and screenshots are kept.
    let dir = install_dir();
    if dir.exists() {
        use std::os::windows::process::CommandExt;
        let _ = std::process::Command::new("cmd.exe")
            .raw_arg(format!(
                "/C ping 127.0.0.1 -n 3 >nul & rmdir /S /Q \"{}\"",
                dir.display()
            ))
            .creation_flags(0x0800_0000 | 0x0000_0008) // CREATE_NO_WINDOW | DETACHED_PROCESS
            .spawn();
    }
}

pub fn launch(exe: &std::path::Path, args: &[&str]) {
    use std::os::windows::process::CommandExt;
    let _ = std::process::Command::new(exe)
        .args(args)
        .creation_flags(0x0000_0008)
        .spawn();
}

/// Starts `exe` once this process has exited (it would otherwise meet our
/// single-instance mutex and just hand the request back to us).
pub fn launch_after_exit(exe: &std::path::Path, args: &[&str]) {
    let mut all: Vec<String> = args.iter().map(|s| s.to_string()).collect();
    all.push("--wait-pid".into());
    all.push(std::process::id().to_string());
    let refs: Vec<&str> = all.iter().map(|s| s.as_str()).collect();
    launch(exe, &refs);
}

/// Keeps the Settings > Apps entry and the Start menu shortcut in sync with the
/// running installed copy (the version changes with every auto-update). User
/// choices (autostart, PrintScreen) are not touched.
pub fn refresh_registration() {
    if !is_installed_copy() {
        return;
    }
    let dst = installed_exe();
    if let Some(k) = open(HKEY_CURRENT_USER, UNINSTALL_KEY, true) {
        set_str(k, "DisplayName", "Refract-screener");
        set_str(k, "DisplayVersion", env!("CARGO_PKG_VERSION"));
        set_str(k, "Publisher", "Refract");
        set_str(k, "DisplayIcon", &format!("\"{}\"", dst.display()));
        set_str(k, "InstallLocation", &install_dir().display().to_string());
        set_str(
            k,
            "UninstallString",
            &format!("\"{}\" --uninstall", dst.display()),
        );
        set_str(
            k,
            "QuietUninstallString",
            &format!("\"{}\" --uninstall --quiet", dst.display()),
        );
        set_dword(k, "NoModify", 1);
        set_dword(k, "NoRepair", 1);
        let size = std::fs::metadata(&dst)
            .map(|m| (m.len() / 1024) as u32)
            .unwrap_or(0);
        set_dword(k, "EstimatedSize", size);
        close(k);
    }
    if let Some(link) = start_menu_link() {
        if !link.exists() {
            let _ = make_shortcut(&link, &dst, "");
        }
    }
}
