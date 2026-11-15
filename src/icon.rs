//! The Refract logo. The EXE carries it as icon resource 1 (assets/refract.ico);
//! notifications and the utility windows use the 512 px PNG (assets/mark-512.png).

use windows::core::PCWSTR;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The logo as a PNG (for the notification app icon).
pub static MARK_PNG: &[u8] = include_bytes!("../assets/mark-512.png");

/// The logo at `size` px, picked by Windows from the multi-size icon resource.
pub fn make(size: i32) -> HICON {
    unsafe {
        let inst = GetModuleHandleW(None).unwrap_or_default();
        LoadImageW(
            inst,
            PCWSTR(1 as _),
            IMAGE_ICON,
            size,
            size,
            LR_DEFAULTCOLOR,
        )
        .map(|h| HICON(h.0))
        .unwrap_or_default()
    }
}
