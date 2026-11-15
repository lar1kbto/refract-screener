//! Global hotkeys through a low-level keyboard hook on its own thread.
//! A hook (unlike RegisterHotKey) sees Win+Shift+S and PrintScreen before the
//! shell does, so they can be taken away from Snipping Tool.

use crate::win::*;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Region = 1,
    Fullscreen = 2,
    Window = 3,
}

impl Action {
    pub fn from(v: usize) -> Option<Action> {
        match v {
            1 => Some(Action::Region),
            2 => Some(Action::Fullscreen),
            3 => Some(Action::Window),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Combo {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
    pub win: bool,
    pub vk: u32,
}

pub fn parse(s: &str) -> Option<Combo> {
    let mut c = Combo {
        ctrl: false,
        alt: false,
        shift: false,
        win: false,
        vk: 0,
    };
    for part in s.split('+').map(|p| p.trim().to_ascii_lowercase()) {
        match part.as_str() {
            "ctrl" | "control" => c.ctrl = true,
            "alt" => c.alt = true,
            "shift" => c.shift = true,
            "win" | "windows" | "super" => c.win = true,
            k => c.vk = key_code(k)?,
        }
    }
    (c.vk != 0).then_some(c)
}

fn key_code(k: &str) -> Option<u32> {
    let v = match k {
        "printscreen" | "prtsc" | "print" | "snapshot" => VK_SNAPSHOT.0,
        "pause" => VK_PAUSE.0,
        "scrolllock" => VK_SCROLL.0,
        "insert" => VK_INSERT.0,
        "home" => VK_HOME.0,
        "end" => VK_END.0,
        "pageup" => VK_PRIOR.0,
        "pagedown" => VK_NEXT.0,
        "space" => VK_SPACE.0,
        "tab" => VK_TAB.0,
        _ => {
            if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
                if (1..=24).contains(&n) {
                    return Some((VK_F1.0 + n - 1) as u32);
                }
            }
            let ch: Vec<char> = k.chars().collect();
            if ch.len() == 1 && ch[0].is_ascii_alphanumeric() {
                return Some(ch[0].to_ascii_uppercase() as u32);
            }
            return None;
        }
    };
    Some(v as u32)
}

struct Table {
    combos: Vec<(Combo, Action)>,
    paused: bool,
}

static TABLE: Mutex<Table> = Mutex::new(Table {
    combos: Vec::new(),
    paused: false,
});
static TARGET: AtomicIsize = AtomicIsize::new(0);
/// VK whose key-up must be swallowed because we swallowed its key-down.
static SWALLOWED: AtomicIsize = AtomicIsize::new(0);
const OUR_INPUT: usize = 0x52_46_52_43; // "RFRC"

pub fn set_combos(list: Vec<(Combo, Action)>) {
    TABLE.lock().unwrap().combos = list;
}

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if k.dwExtraInfo != OUR_INPUT {
            let msg = wp.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            if is_up && SWALLOWED.load(Ordering::Relaxed) == k.vkCode as isize {
                SWALLOWED.store(0, Ordering::Relaxed);
                return LRESULT(1);
            }
            if is_down {
                let hit = {
                    let t = TABLE.lock().unwrap();
                    if t.paused {
                        None
                    } else {
                        let now = Combo {
                            ctrl: down(VK_CONTROL),
                            alt: down(VK_MENU),
                            shift: down(VK_SHIFT),
                            win: down(VK_LWIN) || down(VK_RWIN),
                            vk: k.vkCode,
                        };
                        t.combos.iter().find(|(c, _)| *c == now).copied()
                    }
                };
                if let Some((c, action)) = hit {
                    SWALLOWED.store(k.vkCode as isize, Ordering::Relaxed);
                    if c.win || c.alt {
                        // A harmless key between Win/Alt down and up keeps the Start
                        // menu / menu bar from opening when the modifier is released.
                        mask_key();
                    }
                    let target = hwnd_from(TARGET.load(Ordering::Relaxed));
                    let _ = PostMessageW(target, WM_APP_HOTKEY, WPARAM(action as usize), LPARAM(0));
                    return LRESULT(1);
                }
            }
        }
    }
    CallNextHookEx(None, code, wp, lp)
}

unsafe fn mask_key() {
    let key = |flags: KEYBD_EVENT_FLAGS| INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0xE8),
                wScan: 0,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: OUR_INPUT,
            },
        },
    };
    let inputs = [key(KEYBD_EVENT_FLAGS(0)), key(KEYEVENTF_KEYUP)];
    SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
}

/// Starts the hook thread. Hotkey presses are posted to `target` as WM_APP_HOTKEY.
pub fn start(target: HWND) {
    TARGET.store(hwnd_to(target), Ordering::Relaxed);
    std::thread::Builder::new()
        .name("hotkeys".into())
        .spawn(|| unsafe {
            let h = SetWindowsHookExW(WH_KEYBOARD_LL, Some(hook), None, 0);
            if h.is_err() {
                return;
            }
            let mut msg = MSG::default();
            while GetMessageW(&mut msg, None, 0, 0).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        })
        .ok();
}

fn is_snipping_combo(c: &Combo) -> bool {
    let prtsc = c.vk == VK_SNAPSHOT.0 as u32 && !c.ctrl && !c.alt && !c.shift && !c.win;
    let win_shift_s = c.vk == 'S' as u32 && c.win && c.shift && !c.ctrl && !c.alt;
    prtsc || win_shift_s
}

/// Hotkeys from the settings. With "Replace Snipping Tool" off, PrintScreen and
/// Win+Shift+S are left to Windows.
pub fn combos(cfg: &crate::config::Config) -> Vec<(Combo, Action)> {
    let mut list = Vec::new();
    for (keys, action) in [
        (&cfg.hotkey_region, Action::Region),
        (&cfg.hotkey_fullscreen, Action::Fullscreen),
        (&cfg.hotkey_window, Action::Window),
    ] {
        for k in keys {
            if let Some(c) = parse(k) {
                if cfg.replace_snipping_tool || !is_snipping_combo(&c) {
                    list.push((c, action));
                }
            }
        }
    }
    list
}
