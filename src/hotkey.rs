//! Global hotkeys through a low-level keyboard hook on its own thread.
//! A hook (unlike RegisterHotKey) sees Win+Shift+S and PrintScreen before the
//! shell does, so they can be taken away from Snipping Tool.
//!
//! The hook also has two short-lived modes used by the UI:
//! - recording: the next shortcut (any keys, Win and PrintScreen included) is
//!   reported to a window instead of reaching Windows or running an action;
//! - Esc catcher: while a scrolling capture runs, Esc is reported to a window,
//!   whichever app has the keyboard focus.

use crate::win::*;
use std::sync::atomic::{AtomicIsize, AtomicU32, Ordering};
use std::sync::Mutex;
use windows::Win32::Foundation::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Region = 1,
    Fullscreen = 2,
    Window = 3,
    Scroll = 4,
}

impl Action {
    pub const ALL: [Action; 4] = [
        Action::Region,
        Action::Fullscreen,
        Action::Window,
        Action::Scroll,
    ];
    pub fn from(v: usize) -> Option<Action> {
        match v {
            1 => Some(Action::Region),
            2 => Some(Action::Fullscreen),
            3 => Some(Action::Window),
            4 => Some(Action::Scroll),
            _ => None,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Action::Region => "Region",
            Action::Fullscreen => "Full screen",
            Action::Window => "Window",
            Action::Scroll => "Scrolling capture",
        }
    }
    /// The config.json list for this action.
    pub fn keys(self, cfg: &crate::config::Config) -> &Vec<String> {
        match self {
            Action::Region => &cfg.hotkey_region,
            Action::Fullscreen => &cfg.hotkey_fullscreen,
            Action::Window => &cfg.hotkey_window,
            Action::Scroll => &cfg.hotkey_scroll,
        }
    }
    pub fn keys_mut(self, cfg: &mut crate::config::Config) -> &mut Vec<String> {
        match self {
            Action::Region => &mut cfg.hotkey_region,
            Action::Fullscreen => &mut cfg.hotkey_fullscreen,
            Action::Window => &mut cfg.hotkey_window,
            Action::Scroll => &mut cfg.hotkey_scroll,
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

impl Combo {
    /// Packs the combo into a message parameter.
    pub fn pack(self) -> usize {
        self.vk as usize
            | (self.ctrl as usize) << 16
            | (self.alt as usize) << 17
            | (self.shift as usize) << 18
            | (self.win as usize) << 19
    }
    pub fn unpack(v: usize) -> Combo {
        Combo {
            vk: (v & 0xFFFF) as u32,
            ctrl: v & (1 << 16) != 0,
            alt: v & (1 << 17) != 0,
            shift: v & (1 << 18) != 0,
            win: v & (1 << 19) != 0,
        }
    }
    fn mods(self) -> Vec<&'static str> {
        // Windows' own order: Win+Shift+S, Ctrl+Alt+Del, Ctrl+Shift+Esc.
        let mut m = Vec::new();
        if self.win {
            m.push("Win");
        }
        if self.ctrl {
            m.push("Ctrl");
        }
        if self.alt {
            m.push("Alt");
        }
        if self.shift {
            m.push("Shift");
        }
        m
    }
    /// The config.json spelling, e.g. "Ctrl+Shift+PrintScreen"; `parse` reads it back.
    pub fn text(self) -> String {
        let mut m = self.mods();
        let key = key_name(self.vk);
        m.push(&key);
        m.join("+")
    }
    /// Short form for the UI ("PrtSc" instead of "PrintScreen").
    pub fn label(self) -> String {
        let mut m = self.mods();
        let key = match self.vk {
            v if v == VK_SNAPSHOT.0 as u32 => "PrtSc".to_string(),
            v => key_name(v),
        };
        m.push(&key);
        m.join("+")
    }
    pub fn has_mods(self) -> bool {
        self.ctrl || self.alt || self.shift || self.win
    }
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

/// Named keys: (config name, virtual key). Letters, digits and F1..F24 are handled
/// separately. Names avoid "+", which separates the parts of a shortcut.
const NAMED: [(&str, u16); 42] = [
    ("PrintScreen", VK_SNAPSHOT.0),
    ("Pause", VK_PAUSE.0),
    ("ScrollLock", VK_SCROLL.0),
    ("Insert", VK_INSERT.0),
    ("Delete", VK_DELETE.0),
    ("Home", VK_HOME.0),
    ("End", VK_END.0),
    ("PageUp", VK_PRIOR.0),
    ("PageDown", VK_NEXT.0),
    ("Up", VK_UP.0),
    ("Down", VK_DOWN.0),
    ("Left", VK_LEFT.0),
    ("Right", VK_RIGHT.0),
    ("Space", VK_SPACE.0),
    ("Tab", VK_TAB.0),
    ("Enter", VK_RETURN.0),
    ("Backspace", VK_BACK.0),
    ("Escape", VK_ESCAPE.0),
    ("Minus", VK_OEM_MINUS.0),
    ("Equals", VK_OEM_PLUS.0),
    ("Comma", VK_OEM_COMMA.0),
    ("Period", VK_OEM_PERIOD.0),
    ("Slash", VK_OEM_2.0),
    ("Backquote", VK_OEM_3.0),
    ("BracketLeft", VK_OEM_4.0),
    ("Backslash", VK_OEM_5.0),
    ("BracketRight", VK_OEM_6.0),
    ("Semicolon", VK_OEM_1.0),
    ("Quote", VK_OEM_7.0),
    ("Num0", VK_NUMPAD0.0),
    ("Num1", VK_NUMPAD1.0),
    ("Num2", VK_NUMPAD2.0),
    ("Num3", VK_NUMPAD3.0),
    ("Num4", VK_NUMPAD4.0),
    ("Num5", VK_NUMPAD5.0),
    ("Num6", VK_NUMPAD6.0),
    ("Num7", VK_NUMPAD7.0),
    ("Num8", VK_NUMPAD8.0),
    ("Num9", VK_NUMPAD9.0),
    ("NumMultiply", VK_MULTIPLY.0),
    ("NumAdd", VK_ADD.0),
    ("NumSubtract", VK_SUBTRACT.0),
];

fn key_code(k: &str) -> Option<u32> {
    match k {
        "prtsc" | "print" | "snapshot" => return Some(VK_SNAPSHOT.0 as u32),
        "pgup" => return Some(VK_PRIOR.0 as u32),
        "pgdn" => return Some(VK_NEXT.0 as u32),
        "del" => return Some(VK_DELETE.0 as u32),
        "ins" => return Some(VK_INSERT.0 as u32),
        "esc" => return Some(VK_ESCAPE.0 as u32),
        "numdivide" => return Some(VK_DIVIDE.0 as u32),
        "numdecimal" => return Some(VK_DECIMAL.0 as u32),
        _ => {}
    }
    if let Some((_, vk)) = NAMED.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)) {
        return Some(*vk as u32);
    }
    if let Some(n) = k.strip_prefix('f').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=24).contains(&n) {
            return Some((VK_F1.0 + n - 1) as u32);
        }
    }
    let ch: Vec<char> = k.chars().collect();
    if ch.len() == 1 && ch[0].is_ascii_alphanumeric() {
        return Some(ch[0].to_ascii_uppercase() as u32);
    }
    None
}

