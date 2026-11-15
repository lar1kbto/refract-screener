//! Lossless screen capture of the whole virtual desktop at physical resolution.
//! The process is per-monitor DPI aware (v2), so GDI returns real device pixels
//! on every monitor, without any scaling.

use crate::geom::RectI;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Debug)]
pub struct Monitor {
    /// In image coordinates.
    pub rect: RectI,
    pub work: RectI,
    pub scale: f32,
}

pub struct Shot {
    /// Virtual-desktop origin of pixel (0, 0).
    pub ox: i32,
    pub oy: i32,
    pub w: i32,
    pub h: i32,
    /// BGRA, top-down, alpha = 255.
    pub px: Vec<u8>,
    /// Visible top-level windows in Z order (topmost first), image coordinates.
    pub windows: Vec<RectI>,
    pub monitors: Vec<Monitor>,
    /// Foreground window rectangle at capture time.
    pub foreground: Option<RectI>,
}

impl Shot {
    pub fn pixel(&self, x: i32, y: i32) -> Option<(u8, u8, u8)> {
        if x < 0 || y < 0 || x >= self.w || y >= self.h {
            return None;
        }
        let i = ((y * self.w + x) * 4) as usize;
        Some((self.px[i + 2], self.px[i + 1], self.px[i]))
    }

    pub fn crop(&self, r: &RectI) -> Vec<u8> {
        let r = r.clamp_to(&RectI::new(0, 0, self.w, self.h));
        let mut out = Vec::with_capacity((r.w().max(0) * r.h().max(0) * 4) as usize);
        for y in r.t..r.b {
            let s = ((y * self.w + r.l) * 4) as usize;
            out.extend_from_slice(&self.px[s..s + (r.w() * 4) as usize]);
        }
        out
    }

    pub fn monitor_at(&self, x: i32, y: i32) -> Option<&Monitor> {
        self.monitors
            .iter()
            .find(|m| m.rect.contains(x, y))
            .or_else(|| self.monitors.first())
    }

    pub fn full(&self) -> RectI {
        RectI::new(0, 0, self.w, self.h)
    }
}

pub fn grab(cursor: bool, exclude: &[HWND]) -> windows::core::Result<Shot> {
    unsafe {
        let ox = GetSystemMetrics(SM_XVIRTUALSCREEN);
        let oy = GetSystemMetrics(SM_YVIRTUALSCREEN);
        let w = GetSystemMetrics(SM_CXVIRTUALSCREEN).max(1);
        let h = GetSystemMetrics(SM_CYVIRTUALSCREEN).max(1);

        let screen = GetDC(None);
        let mem = CreateCompatibleDC(screen);
        let mut bmi = BITMAPINFO::default();
        bmi.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: w,
            biHeight: -h,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
        let dib = CreateDIBSection(screen, &bmi, DIB_RGB_COLORS, &mut bits, None, 0);
        let dib = match dib {
            Ok(d) => d,
            Err(e) => {
                let _ = DeleteDC(mem);
                ReleaseDC(None, screen);
                return Err(e);
            }
        };
        let old = SelectObject(mem, dib);
        let blt = BitBlt(
            mem,
            0,
            0,
            w,
            h,
            screen,
            ox,
            oy,
            ROP_CODE(SRCCOPY.0 | CAPTUREBLT.0),
        );

        if blt.is_ok() && cursor {
            draw_cursor(mem, ox, oy);
        }

        let n = (w as usize) * (h as usize) * 4;
        let mut px = vec![0u8; n];
        if blt.is_ok() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(bits as *const u8, px.as_mut_ptr(), n);
        }
        for a in px.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }

        SelectObject(mem, old);
        let _ = DeleteObject(dib);
        let _ = DeleteDC(mem);
        ReleaseDC(None, screen);
        blt?;

        let mut shot = Shot {
            ox,
            oy,
            w,
            h,
            px,
            windows: Vec::new(),
            monitors: Vec::new(),
            foreground: None,
        };
        shot.monitors = monitors(ox, oy);
        shot.windows = windows_list(ox, oy, exclude);
        let fg = GetForegroundWindow();
        if !fg.0.is_null() {
            shot.foreground = frame_rect(fg).map(|r| {
                RectI::new(r.left - ox, r.top - oy, r.right - ox, r.bottom - oy)
                    .clamp_to(&RectI::new(0, 0, w, h))
            });
        }
        Ok(shot)
    }
}

unsafe fn draw_cursor(dc: HDC, ox: i32, oy: i32) {
    let mut ci = CURSORINFO {
        cbSize: std::mem::size_of::<CURSORINFO>() as u32,
        ..Default::default()
    };
    if GetCursorInfo(&mut ci).is_err() || (ci.flags.0 & CURSOR_SHOWING.0) == 0 {
        return;
    }
    let icon = HICON(ci.hCursor.0);
    let mut ii = ICONINFO::default();
    if GetIconInfo(icon, &mut ii).is_ok() {
        let x = ci.ptScreenPos.x - ii.xHotspot as i32 - ox;
        let y = ci.ptScreenPos.y - ii.yHotspot as i32 - oy;
        let _ = DrawIconEx(dc, x, y, icon, 0, 0, 0, None, DI_NORMAL);
        if !ii.hbmColor.is_invalid() {
            let _ = DeleteObject(ii.hbmColor);
        }
        if !ii.hbmMask.is_invalid() {
            let _ = DeleteObject(ii.hbmMask);
        }
    }
}

fn monitors(ox: i32, oy: i32) -> Vec<Monitor> {
    unsafe extern "system" fn cb(m: HMONITOR, _: HDC, _: *mut RECT, data: LPARAM) -> BOOL {
        let list = &mut *(data.0 as *mut Vec<(HMONITOR, RECT, RECT)>);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(m, &mut mi).as_bool() {
            list.push((m, mi.rcMonitor, mi.rcWork));
        }
        TRUE
    }
    let mut raw: Vec<(HMONITOR, RECT, RECT)> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(None, None, Some(cb), LPARAM(&mut raw as *mut _ as isize));
    }
    let conv = |r: RECT| RectI::new(r.left - ox, r.top - oy, r.right - ox, r.bottom - oy);
    raw.into_iter()
        .map(|(m, r, wk)| {
            let (mut dx, mut dy) = (96u32, 96u32);
            let scale = unsafe {
                if GetDpiForMonitor(m, MDT_EFFECTIVE_DPI, &mut dx, &mut dy).is_ok() {
                    dx as f32 / 96.0
                } else {
                    1.0
                }
            };
            Monitor {
                rect: conv(r),
                work: conv(wk),
                scale,
            }
        })
        .collect()
}

unsafe fn frame_rect(h: HWND) -> Option<RECT> {
    let mut r = RECT::default();
    if DwmGetWindowAttribute(
        h,
        DWMWA_EXTENDED_FRAME_BOUNDS,
        &mut r as *mut _ as *mut _,
        std::mem::size_of::<RECT>() as u32,
    )
    .is_err()
    {
        GetWindowRect(h, &mut r).ok()?;
    }
    if r.right - r.left < 2 || r.bottom - r.top < 2 {
        None
    } else {
        Some(r)
    }
}

fn windows_list(ox: i32, oy: i32, exclude: &[HWND]) -> Vec<RectI> {
    struct Ctx<'a> {
        out: Vec<RectI>,
        exclude: &'a [HWND],
        ox: i32,
        oy: i32,
    }
    unsafe extern "system" fn cb(h: HWND, data: LPARAM) -> BOOL {
        let ctx = &mut *(data.0 as *mut Ctx);
        if ctx.exclude.contains(&h) || !IsWindowVisible(h).as_bool() || IsIconic(h).as_bool() {
            return TRUE;
        }
        let mut cloaked = 0u32;
        let _ = DwmGetWindowAttribute(h, DWMWA_CLOAKED, &mut cloaked as *mut _ as *mut _, 4);
        if cloaked != 0 {
            return TRUE;
        }
        let ex = GetWindowLongW(h, GWL_EXSTYLE) as u32;
        if ex & WS_EX_TRANSPARENT.0 != 0 && ex & WS_EX_LAYERED.0 != 0 {
            return TRUE;
        }
        if let Some(r) = frame_rect(h) {
            ctx.out.push(RectI::new(
                r.left - ctx.ox,
                r.top - ctx.oy,
                r.right - ctx.ox,
                r.bottom - ctx.oy,
            ));
        }
        TRUE
    }
    let mut ctx = Ctx {
        out: Vec::new(),
        exclude,
        ox,
        oy,
    };
    unsafe {
        let _ = EnumWindows(Some(cb), LPARAM(&mut ctx as *mut _ as isize));
    }
    ctx.out
}