/// Name of a virtual key, empty when the key cannot be part of a shortcut.
pub fn key_name(vk: u32) -> String {
    if let Some((n, _)) = NAMED.iter().find(|(_, v)| *v as u32 == vk) {
        return n.to_string();
    }
    if vk == VK_DIVIDE.0 as u32 {
        return "NumDivide".into();
    }
    if vk == VK_DECIMAL.0 as u32 {
        return "NumDecimal".into();
    }
    if (VK_F1.0 as u32..=VK_F24.0 as u32).contains(&vk) {
        return format!("F{}", vk - VK_F1.0 as u32 + 1);
    }
    match char::from_u32(vk) {
        Some(c) if c.is_ascii_uppercase() || c.is_ascii_digit() => c.to_string(),
        _ => String::new(),
    }
}

fn is_modifier(vk: u32) -> bool {
    [
        VK_SHIFT,
        VK_CONTROL,
        VK_MENU,
        VK_LSHIFT,
        VK_RSHIFT,
        VK_LCONTROL,
        VK_RCONTROL,
        VK_LMENU,
        VK_RMENU,
        VK_LWIN,
        VK_RWIN,
    ]
    .iter()
    .any(|k| k.0 as u32 == vk)
}

/// Why a recorded shortcut cannot be used, or None when it is fine.
pub fn problem(c: &Combo) -> Option<String> {
    if key_name(c.vk).is_empty() {
        return Some("This key cannot be used in a shortcut".into());
    }
    let standalone = c.vk == VK_SNAPSHOT.0 as u32
        || c.vk == VK_PAUSE.0 as u32
        || c.vk == VK_SCROLL.0 as u32
        || (VK_F1.0 as u32..=VK_F24.0 as u32).contains(&c.vk);
    if !(c.ctrl || c.alt || c.win) && !standalone {
        return Some(format!(
            "{} alone would stop it from typing: add Ctrl, Alt or Win",
            c.label()
        ));
    }
    let only_ctrl = c.ctrl && !c.alt && !c.shift && !c.win;
    let only_alt = c.alt && !c.ctrl && !c.shift && !c.win;
    let taken = (only_ctrl && "ACVXYZSPFNWT".contains(char::from_u32(c.vk).unwrap_or(' ')))
        || (only_alt && (c.vk == VK_TAB.0 as u32 || c.vk == VK_F4.0 as u32))
        || (c.win && !c.ctrl && !c.alt && !c.shift && c.vk == 'L' as u32);
    if taken {
        return Some(format!("{} belongs to Windows and other apps", c.label()));
    }
    None
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
/// VK whose key-up (and auto-repeat) must be swallowed because we swallowed its key-down.
static SWALLOWED: AtomicIsize = AtomicIsize::new(0);
/// Window that receives recorded shortcuts (0 = not recording).
static RECORD: AtomicIsize = AtomicIsize::new(0);
/// Window that receives Esc presses (0 = off).
static ESCAPE: AtomicIsize = AtomicIsize::new(0);
/// Recording: modifier keys held (bit per modifier) and the ones whose key-down we
/// swallowed, so that their key-up is swallowed too and Windows never sees half a press.
static HELD: AtomicU32 = AtomicU32::new(0);
static OURS: AtomicU32 = AtomicU32::new(0);
const OUR_INPUT: usize = 0x52_46_52_43; // "RFRC"

/// Recording -> window: a shortcut was pressed (wparam = Combo::pack).
pub const WM_APP_KEY_RECORDED: u32 = WM_APP + 40;
/// Recording -> window: modifiers held changed (wparam = Combo::pack with vk 0).
pub const WM_APP_KEY_MODS: u32 = WM_APP + 41;
/// Esc catcher -> window: Esc was pressed.
pub const WM_APP_KEY_ESCAPE: u32 = WM_APP + 42;

const MOD_KEYS: [(VIRTUAL_KEY, u32); 8] = [
    (VK_LCONTROL, 1),
    (VK_RCONTROL, 2),
    (VK_LMENU, 4),
    (VK_RMENU, 8),
    (VK_LSHIFT, 16),
    (VK_RSHIFT, 32),
    (VK_LWIN, 64),
    (VK_RWIN, 128),
];

fn mod_bit(vk: u32) -> u32 {
    MOD_KEYS
        .iter()
        .find(|(k, _)| k.0 as u32 == vk)
        .map(|(_, b)| *b)
        .unwrap_or(0)
}

fn held_combo(vk: u32) -> Combo {
    let h = HELD.load(Ordering::Relaxed);
    Combo {
        ctrl: h & 3 != 0,
        alt: h & 12 != 0,
        shift: h & 48 != 0,
        win: h & 192 != 0,
        vk,
    }
}

pub fn set_combos(list: Vec<(Combo, Action)>) {
    TABLE.lock().unwrap().combos = list;
}

/// Stops (or resumes) acting on hotkeys; keys then go to Windows as usual.
pub fn set_paused(paused: bool) {
    TABLE.lock().unwrap().paused = paused;
}

/// Starts recording: the next shortcut goes to `to` as WM_APP_KEY_RECORDED.
pub fn record_start(to: HWND) {
    let mut held = 0;
    for (k, b) in MOD_KEYS {
        if down(k) {
            held |= b;
        }
    }
    HELD.store(held, Ordering::Relaxed);
    RECORD.store(hwnd_to(to), Ordering::Relaxed);
}

pub fn record_stop() {
    RECORD.store(0, Ordering::Relaxed);
}

/// Reports Esc presses to `to` (None stops it).
pub fn catch_escape(to: Option<HWND>) {
    ESCAPE.store(to.map(hwnd_to).unwrap_or(0), Ordering::Relaxed);
}

fn down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

/// Recording mode: every key is ours. Returns true when the event is swallowed.
unsafe fn record(to: HWND, vk: u32, is_down: bool) -> bool {
    let bit = mod_bit(vk);
    if bit != 0 {
        let held = HELD.load(Ordering::Relaxed);
        if is_down {
            if held & bit == 0 {
                HELD.store(held | bit, Ordering::Relaxed);
                OURS.fetch_or(bit, Ordering::Relaxed);
                let _ = PostMessageW(to, WM_APP_KEY_MODS, WPARAM(held_combo(0).pack()), LPARAM(0));
            }
            // A modifier held before recording started was seen by Windows: let
            // its auto-repeat through, swallow ours.
            return OURS.load(Ordering::Relaxed) & bit != 0;
        }
        HELD.store(held & !bit, Ordering::Relaxed);
        let _ = PostMessageW(to, WM_APP_KEY_MODS, WPARAM(held_combo(0).pack()), LPARAM(0));
        return OURS.fetch_and(!bit, Ordering::Relaxed) & bit != 0;
    }
    if is_modifier(vk) {
        return false;
    }
    if is_down {
        SWALLOWED.store(vk as isize, Ordering::Relaxed);
        let _ = PostMessageW(
            to,
            WM_APP_KEY_RECORDED,
            WPARAM(held_combo(vk).pack()),
            LPARAM(0),
        );
        return true;
    }
    false
}

unsafe extern "system" fn hook(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 {
        let k = &*(lp.0 as *const KBDLLHOOKSTRUCT);
        if k.dwExtraInfo != OUR_INPUT {
            let msg = wp.0 as u32;
            let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
            let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
            let vk = k.vkCode;
            if SWALLOWED.load(Ordering::Relaxed) == vk as isize {
                // Auto-repeat and release of a key whose press was ours.
                if is_up {
                    SWALLOWED.store(0, Ordering::Relaxed);
                }
                return LRESULT(1);
            }
            // Key-ups of modifiers swallowed while recording, also after it ended.
            let bit = mod_bit(vk);
            if is_up && bit != 0 && RECORD.load(Ordering::Relaxed) == 0 {
                HELD.fetch_and(!bit, Ordering::Relaxed);
                if OURS.fetch_and(!bit, Ordering::Relaxed) & bit != 0 {
                    return LRESULT(1);
                }
            }
            let rec = RECORD.load(Ordering::Relaxed);
            if rec != 0 && (is_down || is_up) {
                if record(hwnd_from(rec), vk, is_down) {
                    return LRESULT(1);
                }
                return CallNextHookEx(None, code, wp, lp);
            }
            let esc = ESCAPE.load(Ordering::Relaxed);
            if esc != 0 && is_down && vk == VK_ESCAPE.0 as u32 {
                SWALLOWED.store(vk as isize, Ordering::Relaxed);
                let _ = PostMessageW(hwnd_from(esc), WM_APP_KEY_ESCAPE, WPARAM(0), LPARAM(0));
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
                            vk,
                        };
                        t.combos.iter().find(|(c, _)| *c == now).copied()
                    }
                };
                if let Some((c, action)) = hit {
                    SWALLOWED.store(vk as isize, Ordering::Relaxed);
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

/// PrintScreen and Win+Shift+S: Snipping Tool's keys, left to Windows when
/// "Replace Snipping Tool" is off.
pub fn is_snipping_combo(c: &Combo) -> bool {
    let prtsc = c.vk == VK_SNAPSHOT.0 as u32 && !c.ctrl && !c.alt && !c.shift && !c.win;
    let win_shift_s = c.vk == 'S' as u32 && c.win && c.shift && !c.ctrl && !c.alt;
    prtsc || win_shift_s
}

/// Hotkeys from the settings. With "Replace Snipping Tool" off, PrintScreen and
/// Win+Shift+S are left to Windows.
pub fn combos(cfg: &crate::config::Config) -> Vec<(Combo, Action)> {
    let mut list = Vec::new();
    for action in Action::ALL {
        for k in action.keys(cfg) {
            if let Some(c) = parse(k) {
                if cfg.replace_snipping_tool || !is_snipping_combo(&c) {
                    list.push((c, action));
                }
            }
        }
    }
    list
}

/// First working shortcut of an action, for menus ("Ctrl+PrtSc").
pub fn first_label(cfg: &crate::config::Config, action: Action) -> Option<String> {
    combos(cfg)
        .into_iter()
        .find(|(_, a)| *a == action)
        .map(|(c, _)| c.label())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_round_trip() {
        for s in [
            "PrintScreen",
            "Win+Shift+S",
            "Ctrl+Shift+PrintScreen",
            "Ctrl+Alt+F13",
            "Win+Alt+Num5",
            "Ctrl+BracketLeft",
            "Ctrl+Shift+9",
        ] {
            let c = parse(s).unwrap();
            assert_eq!(c.text(), s);
            assert_eq!(Combo::unpack(c.pack()), c);
        }
        assert_eq!(parse("ctrl + prtsc").unwrap().text(), "Ctrl+PrintScreen");
        assert!(problem(&parse("Shift+A").unwrap()).is_some());
        assert!(problem(&parse("Ctrl+C").unwrap()).is_some());
        assert!(problem(&parse("PrintScreen").unwrap()).is_none());
        assert!(problem(&parse("Ctrl+Shift+C").unwrap()).is_none());
    }
}
