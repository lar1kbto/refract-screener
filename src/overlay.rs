//! The full-screen capture window: region selection, annotation tools, toolbar,
//! colour picker, text recognition and live translation.

use crate::capture::Shot;
use crate::config::*;
use crate::encode::{self, Image};
use crate::export::{self, FxCache};
use crate::geom::*;
use crate::ocr::{self, OcrReply, TranslateReply};
use crate::paint::*;
use crate::shapes::*;
use crate::win::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::time::{Duration, Instant};
use windows::core::Result;
use windows::Foundation::Numerics::Matrix3x2;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::DirectWrite::*;
use windows::Win32::Graphics::Dwm::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Finish {
    Copy,
    Save,
    SaveAs,
    Cancel,
}

pub struct Done {
    pub how: Finish,
    pub image: Option<Image>,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hit {
    Tool(Tool),
    Ocr,
    Translate,
    Lang,
    Undo,
    Redo,
    Save,
    Copy,
    Close,
    Done,
    Swatch(usize),
    Custom,
    Size,
    Opacity,
    Fill,
    Shadow,
    PickSv,
    PickHue,
    LangItem(usize),
    Panel,
}

impl Hit {
    fn tip(self) -> Option<&'static str> {
        Some(match self {
            Hit::Tool(t) => t.name(),
            Hit::Ocr => "Select text (O)  -  Ctrl+C copies it",
            Hit::Translate => "Translate text (Ctrl+T)",
            Hit::Lang => "Translate to...",
            Hit::Undo => "Undo (Ctrl+Z)",
            Hit::Redo => "Redo (Ctrl+Y)",
            Hit::Save => "Save as... (Ctrl+Shift+S)",
            Hit::Copy => "Copy (Ctrl+C)",
            Hit::Close => "Cancel (Esc)",
            Hit::Done => "Done: copy and save (Enter)",
            Hit::Custom => "Custom colour  -  I picks from screen",
            Hit::Fill => "Fill: none / soft / solid",
            Hit::Shadow => "Shadow",
            _ => return None,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Popup {
    None,
    Color,
    Lang,
}

#[derive(Clone, Debug)]
enum Drag {
    None,
    NewSel { start: P },
    MoveSel { start: P, orig: RectI },
    ResizeSel { handle: usize, orig: RectI },
    Draw,
    MoveShape { idx: usize, last: P },
    ShapeHandle { idx: usize, handle: usize },
    Slider(Hit),
    Picker(Hit),
    Words { anchor: usize },
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Phase {
    Selecting,
    Editing,
}

struct OcrState {
    gen: u64,
    busy: bool,
    result: Option<Box<OcrReply>>,
    for_sel: Option<RectI>,
    mode: bool,
    range: Option<(usize, usize)>,
}

struct TrState {
    on: bool,
    gen: u64,
    busy: bool,
    target: String,
    cache: HashMap<(String, String), String>,
}

pub struct Overlay {
    pub hwnd: HWND,
    main: HWND,
    pub cfg: Config,
    shot: Shot,
    painter: Painter,
    rt: Option<ID2D1HwndRenderTarget>,
    base: Option<ID2D1Bitmap>,
    fx: FxCache,
    fx_bmp: HashMap<(Tool, u32), ID2D1Bitmap>,
    dashed: Option<ID2D1StrokeStyle>,

    phase: Phase,
    sel: Option<RectI>,
    drag: Drag,
    mouse: P,
    down_at: P,
    tool: Tool,
    shapes: Vec<Shape>,
    current: Option<Shape>,
    editing: Option<usize>,
    selected: Option<usize>,
    undo: Vec<Vec<Shape>>,
    redo: Vec<Vec<Shape>>,
    popup: Popup,
    hover: Option<Hit>,
    hsv: (f32, f32, f32),
    surrogate: Option<u16>,
    last_click: (Instant, P),

    ocr: OcrState,
    tr: TrState,
    toast: Option<(String, Instant, bool)>,
    tr_pending: Vec<String>,
    pub done: Option<Done>,
}

thread_local! {
    static CURRENT: RefCell<Option<Overlay>> = const { RefCell::new(None) };
}

pub fn is_open() -> bool {
    CURRENT.with(|c| c.try_borrow().map(|o| o.is_some()).unwrap_or(true))
}

/// Removes the overlay (its window is destroyed when it is dropped).
pub fn take() -> Option<Overlay> {
    CURRENT.with(|c| c.try_borrow_mut().ok().and_then(|mut o| o.take()))
}

impl Drop for Overlay {
    fn drop(&mut self) {
        self.rt = None;
        self.base = None;
        self.fx_bmp.clear();
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

fn register_class() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| unsafe {
        let name = W::new(OVERLAY_CLASS);
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            style: CS_DBLCLKS,
            lpfnWndProc: Some(wndproc),
            hInstance: GetModuleHandleW(None).unwrap_or_default().into(),
            hCursor: LoadCursorW(None, IDC_CROSS).unwrap_or_default(),
            lpszClassName: name.p(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        std::mem::forget(name);
    });
}

/// Shows the capture overlay for `shot`.
pub fn open(main: HWND, cfg: Config, shot: Shot) -> Result<()> {
    register_class();
    let painter = Painter::new(&cfg.font, cfg.corner_radius)?;
    unsafe {
        let name = W::new(OVERLAY_CLASS);
        let title = W::new("Refract-screener capture");
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            name.p(),
            title.p(),
            WS_POPUP,
            shot.ox,
            shot.oy,
            shot.w,
            shot.h,
            None,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        )?;
        let ov = build(hwnd, main, cfg, shot, painter);
        CURRENT.with(|c| *c.borrow_mut() = Some(ov));

        // Paint the first frame while the window is cloaked: no flash on show.
        let cloak: BOOL = TRUE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &cloak as *const _ as *const _, 4);
        let _ = ShowWindow(hwnd, SW_SHOW);
        with(|o| {
            let mut pt = POINT::default();
            let _ = GetCursorPos(&mut pt);
            o.mouse = p((pt.x - o.shot.ox) as f32, (pt.y - o.shot.oy) as f32);
            o.render();
            Some(())
        });
        let uncloak: BOOL = FALSE;
        let _ = DwmSetWindowAttribute(hwnd, DWMWA_CLOAK, &uncloak as *const _ as *const _, 4);
        force_foreground(hwnd);
    }
    Ok(())
}

fn build(hwnd: HWND, main: HWND, cfg: Config, shot: Shot, painter: Painter) -> Overlay {
    let tool = cfg.tool;
    let target = cfg.translate_to.clone();
    let hsv = cfg.style(tool).color.to_hsv();
    Overlay {
        hwnd,
        main,
        cfg,
        shot,
        painter,
        rt: None,
        base: None,
        fx: FxCache::default(),
        fx_bmp: HashMap::new(),
        dashed: None,
        phase: Phase::Selecting,
        sel: None,
        drag: Drag::None,
        mouse: P::default(),
        down_at: P::default(),
        tool,
        shapes: Vec::new(),
        current: None,
        editing: None,
        selected: None,
        undo: Vec::new(),
        redo: Vec::new(),
        popup: Popup::None,
        hover: None,
        hsv,
        surrogate: None,
        last_click: (Instant::now() - Duration::from_secs(5), P::default()),
        ocr: OcrState {
            gen: 0,
            busy: false,
            result: None,
            for_sel: None,
            mode: false,
            range: None,
        },
        tr: TrState {
            on: false,
            gen: 0,
            busy: false,
            target,
            cache: HashMap::new(),
        },
        toast: None,
        tr_pending: Vec::new(),
        done: None,
    }
}

/// Renders overlay states off screen into PNG files (used by --selftest).
pub fn preview(
    cfg: Config,
    shot: Shot,
    shapes: Vec<Shape>,
    sel: RectI,
    ocr: Option<Box<OcrReply>>,
    dir: &std::path::Path,
) -> Result<()> {
    register_class();
    let painter = Painter::new(&cfg.font, cfg.corner_radius)?;
    let hwnd = unsafe {
        let name = W::new(OVERLAY_CLASS);
        CreateWindowExW(
            WS_EX_TOOLWINDOW,
            name.p(),
            name.p(),
            WS_POPUP,
            0,
            0,
            1,
            1,
            None,
            None,
            GetModuleHandleW(None).unwrap_or_default(),
            None,
        )?
    };
    let mut o = build(hwnd, HWND::default(), cfg, shot, painter);
    o.mouse = p(sel.l as f32 + 60.0, sel.t as f32 + 40.0);
    render_to(&mut o, &dir.join("ui-1-selecting.png"))?;

    o.phase = Phase::Editing;
    o.sel = Some(sel);
    o.shapes = shapes;
    o.tool = Tool::Arrow;
    o.hover = Some(Hit::Tool(Tool::Arrow));
    render_to(&mut o, &dir.join("ui-2-editing.png"))?;

    o.selected = None;
    o.hover = None;
    o.popup = Popup::None;
    o.shapes.clear();
    o.tool = Tool::Select;
    if let Some(r) = ocr {
        // Select the first paragraph (lines 1..=3), like a drag over it.
        let a = r.lines.get(1).map(|l| l.first_word).unwrap_or(0);
        let b = r.lines.get(3).map(|l| l.first_word + l.word_count - 1).unwrap_or(r.words.len().saturating_sub(1));
        o.ocr.result = Some(r);
        o.ocr.for_sel = Some(sel);
        o.ocr.mode = true;
        o.ocr.range = Some((a, b));
    }
    o.toast = Some(("Copied 213 characters".into(), Instant::now(), false));
    render_to(&mut o, &dir.join("ui-3-text.png"))?;

    o.toast = None;
    o.ocr.mode = false;
    o.ocr.range = None;
    o.tr.on = true;
    render_to(&mut o, &dir.join("ui-4-translate.png"))?;
    Ok(())
}

fn render_to(o: &mut Overlay, path: &std::path::Path) -> Result<()> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    let (w, h) = (o.shot.w as u32, o.shot.h as u32);
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
        let target =
            wic.CreateBitmap(w, h, &GUID_WICPixelFormat32bppPBGRA, WICBitmapCacheOnLoad)?;
        let props = D2D1_RENDER_TARGET_PROPERTIES {
            r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
            pixelFormat: D2D1_PIXEL_FORMAT {
                format: DXGI_FORMAT_B8G8R8A8_UNORM,
                alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
            },
            dpiX: 96.0,
            dpiY: 96.0,
            usage: D2D1_RENDER_TARGET_USAGE_NONE,
            minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
        };
        let rt = o.painter.d2d.CreateWicBitmapRenderTarget(&target, &props)?;
        rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
        let base = export::make_bitmap(&rt, &o.shot.px, o.shot.w, o.shot.h)?;
        o.base = Some(base.clone());
        o.fx_bmp.clear();
        o.dashed = o.painter.dashed().ok();
        rt.BeginDraw();
        rt.SetTransform(&Matrix3x2::identity());
        rt.DrawBitmap(
            &base,
            Some(&rc(o.shot.full().f())),
            1.0,
            D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
            None,
        );
        let _ = o.draw_scene(&rt);
        rt.EndDraw(None, None)?;
        let mut px = vec![0u8; (w * h * 4) as usize];
        target.CopyPixels(std::ptr::null(), w * 4, &mut px)?;
        o.base = None;
        o.fx_bmp.clear();
        let img = Image { w, h, bgra: px };
        let data = encode::png(&img, 0).map_err(|e| windows::core::Error::new(E_FAIL, e))?;
        let _ = std::fs::write(path, data);
    }
    Ok(())
}

unsafe fn force_foreground(hwnd: HWND) {
    let fg = GetForegroundWindow();
    let other = GetWindowThreadProcessId(fg, None);
    let me = GetCurrentThreadId();
    if other != 0 && other != me {
        let _ = AttachThreadInput(me, other, TRUE);
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
        let _ = AttachThreadInput(me, other, FALSE);
    } else {
        let _ = SetForegroundWindow(hwnd);
        let _ = SetFocus(hwnd);
    }
}

fn with<R>(f: impl FnOnce(&mut Overlay) -> Option<R>) -> Option<R> {
    CURRENT.with(|c| {
        let mut g = c.try_borrow_mut().ok()?;
        f(g.as_mut()?)
    })
}

unsafe extern "system" fn wndproc(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_ERASEBKGND => return LRESULT(1),
        WM_APP_OCR => {
            let reply = Box::from_raw(lp.0 as *mut OcrReply);
            with(|o| {
                o.on_ocr(reply);
                Some(())
            });
            return LRESULT(0);
        }
        WM_APP_TRANSLATED => {
            let reply = Box::from_raw(lp.0 as *mut TranslateReply);
            with(|o| {
                o.on_translated(*reply);
                Some(())
            });
            return LRESULT(0);
        }
        _ => {}
    }
    if let Some(r) = with(|o| o.handle(msg, wp, lp)) {
        return r;
    }
    DefWindowProcW(h, msg, wp, lp)
}

fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { (GetKeyState(vk.0 as i32) as u16 & 0x8000) != 0 }
}

impl Overlay {
    fn s(&self) -> f32 {
        let c = self
            .sel
            .map(|r| (r.l + r.w() / 2, r.t + r.h() / 2))
            .unwrap_or((self.mouse.x as i32, self.mouse.y as i32));
        self.shot
            .monitor_at(c.0, c.1)
            .map(|m| m.scale)
            .unwrap_or(1.0)
            * self.cfg.ui_scale
    }

    fn style(&self) -> Style {
        match self.selected.and_then(|i| self.shapes.get(i)) {
            Some(s) => s.style,
            None => self.cfg.style(self.tool),
        }
    }
    fn style_tool(&self) -> Tool {
        match self.selected.and_then(|i| self.shapes.get(i)) {
            Some(s) => s.tool(),
            None => self.tool,
        }
    }

    fn set_style(&mut self, st: Style, checkpoint: bool) {
        let t = self.style_tool();
        if let Some(i) = self.selected {
            if checkpoint {
                self.checkpoint();
            }
            if let Some(s) = self.shapes.get_mut(i) {
                s.style = st;
                if let Kind::Text { ext, .. } = &s.kind {
                    ext.set((0.0, 0.0));
                }
            }
        }
        self.cfg.set_style(t, st);
    }

    fn set_color(&mut self, c: Rgb, checkpoint: bool) {
        let mut st = self.style();
        st.color = c;
        self.set_style(st, checkpoint);
        for t in Tool::ALL {
            let mut s = self.cfg.style(t);
            s.color = c;
            self.cfg.set_style(t, s);
        }
    }

    fn checkpoint(&mut self) {
        self.undo.push(self.shapes.clone());
        if self.undo.len() > 200 {
            self.undo.remove(0);
        }
        self.redo.clear();
    }

    fn invalidate(&self) {
        unsafe {
            let _ = InvalidateRect(self.hwnd, None, FALSE);
        }
    }

    fn toast(&mut self, text: impl Into<String>, error: bool) {
        self.toast = Some((text.into(), Instant::now(), error));
        unsafe {
            SetTimer(self.hwnd, 1, 120, None);
        }
    }

    // ---------------------------------------------------------------- layout

    fn monitor_work(&self, r: &RectI) -> RectF {
        self.shot
            .monitor_at(r.l + r.w() / 2, r.t + r.h() / 2)
            .map(|m| m.work.f())
            .unwrap_or(self.shot.full().f())
    }

    /// Toolbar buttons with their rectangles; first item is the panel itself.
    fn layout(&self) -> Vec<(RectF, Hit)> {
        let Some(sel) = self.sel else {
            return Vec::new();
        };
        if self.phase != Phase::Editing
            || matches!(
                self.drag,
                Drag::NewSel { .. } | Drag::MoveSel { .. } | Drag::ResizeSel { .. }
            )
        {
            return Vec::new();
        }
        let s = self.s();
        let b = 34.0 * s;
        let pad = 6.0 * s;
        let sep = 9.0 * s;
        let mut row1: Vec<(f32, Option<Hit>)> = Vec::new();
        for t in Tool::ALL {
            row1.push((b, Some(Hit::Tool(t))));
        }
        row1.push((sep, None));
        row1.push((b, Some(Hit::Ocr)));
        row1.push((b, Some(Hit::Translate)));
        row1.push((46.0 * s, Some(Hit::Lang)));
        row1.push((sep, None));
        row1.push((b, Some(Hit::Undo)));
        row1.push((b, Some(Hit::Redo)));
        row1.push((sep, None));
        row1.push((b, Some(Hit::Save)));
        row1.push((b, Some(Hit::Copy)));
        row1.push((b, Some(Hit::Close)));
        row1.push((b, Some(Hit::Done)));

        let sw = 24.0 * s;
        let mut row2: Vec<(f32, Option<Hit>)> = Vec::new();
        for i in 0..self.cfg.palette.len() {
            row2.push((sw, Some(Hit::Swatch(i))));
        }
        row2.push((sw, Some(Hit::Custom)));
        row2.push((sep, None));
        row2.push((172.0 * s, Some(Hit::Size)));
        row2.push((sep * 0.5, None));
        row2.push((160.0 * s, Some(Hit::Opacity)));
        row2.push((sep * 0.5, None));
        row2.push((b, Some(Hit::Fill)));
        row2.push((b, Some(Hit::Shadow)));

        let w1: f32 = row1.iter().map(|x| x.0).sum();
        let w2: f32 = row2.iter().map(|x| x.0).sum();
        let pw = w1.max(w2) + pad * 2.0;
        let r2h = 28.0 * s;
        let ph = pad + b + 5.0 * s + r2h + pad;

        let work = self.monitor_work(&sel);
        let gap = 10.0 * s;
        let selr = sel.f();
        let x = (selr.cx() - pw * 0.5).clamp(work.l + 8.0, (work.r - pw - 8.0).max(work.l + 8.0));
        let mut y = selr.b + gap;
        if y + ph > work.b - 8.0 {
            y = selr.t - gap - ph;
            if y < work.t + 8.0 {
                y = (selr.b - ph - gap).max(work.t + 8.0);
            }
        }
        let panel = RectF::xywh(x, y, pw, ph);
        let mut out = vec![(panel, Hit::Panel)];
        let mut cx = x + pad + (pw - pad * 2.0 - w1) * 0.5;
        for (w, h) in row1 {
            if let Some(h) = h {
                out.push((RectF::xywh(cx, y + pad, w, b), h));
            }
            cx += w;
        }
        let ry = y + pad + b + 5.0 * s;
        let mut cx = x + pad + (pw - pad * 2.0 - w2) * 0.5;
        for (w, h) in row2 {
            if let Some(h) = h {
                out.push((RectF::xywh(cx, ry, w, r2h), h));
            }
            cx += w;
        }
        // Popups.
        let find = |hh: Hit| out.iter().find(|(_, x)| *x == hh).map(|(r, _)| *r);
        match self.popup {
            Popup::Color => {
                if let Some(anchor) = find(Hit::Custom) {
                    let (pw2, ph2) = (196.0 * s, 178.0 * s);
                    let mut py = panel.t - ph2 - 8.0 * s;
                    if py < work.t {
                        py = panel.b + 8.0 * s;
                    }
                    let px = (anchor.cx() - pw2 * 0.5).clamp(work.l + 4.0, work.r - pw2 - 4.0);
                    let pr = RectF::xywh(px, py, pw2, ph2);
                    out.push((pr, Hit::Panel));
                    out.push((
                        RectF::xywh(px + 10.0 * s, py + 10.0 * s, 146.0 * s, 132.0 * s),
                        Hit::PickSv,
                    ));
                    out.push((
                        RectF::xywh(px + 164.0 * s, py + 10.0 * s, 22.0 * s, 132.0 * s),
                        Hit::PickHue,
                    ));
                }
            }
            Popup::Lang => {
                if let Some(anchor) = find(Hit::Lang) {
                    let n = self.cfg.translate_languages.len();
                    let ih = 26.0 * s;
                    let (pw2, ph2) = (150.0 * s, ih * n as f32 + 8.0 * s);
                    let mut py = panel.t - ph2 - 8.0 * s;
                    if py < work.t {
                        py = panel.b + 8.0 * s;
                    }
                    let px = (anchor.cx() - pw2 * 0.5).clamp(work.l + 4.0, work.r - pw2 - 4.0);
                    out.push((RectF::xywh(px, py, pw2, ph2), Hit::Panel));
                    for i in 0..n {
                        out.push((
                            RectF::xywh(
                                px + 4.0 * s,
                                py + 4.0 * s + ih * i as f32,
                                pw2 - 8.0 * s,
                                ih,
                            ),
                            Hit::LangItem(i),
                        ));
                    }
                }
            }
            Popup::None => {}
        }
        out
    }

    fn ui_hit(&self, q: P) -> Option<(RectF, Hit)> {
        let lay = self.layout();
        // Popup items first (drawn on top), then buttons, then panels.
        let mut panel = None;
        for (r, h) in lay.iter().rev() {
            if r.contains(q) {
                if *h == Hit::Panel {
                    panel.get_or_insert((*r, *h));
                } else {
                    return Some((*r, *h));
                }
            }
        }
        panel
    }

    fn sel_handles(&self) -> Vec<P> {
        let Some(r) = self.sel else { return Vec::new() };
        let r = r.f();
        vec![
            p(r.l, r.t),
            p(r.cx(), r.t),
            p(r.r, r.t),
            p(r.r, r.cy()),
            p(r.r, r.b),
            p(r.cx(), r.b),
            p(r.l, r.b),
            p(r.l, r.cy()),
        ]
    }

    fn sel_handle_at(&self, q: P) -> Option<usize> {
        if self.tool != Tool::Select || self.ocr.mode {
            return None;
        }
        let tol = 9.0 * self.s();
        self.sel_handles().iter().position(|h| h.dist(q) <= tol)
    }

    fn snap_target(&self) -> RectI {
        let (x, y) = (self.mouse.x as i32, self.mouse.y as i32);
        if self.cfg.snap_windows {
            if let Some(r) = self.shot.windows.iter().find(|r| r.contains(x, y)) {
                return r.clamp_to(&self.shot.full());
            }
        }
        self.shot
            .monitor_at(x, y)
            .map(|m| m.rect)
            .unwrap_or(self.shot.full())
    }

    fn word_at(&self, q: P, nearest: bool) -> Option<usize> {
        let res = self.ocr.result.as_ref()?;
        if let Some(i) = res
            .words
            .iter()
            .position(|w| w.rect.inflate(3.0).contains(q))
        {
            return Some(i);
        }
        if !nearest {
            return None;
        }
        // Nearest word, preferring the line under the pointer.
        res.words
            .iter()
            .enumerate()
            .min_by(|a, b| {
                let d = |w: &ocr::Word| {
                    let r = w.rect;
                    let dx = (r.l - q.x).max(0.0).max(q.x - r.r);
                    let dy = (r.t - q.y).max(0.0).max(q.y - r.b);
                    dx + dy * 4.0
                };
                d(a.1)
                    .partial_cmp(&d(b.1))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .map(|(i, _)| i)
    }

    // ---------------------------------------------------------------- OCR / translation

    fn ocr_valid(&self) -> bool {
        self.ocr.result.is_some() && self.ocr.for_sel == self.sel
    }

    fn ensure_ocr(&mut self) {
        let Some(sel) = self.sel else { return };
        if sel.is_empty() || self.ocr_valid() || (self.ocr.busy && self.ocr.for_sel == self.sel) {
            return;
        }
        self.ocr.gen += 1;
        self.ocr.busy = true;
        self.ocr.for_sel = Some(sel);
        self.ocr.result = None;
        self.ocr.range = None;
        let px = self.shot.crop(&sel);
        ocr::spawn_ocr(
            hwnd_to(self.hwnd),
            self.ocr.gen,
            sel,
            px,
            self.cfg.ocr_language.clone(),
        );
        unsafe {
            SetTimer(self.hwnd, 1, 120, None);
        }
    }

    fn reset_ocr(&mut self) {
        self.ocr.result = None;
        self.ocr.range = None;
        self.ocr.for_sel = None;
        self.ocr.busy = false;
        self.ocr.gen += 1;
        self.tr.gen += 1;
        self.tr.busy = false;
    }

    fn after_sel_change(&mut self) {
        self.reset_ocr();
        if self.cfg.auto_ocr || self.ocr.mode || self.tr.on {
            self.ensure_ocr();
        }
    }

    fn on_ocr(&mut self, r: Box<OcrReply>) {
        if r.gen != self.ocr.gen {
            return;
        }
        self.ocr.busy = false;
        if let Some(e) = &r.error {
            self.toast(e.clone(), true);
        } else if (self.ocr.mode || self.tr.on) && r.words.is_empty() {
            self.toast("No text found in the selection", false);
        }
        self.ocr.result = Some(r);
        if self.tr.on {
            self.request_translation();
        }
        self.invalidate();
    }

    fn request_translation(&mut self) {
        if !self.ocr_valid() {
            self.ensure_ocr();
            return;
        }
        let target = self.tr.target.clone();
        let Some(res) = self.ocr.result.as_mut() else {
            return;
        };
        let mut missing = Vec::new();
        for b in res.blocks.iter_mut() {
            match self.tr.cache.get(&(target.clone(), b.text.clone())) {
                Some(t) => {
                    if b.translated.as_deref() != Some(t.as_str()) {
                        b.translated = Some(t.clone());
                        b.fitted.set(0.0);
                    }
                }
                None => {
                    b.translated = None;
                    if !missing.contains(&b.text) {
                        missing.push(b.text.clone());
                    }
                }
            }
        }
        if missing.is_empty() {
            self.tr.busy = false;
            return;
        }
        self.tr.gen += 1;
        self.tr.busy = true;
        self.tr_pending = missing.clone();
        ocr::spawn_translate(
            hwnd_to(self.hwnd),
            self.tr.gen,
            missing,
            target,
            self.cfg.google_api_key.clone(),
        );
        unsafe {
            SetTimer(self.hwnd, 1, 120, None);
        }
    }

    fn on_translated(&mut self, r: TranslateReply) {
        if r.gen != self.tr.gen {
            return;
        }
        self.tr.busy = false;
        let asked = std::mem::take(&mut self.tr_pending);
        match r.result {
            Ok(list) => {
                for (src, dst) in asked.into_iter().zip(list) {
                    self.tr.cache.insert((r.target.clone(), src), dst);
                }
                if self.tr.on {
                    self.request_translation();
                }
            }
            Err(e) => {
                self.toast(e, true);
            }
        }
        self.invalidate();
    }

    fn selected_text(&self, all: bool) -> String {
        let Some(res) = self.ocr.result.as_ref() else {
            return String::new();
        };
        if self.tr.on && res.blocks.iter().any(|b| b.translated.is_some()) {
            return res
                .blocks
                .iter()
                .map(|b| b.translated.clone().unwrap_or_else(|| b.text.clone()))
                .collect::<Vec<_>>()
                .join("\n");
        }
        let (a, b) = match (self.ocr.range, all) {
            (Some((a, b)), false) => (a.min(b), a.max(b)),
            _ => (0, res.words.len().saturating_sub(1)),
        };
        if res.words.is_empty() {
            return String::new();
        }
        let mut out: Vec<String> = Vec::new();
        for (li, line) in res.lines.iter().enumerate() {
            let lo = line.first_word;
            let hi = line.first_word + line.word_count;
            if hi <= a || lo > b {
                continue;
            }
            if lo >= a && hi - 1 <= b {
                out.push(line.text.clone());
            } else {
                let words: Vec<&str> = res.words[lo.max(a)..hi.min(b + 1)]
                    .iter()
                    .filter(|w| w.line == li)
                    .map(|w| w.text.as_str())
                    .collect();
                out.push(words.join(" "));
            }
        }
        out.join("\n")
    }

    fn copy_text(&mut self) {
        let t = self.selected_text(self.ocr.range.is_none());
        if t.trim().is_empty() {
            self.toast(
                if self.ocr.busy {
                    "Still recognising text..."
                } else {
                    "No text to copy"
                },
                false,
            );
            return;
        }
        if encode::copy_text(self.hwnd, &t) {
            let n = t.chars().count();
            self.toast(format!("Copied {n} characters"), false);
        }
    }

    // ---------------------------------------------------------------- actions

    fn commit_text(&mut self) {
        if let Some(i) = self.editing.take() {
            if self.shapes.get(i).is_some_and(|s| s.is_degenerate()) {
                self.shapes.remove(i);
                if self.selected == Some(i) {
                    self.selected = None;
                }
            }
            self.selected = None;
        }
    }

    fn set_tool(&mut self, t: Tool) {
        self.commit_text();
        self.selected = None;
        self.tool = t;
        self.cfg.tool = t;
        self.ocr.mode = false;
        self.popup = Popup::None;
        self.hsv = self.cfg.style(t).color.to_hsv();
    }

    fn finish(&mut self, how: Finish) {
        self.commit_text();
        let image = match (how, self.sel) {
            (Finish::Cancel, _) | (_, None) => None,
            (_, Some(sel)) => {
                let blocks = if self.tr.on {
                    self.ocr.result.as_ref().map(|r| r.blocks.as_slice())
                } else {
                    None
                };
                export::render(
                    &self.painter,
                    &self.shot,
                    sel,
                    &self.shapes,
                    &mut self.fx,
                    blocks,
                )
                .ok()
            }
        };
        self.done = Some(Done { how, image });
        unsafe {
            let _ = ShowWindow(self.hwnd, SW_HIDE);
            let _ = PostMessageW(self.main, WM_APP_OVERLAY_DONE, WPARAM(0), LPARAM(0));
        }
    }

    fn click_ui(&mut self, h: Hit, r: RectF) {
        match h {
            Hit::Tool(t) => self.set_tool(t),
            Hit::Ocr => {
                self.commit_text();
                self.ocr.mode = !self.ocr.mode;
                self.selected = None;
                if self.ocr.mode {
                    self.ensure_ocr();
                    if self.ocr.busy {
                        self.toast("Recognising text...", false);
                    }
                }
            }
            Hit::Translate => self.toggle_translate(),
            Hit::Lang => {
                self.popup = if self.popup == Popup::Lang {
                    Popup::None
                } else {
                    Popup::Lang
                }
            }
            Hit::LangItem(i) => {
                if let Some(l) = self.cfg.translate_languages.get(i) {
                    self.tr.target = l.clone();
                    self.cfg.translate_to = l.clone();
                    if let Some(res) = self.ocr.result.as_mut() {
                        res.blocks.iter_mut().for_each(|b| b.translated = None);
                    }
                    self.tr.on = true;
                    self.request_translation();
                }
                self.popup = Popup::None;
            }
            Hit::Undo => self.undo_step(),
            Hit::Redo => self.redo_step(),
            Hit::Save => self.finish(Finish::SaveAs),
            Hit::Copy => {
                if self.ocr.mode {
                    self.copy_text()
                } else {
                    self.finish(Finish::Copy)
                }
            }
            Hit::Close => self.finish(Finish::Cancel),
            Hit::Done => self.finish(Finish::Save),
            Hit::Swatch(i) => {
                if let Some(c) = self.cfg.palette.get(i).copied() {
                    self.set_color(c, true);
                    self.hsv = c.to_hsv();
                }
                self.popup = Popup::None;
            }
            Hit::Custom => {
                self.hsv = self.style().color.to_hsv();
                self.popup = if self.popup == Popup::Color {
                    Popup::None
                } else {
                    Popup::Color
                };
            }
            Hit::Size | Hit::Opacity => {
                if self.selected.is_some() {
                    self.checkpoint();
                }
                self.drag = Drag::Slider(h);
                self.slide(h, r);
            }
            Hit::PickSv | Hit::PickHue => {
                if self.selected.is_some() {
                    self.checkpoint();
                }
                self.drag = Drag::Picker(h);
                self.pick(h, r);
            }
            Hit::Fill => {
                let mut st = self.style();
                st.fill = st.fill.next();
                self.set_style(st, true);
            }
            Hit::Shadow => {
                let mut st = self.style();
                st.shadow = !st.shadow;
                self.set_style(st, true);
            }
            Hit::Panel => {}
        }
    }

    fn toggle_translate(&mut self) {
        self.tr.on = !self.tr.on;
        self.popup = Popup::None;
        if self.tr.on {
            self.request_translation();
            if self.ocr.busy || self.tr.busy {
                self.toast(
                    format!("Translating to {}...", self.tr.target.to_uppercase()),
                    false,
                );
            }
        }
    }

    fn slider_track(&self, r: RectF) -> (f32, f32) {
        let s = self.s();
        (r.l + 80.0 * s, r.r - 10.0 * s)
    }

    fn slide(&mut self, h: Hit, r: RectF) {
        let (a, b) = self.slider_track(r);
        let t = ((self.mouse.x - a) / (b - a)).clamp(0.0, 1.0);
        let mut st = self.style();
        match h {
            Hit::Size => {
                let (lo, hi) = self.style_tool().size_range();
                // Quadratic: fine control at small sizes.
                st.width = (lo + (hi - lo) * t * t).round().max(lo);
            }
            _ => st.opacity = (0.05 + 0.95 * t).clamp(0.05, 1.0),
        }
        self.set_style(st, false);
    }

    fn pick(&mut self, h: Hit, r: RectF) {
        let q = self.mouse;
        let fx = ((q.x - r.l) / r.w()).clamp(0.0, 1.0);
        let fy = ((q.y - r.t) / r.h()).clamp(0.0, 1.0);
        match h {
            Hit::PickSv => {
                self.hsv.1 = fx;
                self.hsv.2 = 1.0 - fy;
            }
            _ => self.hsv.0 = fy * 360.0,
        }
        let c = Rgb::from_hsv(self.hsv.0, self.hsv.1, self.hsv.2);
        self.set_color(c, false);
    }

    fn undo_step(&mut self) {
        self.commit_text();
        if let Some(prev) = self.undo.pop() {
            self.redo.push(std::mem::replace(&mut self.shapes, prev));
            self.selected = None;
        }
    }

    fn redo_step(&mut self) {
        self.commit_text();
        if let Some(next) = self.redo.pop() {
            self.undo.push(std::mem::replace(&mut self.shapes, next));
            self.selected = None;
        }
    }

    fn next_counter(&self) -> u32 {
        self.shapes
            .iter()
            .filter_map(|s| match s.kind {
                Kind::Counter { n, .. } => Some(n),
                _ => None,
            })
            .max()
            .unwrap_or(0)
            + 1
    }

    fn shape_at(&self, q: P) -> Option<usize> {
        let tol = 5.0 * self.s();
        (0..self.shapes.len())
            .rev()
            .find(|&i| self.shapes[i].hit(q, tol))
    }

    fn eyedrop(&mut self) {
        if let Some((r, g, b)) = self.shot.pixel(self.mouse.x as i32, self.mouse.y as i32) {
            let c = Rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32);
            self.set_color(c, true);
            self.hsv = c.to_hsv();
            self.toast(format!("Colour {}", c.hex()), false);
        }
    }

    // ---------------------------------------------------------------- input

    fn handle(&mut self, msg: u32, wp: WPARAM, lp: LPARAM) -> Option<LRESULT> {
        match msg {
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                unsafe {
                    BeginPaint(self.hwnd, &mut ps);
                }
                self.render();
                unsafe {
                    let _ = EndPaint(self.hwnd, &ps);
                }
                Some(LRESULT(0))
            }
            WM_TIMER => {
                let busy = self.ocr.busy || self.tr.busy;
                let toast_alive = self
                    .toast
                    .as_ref()
                    .is_some_and(|t| t.1.elapsed() < Duration::from_millis(2600));
                if !toast_alive {
                    self.toast = None;
                }
                if !busy && !toast_alive {
                    unsafe {
                        let _ = KillTimer(self.hwnd, 1);
                    }
                }
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_SETCURSOR => {
                self.update_cursor();
                Some(LRESULT(1))
            }
            WM_MOUSEMOVE => {
                let (x, y) = lparam_xy(lp);
                self.on_move(p(x as f32, y as f32));
                Some(LRESULT(0))
            }
            WM_LBUTTONDOWN | WM_LBUTTONDBLCLK => {
                let (x, y) = lparam_xy(lp);
                unsafe {
                    SetCapture(self.hwnd);
                }
                self.on_down(p(x as f32, y as f32));
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_LBUTTONUP => {
                let (x, y) = lparam_xy(lp);
                unsafe {
                    let _ = ReleaseCapture();
                }
                self.on_up(p(x as f32, y as f32));
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_RBUTTONDOWN => {
                self.on_right();
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_MOUSEWHEEL => {
                let delta = ((wp.0 >> 16) & 0xFFFF) as i16 as f32 / 120.0;
                self.on_wheel(delta);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_KEYDOWN | WM_SYSKEYDOWN => {
                self.on_key(wp.0 as u16);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_CHAR => {
                self.on_char(wp.0 as u16);
                self.invalidate();
                Some(LRESULT(0))
            }
            WM_CLOSE => {
                self.finish(Finish::Cancel);
                Some(LRESULT(0))
            }
            WM_DISPLAYCHANGE => {
                self.finish(Finish::Cancel);
                Some(LRESULT(0))
            }
            _ => None,
        }
    }

    fn update_cursor(&self) {
        let q = self.mouse;
        let id = if self.phase == Phase::Selecting {
            IDC_CROSS
        } else if self.ui_hit(q).is_some() {
            IDC_ARROW
        } else if let Some(h) = self.sel_handle_at(q) {
            match h {
                0 | 4 => IDC_SIZENWSE,
                2 | 6 => IDC_SIZENESW,
                1 | 5 => IDC_SIZENS,
                _ => IDC_SIZEWE,
            }
        } else if self.ocr.mode {
            if self.word_at(q, false).is_some() {
                IDC_IBEAM
            } else {
                IDC_ARROW
            }
        } else {
            let inside = self.sel.is_some_and(|r| r.f().contains(q));
            match self.tool {
                Tool::Select => {
                    if self.shape_at(q).is_some() || inside {
                        IDC_SIZEALL
                    } else {
                        IDC_CROSS
                    }
                }
                Tool::Text if inside => IDC_IBEAM,
                _ if inside => IDC_CROSS,
                _ => IDC_ARROW,
            }
        };
        unsafe {
            if let Ok(c) = LoadCursorW(None, id) {
                SetCursor(c);
            }
        }
    }

    fn on_down(&mut self, q: P) {
        self.mouse = q;
        self.down_at = q;
        let double = self.last_click.0.elapsed() < Duration::from_millis(400)
            && self.last_click.1.dist(q) < 5.0;
        self.last_click = (Instant::now(), q);

        if self.phase == Phase::Selecting {
            self.drag = Drag::NewSel { start: q };
            return;
        }
        if let Some((r, h)) = self.ui_hit(q) {
            self.click_ui(h, r);
            return;
        }
        if self.popup != Popup::None {
            self.popup = Popup::None;
            return;
        }
        if let Some(h) = self.sel_handle_at(q) {
            self.drag = Drag::ResizeSel {
                handle: h,
                orig: self.sel.unwrap(),
            };
            return;
        }
        let inside = self.sel.is_some_and(|r| r.f().contains(q));

        if self.ocr.mode {
            if let Some(w) =
                self.word_at(q, false)
                    .or_else(|| if inside { self.word_at(q, true) } else { None })
            {
                if double {
                    // Double click: the whole line.
                    if let Some(res) = &self.ocr.result {
                        let l = &res.lines[res.words[w].line];
                        self.ocr.range = Some((l.first_word, l.first_word + l.word_count - 1));
                    }
                } else {
                    self.ocr.range = Some((w, w));
                    self.drag = Drag::Words { anchor: w };
                }
            } else {
                self.ocr.range = None;
            }
            return;
        }

        // Clicking a text shape edits it.
        if let Some(i) = self.shape_at(q) {
            if matches!(self.shapes[i].kind, Kind::Text { .. })
                && (self.tool == Tool::Text || (self.tool == Tool::Select && double))
            {
                if self.editing != Some(i) {
                    self.commit_text();
                    self.checkpoint();
                }
                let i = i.min(self.shapes.len().saturating_sub(1));
                self.editing = Some(i);
                self.selected = Some(i);
                self.drag = Drag::MoveShape { idx: i, last: q };
                return;
            }
        }
        self.commit_text();

        match self.tool {
            Tool::Select => {
                if let Some(i) = self.selected {
                    let tol = 8.0 * self.s();
                    if let Some(hh) = self
                        .shapes
                        .get(i)
                        .and_then(|s| s.handles().iter().position(|h| h.dist(q) <= tol))
                    {
                        self.checkpoint();
                        self.drag = Drag::ShapeHandle { idx: i, handle: hh };
                        return;
                    }
                }
                if let Some(i) = self.shape_at(q) {
                    self.selected = Some(i);
                    self.hsv = self.shapes[i].style.color.to_hsv();
                    self.checkpoint();
                    self.drag = Drag::MoveShape { idx: i, last: q };
                } else if inside {
                    self.selected = None;
                    self.drag = Drag::MoveSel {
                        start: q,
                        orig: self.sel.unwrap(),
                    };
                } else {
                    self.selected = None;
                    self.drag = Drag::NewSel { start: q };
                }
            }
            t => {
                if !inside {
                    return;
                }
                self.selected = None;
                let st = self.cfg.style(t);
                match t {
                    Tool::Text => {
                        self.checkpoint();
                        let at = p(q.x, q.y - st.width * 0.6);
                        self.shapes.push(Shape {
                            kind: Kind::Text {
                                at,
                                text: String::new(),
                                ext: Default::default(),
                            },
                            style: st,
                        });
                        let i = self.shapes.len() - 1;
                        self.editing = Some(i);
                        self.selected = Some(i);
                    }
                    Tool::Counter => {
                        self.checkpoint();
                        let n = self.next_counter();
                        self.shapes.push(Shape {
                            kind: Kind::Counter { at: q, n },
                            style: st,
                        });
                    }
                    _ => {
                        let kind = match t {
                            Tool::Pen => Kind::Pen(vec![q]),
                            Tool::Marker => Kind::Marker(vec![q]),
                            Tool::Line => Kind::Line(q, q),
                            Tool::Arrow => Kind::Arrow(q, q),
                            Tool::Rect => Kind::Rect(q, q),
                            Tool::Ellipse => Kind::Ellipse(q, q),
                            Tool::Blur => Kind::Blur(q, q),
                            _ => Kind::Pixelate(q, q),
                        };
                        self.current = Some(Shape { kind, style: st });
                        self.drag = Drag::Draw;
                    }
                }
            }
        }
    }

    fn on_move(&mut self, q: P) {
        let prev = self.mouse;
        self.mouse = q;
        let shift = key_down(VK_SHIFT);
        let full = self.shot.full();
        match self.drag.clone() {
            Drag::None => {
                let h = if self.phase == Phase::Editing {
                    self.ui_hit(q).map(|x| x.1)
                } else {
                    None
                };
                if h != self.hover {
                    self.hover = h;
                }
            }
            Drag::NewSel { start } => {
                if start.dist(q) >= 3.0 {
                    let a = (start.x.round() as i32, start.y.round() as i32);
                    let mut b = (q.x.round() as i32, q.y.round() as i32);
                    if shift {
                        let d = (b.0 - a.0).abs().max((b.1 - a.1).abs());
                        b = (
                            a.0 + d * (b.0 - a.0).signum(),
                            a.1 + d * (b.1 - a.1).signum(),
                        );
                    }
                    let r = RectI::norm(a, (b.0 + 1, b.1 + 1)).clamp_to(&full);
                    self.sel = Some(r);
                    self.phase = Phase::Editing;
                }
            }
            Drag::MoveSel { start, orig } => {
                let (dx, dy) = (
                    (q.x - start.x).round() as i32,
                    (q.y - start.y).round() as i32,
                );
                let dx = dx.clamp(-orig.l, full.r - orig.r);
                let dy = dy.clamp(-orig.t, full.b - orig.b);
                self.sel = Some(RectI::new(
                    orig.l + dx,
                    orig.t + dy,
                    orig.r + dx,
                    orig.b + dy,
                ));
            }
            Drag::ResizeSel { handle, orig } => {
                let (x, y) = (q.x.round() as i32, q.y.round() as i32);
                let (mut l, mut t, mut r, mut b) = (orig.l, orig.t, orig.r, orig.b);
                match handle {
                    0 => {
                        l = x;
                        t = y
                    }
                    1 => t = y,
                    2 => {
                        r = x;
                        t = y
                    }
                    3 => r = x,
                    4 => {
                        r = x;
                        b = y
                    }
                    5 => b = y,
                    6 => {
                        l = x;
                        b = y
                    }
                    _ => l = x,
                }
                let n = RectI::norm((l, t), (r, b)).clamp_to(&full);
                if n.w() >= 1 && n.h() >= 1 {
                    self.sel = Some(n);
                }
            }
            Drag::Draw => {
                if let Some(cur) = self.current.as_mut() {
                    match &mut cur.kind {
                        Kind::Pen(pts) | Kind::Marker(pts) => {
                            if shift && !pts.is_empty() {
                                let a = pts[0];
                                pts.clear();
                                pts.push(a);
                                pts.push(snap45(a, q));
                            } else if pts.last().map_or(true, |l| l.dist(q) >= 1.2) {
                                pts.push(q);
                            }
                        }
                        Kind::Line(a, b) | Kind::Arrow(a, b) => {
                            *b = if shift { snap45(*a, q) } else { q }
                        }
                        Kind::Rect(a, b) | Kind::Ellipse(a, b) => {
                            *b = if shift { square(*a, q) } else { q }
                        }
                        Kind::Blur(_, b) | Kind::Pixelate(_, b) => *b = q,
                        _ => {}
                    }
                }
            }
            Drag::MoveShape { idx, last } => {
                if let Some(s) = self.shapes.get_mut(idx) {
                    s.translate(q.sub(last));
                }
                self.drag = Drag::MoveShape { idx, last: q };
            }
            Drag::ShapeHandle { idx, handle } => {
                if let Some(s) = self.shapes.get_mut(idx) {
                    s.drag_handle(handle, q);
                }
            }
            Drag::Slider(h) => {
                if let Some((r, _)) = self.layout().into_iter().find(|(_, x)| *x == h) {
                    self.slide(h, r);
                }
            }
            Drag::Picker(h) => {
                if let Some((r, _)) = self.layout().into_iter().find(|(_, x)| *x == h) {
                    self.pick(h, r);
                }
            }
            Drag::Words { anchor } => {
                if let Some(w) = self.word_at(q, true) {
                    self.ocr.range = Some((anchor, w));
                }
            }
        }
        if prev != q {
            self.invalidate();
        }
    }

    fn on_up(&mut self, q: P) {
        self.mouse = q;
        let drag = std::mem::replace(&mut self.drag, Drag::None);
        match drag {
            Drag::NewSel { start } => {
                if start.dist(q) < 3.0 || self.sel.is_none_or(|r| r.w() < 2 || r.h() < 2) {
                    if self.phase == Phase::Selecting || self.tool == Tool::Select {
                        // A click picks the window (or monitor) under the cursor.
                        self.sel = Some(self.snap_target());
                    }
                }
                self.phase = Phase::Editing;
                self.after_sel_change();
            }
            Drag::MoveSel { .. } | Drag::ResizeSel { .. } => self.after_sel_change(),
            Drag::Draw => {
                if let Some(cur) = self.current.take() {
                    if !cur.is_degenerate() {
                        self.checkpoint();
                        self.shapes.push(cur);
                    }
                }
            }
            Drag::MoveShape { .. } | Drag::ShapeHandle { .. } => {
                // Drop the undo entry if nothing moved.
                if self.down_at.dist(q) < 0.5 && self.editing.is_none() {
                    if let Some(prev) = self.undo.pop() {
                        self.shapes = prev;
                    }
                }
            }
            _ => {}
        }
    }

    fn on_right(&mut self) {
        if self.current.take().is_some() {
            self.drag = Drag::None;
        } else if self.popup != Popup::None {
            self.popup = Popup::None;
        } else if self.editing.is_some() || self.selected.is_some() {
            self.commit_text();
            self.selected = None;
        } else if self.phase == Phase::Selecting || self.sel.is_none() {
            self.finish(Finish::Cancel);
        } else if self.tool != Tool::Select {
            self.set_tool(Tool::Select);
        } else {
            // Second right click: back to choosing a region.
            self.sel = None;
            self.phase = Phase::Selecting;
            self.reset_ocr();
        }
    }

    fn on_wheel(&mut self, delta: f32) {
        if self.phase != Phase::Editing {
            return;
        }
        let mut st = self.style();
        if key_down(VK_CONTROL) {
            st.opacity = (st.opacity + 0.05 * delta).clamp(0.05, 1.0);
        } else {
            let (lo, hi) = self.style_tool().size_range();
            let step = if st.width >= 20.0 { 2.0 } else { 1.0 };
            st.width = (st.width + step * delta).clamp(lo, hi).round();
        }
        self.set_style(st, false);
    }

    fn nudge(&mut self, dx: i32, dy: i32) {
        if let Some(i) = self.selected {
            self.checkpoint();
            if let Some(s) = self.shapes.get_mut(i) {
                s.translate(p(dx as f32, dy as f32));
            }
        } else if let Some(r) = self.sel {
            let full = self.shot.full();
            let n = if key_down(VK_MENU) {
                RectI::new(r.l, r.t, (r.r + dx).max(r.l + 1), (r.b + dy).max(r.t + 1))
                    .clamp_to(&full)
            } else {
                let dx = dx.clamp(-r.l, full.r - r.r);
                let dy = dy.clamp(-r.t, full.b - r.b);
                RectI::new(r.l + dx, r.t + dy, r.r + dx, r.b + dy)
            };
            self.sel = Some(n);
            self.after_sel_change();
        }
    }

    fn on_key(&mut self, vk: u16) {
        let ctrl = key_down(VK_CONTROL);
        let shift = key_down(VK_SHIFT);
        let vk = VIRTUAL_KEY(vk);

        // Typing into a text shape.
        if let Some(i) = self.editing {
            match vk {
                VK_ESCAPE => {
                    self.commit_text();
                    return;
                }
                VK_RETURN if ctrl => {
                    self.commit_text();
                    return;
                }
                VK_BACK => {
                    if let Some(Shape {
                        kind: Kind::Text { text, ext, .. },
                        ..
                    }) = self.shapes.get_mut(i)
                    {
                        if ctrl {
                            let t = text.trim_end().to_string();
                            let cut = t
                                .rfind(|c: char| c.is_whitespace())
                                .map(|x| x + 1)
                                .unwrap_or(0);
                            text.truncate(cut);
                        } else {
                            text.pop();
                        }
                        ext.set((0.0, 0.0));
                    }
                    return;
                }
                _ if ctrl && vk.0 == 'V' as u16 => {
                    if let Some(t) = encode::paste_text(self.hwnd) {
                        if let Some(Shape {
                            kind: Kind::Text { text, .. },
                            ..
                        }) = self.shapes.get_mut(i)
                        {
                            text.push_str(&t.replace("\r\n", "\n"));
                        }
                    }
                    return;
                }
                _ if !ctrl => return, // characters arrive as WM_CHAR
                _ => {}
            }
        }

        if self.phase == Phase::Selecting {
            match vk {
                VK_ESCAPE => self.finish(Finish::Cancel),
                VK_SPACE | VK_F => {
                    self.sel = Some(
                        self.shot
                            .monitor_at(self.mouse.x as i32, self.mouse.y as i32)
                            .map(|m| m.rect)
                            .unwrap_or(self.shot.full()),
                    );
                    self.phase = Phase::Editing;
                    self.after_sel_change();
                }
                VK_RETURN => {
                    self.sel = Some(self.snap_target());
                    self.phase = Phase::Editing;
                    self.after_sel_change();
                }
                VK_A if ctrl => {
                    self.sel = Some(self.shot.full());
                    self.phase = Phase::Editing;
                    self.after_sel_change();
                }
                VK_C => {
                    if let Some((r, g, b)) =
                        self.shot.pixel(self.mouse.x as i32, self.mouse.y as i32)
                    {
                        let hex = Rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32).hex();
                        if encode::copy_text(self.hwnd, &hex) {
                            self.toast(format!("Copied {hex}"), false);
                        }
                    }
                }
                VK_LEFT => self.move_cursor(-1, 0),
                VK_RIGHT => self.move_cursor(1, 0),
                VK_UP => self.move_cursor(0, -1),
                VK_DOWN => self.move_cursor(0, 1),
                _ => {}
            }
            return;
        }

        let step = if shift { 10 } else { 1 };
        match vk {
            VK_ESCAPE => {
                if self.popup != Popup::None {
                    self.popup = Popup::None;
                } else if self.selected.is_some() {
                    self.selected = None;
                } else if self.ocr.range.is_some() {
                    self.ocr.range = None;
                } else {
                    self.finish(Finish::Cancel);
                }
            }
            VK_RETURN => self.finish(Finish::Save),
            VK_DELETE | VK_BACK => {
                if let Some(i) = self.selected.take() {
                    self.checkpoint();
                    if i < self.shapes.len() {
                        self.shapes.remove(i);
                    }
                }
            }
            VK_LEFT => self.nudge(-step, 0),
            VK_RIGHT => self.nudge(step, 0),
            VK_UP => self.nudge(0, -step),
            VK_DOWN => self.nudge(0, step),
            VK_OEM_4 => self.on_wheel(-1.0),
            VK_OEM_6 => self.on_wheel(1.0),
            _ if ctrl => match vk {
                VK_Z if shift => self.redo_step(),
                VK_Z => self.undo_step(),
                VK_Y => self.redo_step(),
                VK_C => {
                    if self.ocr.mode || (self.tr.on && self.ocr_valid()) {
                        self.copy_text()
                    } else {
                        self.finish(Finish::Copy)
                    }
                }
                VK_S if shift => self.finish(Finish::SaveAs),
                VK_S => self.finish(Finish::Save),
                VK_T => self.toggle_translate(),
                VK_A => {
                    if let Some(res) = &self.ocr.result {
                        if !res.words.is_empty() {
                            self.ocr.mode = true;
                            self.ocr.range = Some((0, res.words.len() - 1));
                        }
                    }
                }
                _ => {}
            },
            VK_O => self.click_ui(Hit::Ocr, RectF::default()),
            VK_I => self.eyedrop(),
            _ => {
                if let Some(t) = Tool::ALL.iter().find(|t| t.key() == vk.0 as u32) {
                    self.set_tool(*t);
                } else if (0x30..=0x39).contains(&vk.0) {
                    let i = if vk.0 == 0x30 {
                        9
                    } else {
                        (vk.0 - 0x31) as usize
                    };
                    if let Some(c) = self.cfg.palette.get(i).copied() {
                        self.set_color(c, true);
                        self.hsv = c.to_hsv();
                    }
                }
            }
        }
    }

    fn move_cursor(&mut self, dx: i32, dy: i32) {
        let mut pt = POINT::default();
        unsafe {
            let _ = GetCursorPos(&mut pt);
            let _ = SetCursorPos(pt.x + dx, pt.y + dy);
        }
    }

    fn on_char(&mut self, c: u16) {
        let Some(i) = self.editing else { return };
        let ch = if (0xD800..0xDC00).contains(&c) {
            self.surrogate = Some(c);
            return;
        } else if (0xDC00..0xE000).contains(&c) {
            match self.surrogate.take() {
                Some(hi) => char::decode_utf16([hi, c]).next().and_then(|r| r.ok()),
                None => None,
            }
        } else {
            char::from_u32(c as u32)
        };
        let Some(ch) = ch else { return };
        let ch = if ch == '\r' { '\n' } else { ch };
        if (ch as u32) < 32 && ch != '\n' {
            return;
        }
        if let Some(Shape {
            kind: Kind::Text { text, .. },
            ..
        }) = self.shapes.get_mut(i)
        {
            text.push(ch);
        }
    }

    // ---------------------------------------------------------------- rendering

    fn ensure_target(&mut self) -> Result<()> {
        if self.rt.is_some() {
            return Ok(());
        }
        unsafe {
            let props = D2D1_RENDER_TARGET_PROPERTIES {
                r#type: D2D1_RENDER_TARGET_TYPE_DEFAULT,
                pixelFormat: D2D1_PIXEL_FORMAT {
                    format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    alphaMode: D2D1_ALPHA_MODE_PREMULTIPLIED,
                },
                dpiX: 96.0,
                dpiY: 96.0,
                usage: D2D1_RENDER_TARGET_USAGE_NONE,
                minLevel: D2D1_FEATURE_LEVEL_DEFAULT,
            };
            let hp = D2D1_HWND_RENDER_TARGET_PROPERTIES {
                hwnd: self.hwnd,
                pixelSize: D2D_SIZE_U {
                    width: self.shot.w as u32,
                    height: self.shot.h as u32,
                },
                presentOptions: D2D1_PRESENT_OPTIONS_NONE,
            };
            let rt = self.painter.d2d.CreateHwndRenderTarget(&props, &hp)?;
            rt.SetTextAntialiasMode(D2D1_TEXT_ANTIALIAS_MODE_GRAYSCALE);
            self.base = Some(export::make_bitmap(
                &rt,
                &self.shot.px,
                self.shot.w,
                self.shot.h,
            )?);
            self.dashed = self.painter.dashed().ok();
            self.fx_bmp.clear();
            self.rt = Some(rt);
        }
        Ok(())
    }

    fn render(&mut self) {
        if self.ensure_target().is_err() {
            return;
        }
        let rt: ID2D1RenderTarget = match self.rt.as_ref() {
            Some(r) => (**r).clone(),
            None => return,
        };
        unsafe {
            rt.BeginDraw();
            rt.SetTransform(&Matrix3x2::identity());
            let full = self.shot.full().f();
            if let Some(b) = &self.base {
                rt.DrawBitmap(
                    b,
                    Some(&rc(full)),
                    1.0,
                    D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                    None,
                );
            }
        }
        let _ = self.draw_scene(&rt);
        unsafe {
            if let Err(e) = rt.EndDraw(None, None) {
                if e.code() == D2DERR_RECREATE_TARGET {
                    self.rt = None;
                    self.base = None;
                    self.fx_bmp.clear();
                    self.invalidate();
                }
            }
        }
    }

    fn draw_scene(&mut self, rt: &ID2D1RenderTarget) -> Result<()> {
        let s = self.s();
        let accent = self.cfg.accent;
        let full = self.shot.full().f();
        let dim = self.painter.brush(rt, rgba(0.0, 0.0, 0.0, self.cfg.dim))?;
        let acc = self.painter.brush(rt, col(accent, 1.0))?;
        let white = self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 1.0))?;

        let hole = match (self.phase, self.sel) {
            (Phase::Editing, Some(r)) => Some(r.f()),
            (Phase::Selecting, _) if matches!(self.drag, Drag::None) => {
                Some(self.snap_target().f())
            }
            _ => None,
        };

        if let (Phase::Editing, Some(sel)) = (self.phase, self.sel) {
            let sf = sel.f();
            unsafe {
                rt.PushAxisAlignedClip(&rc(sf), D2D1_ANTIALIAS_MODE_ALIASED);
            }
            if self.tr.on {
                if let Some(res) = &self.ocr.result {
                    self.painter.draw_translation(rt, &res.blocks);
                }
            }
            // Shapes (and the one being drawn).
            {
                let Overlay {
                    painter,
                    shapes,
                    current,
                    fx,
                    fx_bmp,
                    shot,
                    editing,
                    ..
                } = self;
                let mut src = |tool: Tool, st: u32| -> Option<ID2D1Bitmap> {
                    let key = (tool, FxCache::quantize(tool, st));
                    if let Some(b) = fx_bmp.get(&key) {
                        if !fx.evicted(&key) {
                            return Some(b.clone());
                        }
                    }
                    let px = fx.get(shot, tool, st);
                    let b = export::make_bitmap(rt, px, shot.w, shot.h).ok()?;
                    fx_bmp.retain(|k, _| fx.cpu.contains_key(k));
                    fx_bmp.insert(key, b.clone());
                    Some(b)
                };
                painter.draw_shapes(rt, shapes, &mut src, *editing);
                if let Some(c) = current.as_ref() {
                    let _ = painter.draw_shape(rt, c, &mut src, false);
                }
            }
            // Recognised words.
            if self.ocr.mode {
                if let Some(res) = &self.ocr.result {
                    let soft = self.painter.brush(rt, col(accent, 0.16))?;
                    let strong = self.painter.brush(rt, col(Rgb(0x2F7BFF), 0.42))?;
                    let (a, b) = self
                        .ocr
                        .range
                        .map(|(a, b)| (a.min(b), a.max(b)))
                        .unwrap_or((usize::MAX, 0));
                    for (i, w) in res.words.iter().enumerate() {
                        let br = if i >= a && i <= b { &strong } else { &soft };
                        unsafe {
                            rt.FillRoundedRectangle(&rrect(w.rect.inflate(1.5), 3.0), br);
                        }
                    }
                }
            }
            unsafe {
                rt.PopAxisAlignedClip();
            }
        }

        // Dim everything outside the hole.
        unsafe {
            match hole {
                Some(h) => {
                    for r in [
                        RectF::new(full.l, full.t, full.r, h.t),
                        RectF::new(full.l, h.b, full.r, full.b),
                        RectF::new(full.l, h.t, h.l, h.b),
                        RectF::new(h.r, h.t, full.r, h.b),
                    ] {
                        if r.w() > 0.0 && r.h() > 0.0 {
                            rt.FillRectangle(&rc(r), &dim);
                        }
                    }
                }
                None => rt.FillRectangle(&rc(full), &dim),
            }
        }

        match self.phase {
            Phase::Selecting => {
                if let Some(h) = hole {
                    unsafe {
                        rt.DrawRectangle(
                            &rc(h.inflate(-1.0)),
                            &acc,
                            2.0 * s.min(1.5),
                            None::<&ID2D1StrokeStyle>,
                        );
                    }
                    self.size_label(rt, h, s)?;
                }
                self.draw_hint(rt, s)?;
                if self.cfg.magnifier {
                    self.draw_loupe(rt, s)?;
                }
            }
            Phase::Editing => {
                if let Some(sel) = self.sel {
                    let sf = sel.f();
                    unsafe {
                        rt.DrawRectangle(
                            &rc(sf.inflate(0.5)),
                            &acc,
                            1.0,
                            None::<&ID2D1StrokeStyle>,
                        );
                    }
                    let resizing = matches!(
                        self.drag,
                        Drag::NewSel { .. } | Drag::ResizeSel { .. } | Drag::MoveSel { .. }
                    );
                    if self.tool == Tool::Select && !self.ocr.mode || resizing {
                        for hp in self.sel_handles() {
                            let e = D2D1_ELLIPSE {
                                point: pt(hp),
                                radiusX: 4.5 * s,
                                radiusY: 4.5 * s,
                            };
                            unsafe {
                                rt.FillEllipse(&e, &white);
                                rt.DrawEllipse(&e, &acc, 1.6 * s, None::<&ID2D1StrokeStyle>);
                            }
                        }
                    }
                    if let Some(i) = self.selected {
                        if let Some(sh) = self.shapes.get(i) {
                            let b = sh.bounds().inflate(3.0);
                            unsafe {
                                rt.DrawRectangle(&rc(b), &acc, 1.0, self.dashed.as_ref());
                            }
                            for hp in sh.handles() {
                                let e = D2D1_ELLIPSE {
                                    point: pt(hp),
                                    radiusX: 4.0 * s,
                                    radiusY: 4.0 * s,
                                };
                                unsafe {
                                    rt.FillEllipse(&e, &white);
                                    rt.DrawEllipse(&e, &acc, 1.5 * s, None::<&ID2D1StrokeStyle>);
                                }
                            }
                        }
                    }
                    self.size_label(rt, sf, s)?;
                    if resizing && self.cfg.magnifier {
                        self.draw_loupe(rt, s)?;
                    }
                }
                self.draw_toolbar(rt, s)?;
            }
        }
        self.draw_toast(rt, s)?;
        Ok(())
    }

    fn pill(&self, rt: &ID2D1RenderTarget, r: RectF, alpha: f32) -> Result<()> {
        let bg = self.painter.brush(rt, rgba(0.075, 0.08, 0.095, alpha))?;
        let edge = self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.09))?;
        unsafe {
            rt.FillRoundedRectangle(&rrect(r, r.h().min(14.0)), &bg);
            rt.DrawRoundedRectangle(
                &rrect(r.inflate(-0.5), r.h().min(14.0)),
                &edge,
                1.0,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Ok(())
    }

    fn label(
        &self,
        rt: &ID2D1RenderTarget,
        text: &str,
        at: P,
        size: f32,
        c: D2D1_COLOR_F,
        bold: bool,
    ) -> Result<(f32, f32)> {
        let f = self.painter.format(
            size,
            if bold {
                DWRITE_FONT_WEIGHT_SEMI_BOLD
            } else {
                DWRITE_FONT_WEIGHT_NORMAL
            },
        )?;
        let l = self.painter.layout(text, &f, 4000.0, 400.0)?;
        let m = Painter::metrics(&l);
        unsafe {
            rt.DrawTextLayout(
                pt(at),
                &l,
                &self.painter.brush(rt, c)?,
                D2D1_DRAW_TEXT_OPTIONS_NONE,
            );
        }
        Ok(m)
    }

    fn measure(&self, text: &str, size: f32, bold: bool) -> (f32, f32) {
        self.painter
            .format(
                size,
                if bold {
                    DWRITE_FONT_WEIGHT_SEMI_BOLD
                } else {
                    DWRITE_FONT_WEIGHT_NORMAL
                },
            )
            .and_then(|f| self.painter.layout(text, &f, 4000.0, 400.0))
            .map(|l| Painter::metrics(&l))
            .unwrap_or((0.0, 0.0))
    }

    fn size_label(&self, rt: &ID2D1RenderTarget, r: RectF, s: f32) -> Result<()> {
        let text = format!("{} \u{00D7} {}", r.w().round() as i32, r.h().round() as i32);
        let fs = 12.0 * s;
        let (tw, th) = self.measure(&text, fs, true);
        let (pw, ph) = (tw + 16.0 * s, th + 8.0 * s);
        let mut y = r.t - ph - 6.0 * s;
        if y < 2.0 {
            y = r.t + 6.0 * s;
        }
        let x = r.l.max(2.0);
        self.pill(rt, RectF::xywh(x, y, pw, ph), 0.88)?;
        self.label(
            rt,
            &text,
            p(x + 8.0 * s, y + 4.0 * s),
            fs,
            rgba(1.0, 1.0, 1.0, 0.95),
            true,
        )?;
        Ok(())
    }

    fn draw_hint(&self, rt: &ID2D1RenderTarget, s: f32) -> Result<()> {
        if !matches!(self.drag, Drag::None) {
            return Ok(());
        }
        let Some(m) = self
            .shot
            .monitor_at(self.mouse.x as i32, self.mouse.y as i32)
        else {
            return Ok(());
        };
        let text = "Drag to select   \u{2022}   Click a window   \u{2022}   Space: full screen   \u{2022}   C: copy colour   \u{2022}   Esc: cancel";
        let fs = 13.0 * s;
        let (tw, th) = self.measure(text, fs, false);
        let r = m.rect.f();
        let (pw, ph) = (tw + 28.0 * s, th + 14.0 * s);
        let x = r.cx() - pw * 0.5;
        let y = r.t + 22.0 * s;
        self.pill(rt, RectF::xywh(x, y, pw, ph), 0.86)?;
        self.label(
            rt,
            text,
            p(x + 14.0 * s, y + 7.0 * s),
            fs,
            rgba(1.0, 1.0, 1.0, 0.9),
            false,
        )?;
        Ok(())
    }

    fn draw_loupe(&self, rt: &ID2D1RenderTarget, s: f32) -> Result<()> {
        let Some(base) = &self.base else {
            return Ok(());
        };
        let n = 15.0;
        let box_ = (126.0 * s).round();
        let z = box_ / n;
        let (mx, my) = (self.mouse.x.floor(), self.mouse.y.floor());
        let full = self.shot.full().f();
        let mut x = self.mouse.x + 22.0 * s;
        let mut y = self.mouse.y + 22.0 * s;
        let info_h = 40.0 * s;
        let mon = self
            .shot
            .monitor_at(mx as i32, my as i32)
            .map(|m| m.rect.f())
            .unwrap_or(full);
        if x + box_ > mon.r - 4.0 {
            x = self.mouse.x - 22.0 * s - box_;
        }
        if y + box_ + info_h > mon.b - 4.0 {
            y = self.mouse.y - 22.0 * s - box_ - info_h;
        }
        let r = RectF::xywh(x, y, box_, box_);
        let half = (n - 1.0) * 0.5;
        let src = RectF::new(mx - half, my - half, mx - half + n, my - half + n);
        let dark = self.painter.brush(rt, rgba(0.075, 0.08, 0.095, 0.95))?;
        unsafe {
            rt.FillRectangle(&rc(r.inflate(2.0)), &dark);
            rt.DrawBitmap(
                base,
                Some(&rc(r)),
                1.0,
                D2D1_BITMAP_INTERPOLATION_MODE_NEAREST_NEIGHBOR,
                Some(&rc(src)),
            );
            let grid = self.painter.brush(rt, rgba(0.0, 0.0, 0.0, 0.16))?;
            for i in 1..n as i32 {
                let o = i as f32 * z;
                rt.DrawLine(
                    pt(p(r.l + o, r.t)),
                    pt(p(r.l + o, r.b)),
                    &grid,
                    1.0,
                    None::<&ID2D1StrokeStyle>,
                );
                rt.DrawLine(
                    pt(p(r.l, r.t + o)),
                    pt(p(r.r, r.t + o)),
                    &grid,
                    1.0,
                    None::<&ID2D1StrokeStyle>,
                );
            }
            let c = RectF::xywh(r.l + half * z, r.t + half * z, z, z);
            rt.DrawRectangle(
                &rc(c),
                &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 1.0))?,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
            rt.DrawRectangle(
                &rc(c.inflate(1.5)),
                &self.painter.brush(rt, rgba(0.0, 0.0, 0.0, 0.8))?,
                1.0,
                None::<&ID2D1StrokeStyle>,
            );
            rt.DrawRectangle(
                &rc(r.inflate(1.0)),
                &self.painter.brush(rt, col(self.cfg.accent, 0.9))?,
                1.5,
                None::<&ID2D1StrokeStyle>,
            );
        }
        let ir = RectF::xywh(r.l - 2.0, r.b + 2.0, box_ + 4.0, info_h);
        unsafe {
            rt.FillRectangle(&rc(ir), &dark);
        }
        let fs = 11.0 * s;
        let sx = mx as i32 + self.shot.ox;
        let sy = my as i32 + self.shot.oy;
        self.label(
            rt,
            &format!("{sx}, {sy}"),
            p(ir.l + 8.0 * s, ir.t + 4.0 * s),
            fs,
            rgba(1.0, 1.0, 1.0, 0.75),
            false,
        )?;
        if let Some((cr, cg, cb)) = self.shot.pixel(mx as i32, my as i32) {
            let c = Rgb(((cr as u32) << 16) | ((cg as u32) << 8) | cb as u32);
            let sw = RectF::xywh(ir.l + 8.0 * s, ir.t + 21.0 * s, 12.0 * s, 12.0 * s);
            unsafe {
                rt.FillRectangle(&rc(sw), &self.painter.brush(rt, col(c, 1.0))?);
                rt.DrawRectangle(
                    &rc(sw),
                    &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.5))?,
                    1.0,
                    None::<&ID2D1StrokeStyle>,
                );
            }
            self.label(
                rt,
                &c.hex(),
                p(sw.r + 6.0 * s, ir.t + 19.0 * s),
                fs,
                rgba(1.0, 1.0, 1.0, 0.95),
                true,
            )?;
        }
        Ok(())
    }

    fn draw_toast(&self, rt: &ID2D1RenderTarget, s: f32) -> Result<()> {
        let (text, err) = match (&self.toast, self.ocr.busy, self.tr.busy) {
            (Some((t, at, e)), _, _) if at.elapsed() < Duration::from_millis(2600) => {
                (t.clone(), *e)
            }
            (_, true, _) => ("Recognising text...".to_string(), false),
            (_, _, true) => (
                format!("Translating to {}...", self.tr.target.to_uppercase()),
                false,
            ),
            _ => return Ok(()),
        };
        let fs = 12.5 * s;
        let (tw, th) = self.measure(&text, fs, true);
        let (pw, ph) = (tw + 26.0 * s, th + 12.0 * s);
        let anchor = self
            .layout()
            .first()
            .map(|(r, _)| *r)
            .or_else(|| self.sel.map(|r| r.f()))
            .unwrap_or(self.shot.full().f());
        let mut y = anchor.t - ph - 8.0 * s;
        if y < 4.0 {
            y = anchor.b + 8.0 * s;
        }
        let x = anchor.cx() - pw * 0.5;
        let r = RectF::xywh(x, y, pw, ph);
        self.pill(rt, r, 0.94)?;
        let c = if err {
            rgba(1.0, 0.45, 0.5, 1.0)
        } else {
            col(self.cfg.accent, 1.0)
        };
        unsafe {
            rt.FillEllipse(
                &D2D1_ELLIPSE {
                    point: pt(p(r.l + 11.0 * s, r.cy())),
                    radiusX: 3.0 * s,
                    radiusY: 3.0 * s,
                },
                &self.painter.brush(rt, c)?,
            );
        }
        self.label(
            rt,
            &text,
            p(x + 19.0 * s, y + 6.0 * s),
            fs,
            rgba(1.0, 1.0, 1.0, 0.95),
            true,
        )?;
        Ok(())
    }

    fn draw_toolbar(&self, rt: &ID2D1RenderTarget, s: f32) -> Result<()> {
        let lay = self.layout();
        if lay.is_empty() {
            return Ok(());
        }
        let accent = self.cfg.accent;
        let st = self.style();
        let stool = self.style_tool();
        let fgc = rgba(0.92, 0.94, 0.96, 1.0);
        let hover_b = self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.08))?;
        let active_b = self.painter.brush(rt, col(accent, 0.2))?;
        let mut panels = 0;
        for (r, h) in &lay {
            match h {
                Hit::Panel => {
                    // Soft drop shadow + glassy dark panel.
                    let sh = self.painter.brush(rt, rgba(0.0, 0.0, 0.0, 0.22))?;
                    unsafe {
                        rt.FillRoundedRectangle(
                            &rrect(r.translate(0.0, 3.0 * s).inflate(1.0), 13.0 * s),
                            &sh,
                        );
                    }
                    self.pill(rt, *r, 0.95)?;
                    let fr = 13.0 * s;
                    let bg = self.painter.brush(rt, rgba(0.075, 0.08, 0.095, 0.97))?;
                    unsafe {
                        rt.FillRoundedRectangle(&rrect(*r, fr), &bg);
                        rt.DrawRoundedRectangle(
                            &rrect(r.inflate(-0.5), fr),
                            &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.1))?,
                            1.0,
                            None::<&ID2D1StrokeStyle>,
                        );
                    }
                    if panels == 0 {
                        // Divider between the two rows.
                        if let Some((r1, _)) = lay.iter().find(|(_, x)| matches!(x, Hit::Tool(_))) {
                            let y = r1.b + 2.5 * s;
                            unsafe {
                                rt.DrawLine(
                                    pt(p(r.l + 10.0 * s, y)),
                                    pt(p(r.r - 10.0 * s, y)),
                                    &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.07))?,
                                    1.0,
                                    None::<&ID2D1StrokeStyle>,
                                );
                            }
                        }
                        if self.popup == Popup::Color {
                            // drawn later with the popup panel
                        }
                    } else if self.popup == Popup::Color {
                        self.draw_picker_extra(rt, *r, s)?;
                    }
                    panels += 1;
                }
                Hit::Swatch(i) => {
                    let c = self.cfg.palette[*i];
                    let center = p(r.cx(), r.cy());
                    let rad = 8.0 * s;
                    let on = c == st.color;
                    unsafe {
                        if on {
                            rt.DrawEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(center),
                                    radiusX: rad + 3.0 * s,
                                    radiusY: rad + 3.0 * s,
                                },
                                &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.95))?,
                                1.6 * s,
                                None::<&ID2D1StrokeStyle>,
                            );
                        }
                        rt.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(center),
                                radiusX: rad,
                                radiusY: rad,
                            },
                            &self.painter.brush(rt, col(c, 1.0))?,
                        );
                        rt.DrawEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(center),
                                radiusX: rad,
                                radiusY: rad,
                            },
                            &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.18))?,
                            1.0,
                            None::<&ID2D1StrokeStyle>,
                        );
                    }
                }
                Hit::Custom => {
                    let center = p(r.cx(), r.cy());
                    let rad = 8.0 * s;
                    for k in 0..12 {
                        let a0 = k as f32 / 12.0 * std::f32::consts::TAU;
                        let a1 = (k + 1) as f32 / 12.0 * std::f32::consts::TAU;
                        let c = Rgb::from_hsv(k as f32 * 30.0, 0.85, 1.0);
                        let g = unsafe { self.painter.d2d.CreatePathGeometry() }.ok();
                        if let Some(g) = g {
                            if let Ok(sk) = unsafe { g.Open() } {
                                unsafe {
                                    sk.BeginFigure(pt(center), D2D1_FIGURE_BEGIN_FILLED);
                                    sk.AddLine(pt(p(
                                        center.x + a0.cos() * (rad + 2.0 * s),
                                        center.y + a0.sin() * (rad + 2.0 * s),
                                    )));
                                    sk.AddLine(pt(p(
                                        center.x + a1.cos() * (rad + 2.0 * s),
                                        center.y + a1.sin() * (rad + 2.0 * s),
                                    )));
                                    sk.EndFigure(D2D1_FIGURE_END_CLOSED);
                                    let _ = sk.Close();
                                    rt.FillGeometry(
                                        &g,
                                        &self.painter.brush(rt, col(c, 1.0))?,
                                        None::<&ID2D1Brush>,
                                    );
                                }
                            }
                        }
                    }
                    let custom = !self.cfg.palette.contains(&st.color);
                    unsafe {
                        rt.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(center),
                                radiusX: rad - 2.5 * s,
                                radiusY: rad - 2.5 * s,
                            },
                            &self.painter.brush(rt, col(st.color, 1.0))?,
                        );
                        if custom || self.popup == Popup::Color {
                            rt.DrawEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(center),
                                    radiusX: rad + 4.0 * s,
                                    radiusY: rad + 4.0 * s,
                                },
                                &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.95))?,
                                1.6 * s,
                                None::<&ID2D1StrokeStyle>,
                            );
                        }
                    }
                }
                Hit::Size | Hit::Opacity => {
                    let (lo, hi) = stool.size_range();
                    let (text, t) = if *h == Hit::Size {
                        (
                            format!("{} {}", stool.size_label(), st.width.round() as i32),
                            (((st.width - lo) / (hi - lo)).max(0.0)).sqrt(),
                        )
                    } else {
                        (
                            format!("Opacity {}%", (st.opacity * 100.0).round() as i32),
                            (st.opacity - 0.05) / 0.95,
                        )
                    };
                    let (a, b) = self.slider_track(*r);
                    let fs = 11.5 * s;
                    let (_, th) = self.measure(&text, fs, false);
                    self.label(
                        rt,
                        &text,
                        p(r.l + 4.0 * s, r.cy() - th * 0.5),
                        fs,
                        rgba(1.0, 1.0, 1.0, 0.82),
                        false,
                    )?;
                    let y = r.cy();
                    let kx = a + (b - a) * t.clamp(0.0, 1.0);
                    unsafe {
                        rt.DrawLine(
                            pt(p(a, y)),
                            pt(p(b, y)),
                            &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 0.16))?,
                            4.0 * s,
                            &self.painter.round,
                        );
                        rt.DrawLine(
                            pt(p(a, y)),
                            pt(p(kx, y)),
                            &self.painter.brush(rt, col(accent, 1.0))?,
                            4.0 * s,
                            &self.painter.round,
                        );
                        rt.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(p(kx, y)),
                                radiusX: 6.5 * s,
                                radiusY: 6.5 * s,
                            },
                            &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 1.0))?,
                        );
                    }
                }
                Hit::PickSv => self.draw_sv(rt, *r, s)?,
                Hit::PickHue => self.draw_hue(rt, *r, s)?,
                Hit::LangItem(i) => {
                    let code = &self.cfg.translate_languages[*i];
                    let on = *code == self.tr.target;
                    if on || self.hover == Some(*h) {
                        unsafe {
                            rt.FillRoundedRectangle(
                                &rrect(*r, 6.0 * s),
                                if on { &active_b } else { &hover_b },
                            );
                        }
                    }
                    let fs = 12.0 * s;
                    let label = format!("{}  {}", code.to_uppercase(), lang_name(code));
                    let (_, th) = self.measure(&label, fs, false);
                    self.label(
                        rt,
                        &label,
                        p(r.l + 8.0 * s, r.cy() - th * 0.5),
                        fs,
                        if on { col(accent, 1.0) } else { fgc },
                        on,
                    )?;
                }
                _ => {
                    let active = match h {
                        Hit::Tool(t) => *t == self.tool && !self.ocr.mode,
                        Hit::Ocr => self.ocr.mode,
                        Hit::Translate => self.tr.on,
                        Hit::Shadow => st.shadow,
                        Hit::Fill => st.fill != Fill::None,
                        Hit::Lang => self.popup == Popup::Lang,
                        _ => false,
                    };
                    let disabled = match h {
                        Hit::Undo => self.undo.is_empty(),
                        Hit::Redo => self.redo.is_empty(),
                        Hit::Fill => !stool.has_fill(),
                        _ => false,
                    };
                    let br = r.inflate(-2.0 * s);
                    unsafe {
                        if active {
                            rt.FillRoundedRectangle(&rrect(br, 8.0 * s), &active_b);
                        } else if self.hover == Some(*h) && !disabled {
                            rt.FillRoundedRectangle(&rrect(br, 8.0 * s), &hover_b);
                        }
                    }
                    let c = if disabled {
                        rgba(1.0, 1.0, 1.0, 0.28)
                    } else if active || *h == Hit::Done {
                        col(accent, 1.0)
                    } else {
                        fgc
                    };
                    self.icon(rt, *h, *r, c, s, &st)?;
                    // Dot when text was found in the selection.
                    if *h == Hit::Ocr
                        && self.ocr_valid()
                        && self
                            .ocr
                            .result
                            .as_ref()
                            .is_some_and(|r| !r.words.is_empty())
                    {
                        unsafe {
                            rt.FillEllipse(
                                &D2D1_ELLIPSE {
                                    point: pt(p(r.r - 8.0 * s, r.t + 8.0 * s)),
                                    radiusX: 2.6 * s,
                                    radiusY: 2.6 * s,
                                },
                                &self.painter.brush(rt, col(accent, 1.0))?,
                            );
                        }
                    }
                }
            }
        }
        // Tooltip.
        if let Some(h) = self.hover {
            if let (Some(tip), Some((r, _))) = (h.tip(), lay.iter().find(|(_, x)| *x == h)) {
                let fs = 11.5 * s;
                let (tw, th) = self.measure(tip, fs, false);
                let (pw, ph) = (tw + 16.0 * s, th + 8.0 * s);
                let panel = lay[0].0;
                let above = panel.t - ph - 6.0 * s > 0.0;
                let y = if above {
                    panel.t - ph - 6.0 * s
                } else {
                    panel.b + 6.0 * s
                };
                let x = (r.cx() - pw * 0.5).max(2.0);
                if !(self.toast.is_some() || self.ocr.busy || self.tr.busy) {
                    self.pill(rt, RectF::xywh(x, y, pw, ph), 0.95)?;
                    self.label(
                        rt,
                        tip,
                        p(x + 8.0 * s, y + 4.0 * s),
                        fs,
                        rgba(1.0, 1.0, 1.0, 0.92),
                        false,
                    )?;
                }
            }
        }
        Ok(())
    }

    fn draw_picker_extra(&self, rt: &ID2D1RenderTarget, panel: RectF, s: f32) -> Result<()> {
        let c = self.style().color;
        let y = panel.b - 26.0 * s;
        let sw = RectF::xywh(panel.l + 10.0 * s, y, 16.0 * s, 16.0 * s);
        unsafe {
            rt.FillRoundedRectangle(&rrect(sw, 4.0 * s), &self.painter.brush(rt, col(c, 1.0))?);
        }
        self.label(
            rt,
            &c.hex(),
            p(sw.r + 8.0 * s, y - 1.0 * s),
            12.0 * s,
            rgba(1.0, 1.0, 1.0, 0.92),
            true,
        )?;
        self.label(
            rt,
            "I: pick from screen",
            p(panel.l + 96.0 * s, y),
            10.5 * s,
            rgba(1.0, 1.0, 1.0, 0.5),
            false,
        )?;
        Ok(())
    }

    fn gradient(
        &self,
        rt: &ID2D1RenderTarget,
        a: P,
        b: P,
        stops: &[(f32, D2D1_COLOR_F)],
    ) -> Result<ID2D1LinearGradientBrush> {
        let gs: Vec<D2D1_GRADIENT_STOP> = stops
            .iter()
            .map(|(p, c)| D2D1_GRADIENT_STOP {
                position: *p,
                color: *c,
            })
            .collect();
        unsafe {
            let coll =
                rt.CreateGradientStopCollection(&gs, D2D1_GAMMA_2_2, D2D1_EXTEND_MODE_CLAMP)?;
            rt.CreateLinearGradientBrush(
                &D2D1_LINEAR_GRADIENT_BRUSH_PROPERTIES {
                    startPoint: pt(a),
                    endPoint: pt(b),
                },
                None,
                &coll,
            )
        }
    }

    fn draw_sv(&self, rt: &ID2D1RenderTarget, r: RectF, s: f32) -> Result<()> {
        let hue = Rgb::from_hsv(self.hsv.0, 1.0, 1.0);
        let rr = rrect(r, 6.0 * s);
        let wb = self.gradient(
            rt,
            p(r.l, r.t),
            p(r.r, r.t),
            &[(0.0, rgba(1.0, 1.0, 1.0, 1.0)), (1.0, col(hue, 1.0))],
        )?;
        let kb = self.gradient(
            rt,
            p(r.l, r.t),
            p(r.l, r.b),
            &[
                (0.0, rgba(0.0, 0.0, 0.0, 0.0)),
                (1.0, rgba(0.0, 0.0, 0.0, 1.0)),
            ],
        )?;
        unsafe {
            rt.FillRoundedRectangle(&rr, &wb);
            rt.FillRoundedRectangle(&rr, &kb);
        }
        let k = p(r.l + self.hsv.1 * r.w(), r.t + (1.0 - self.hsv.2) * r.h());
        unsafe {
            let e = D2D1_ELLIPSE {
                point: pt(k),
                radiusX: 6.0 * s,
                radiusY: 6.0 * s,
            };
            rt.DrawEllipse(
                &e,
                &self.painter.brush(rt, rgba(0.0, 0.0, 0.0, 0.5))?,
                3.0 * s,
                None::<&ID2D1StrokeStyle>,
            );
            rt.DrawEllipse(
                &e,
                &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 1.0))?,
                1.8 * s,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Ok(())
    }

    fn draw_hue(&self, rt: &ID2D1RenderTarget, r: RectF, s: f32) -> Result<()> {
        let stops: Vec<(f32, D2D1_COLOR_F)> = (0..=6)
            .map(|i| {
                (
                    i as f32 / 6.0,
                    col(Rgb::from_hsv(i as f32 * 60.0, 1.0, 1.0), 1.0),
                )
            })
            .collect();
        let g = self.gradient(rt, p(r.l, r.t), p(r.l, r.b), &stops)?;
        unsafe {
            rt.FillRoundedRectangle(&rrect(r, 6.0 * s), &g);
            let y = r.t + self.hsv.0 / 360.0 * r.h();
            let k = RectF::new(r.l - 2.0 * s, y - 3.0 * s, r.r + 2.0 * s, y + 3.0 * s);
            rt.DrawRoundedRectangle(
                &rrect(k, 3.0 * s),
                &self.painter.brush(rt, rgba(1.0, 1.0, 1.0, 1.0))?,
                2.0 * s,
                None::<&ID2D1StrokeStyle>,
            );
        }
        Ok(())
    }

    /// Vector toolbar icons on a 20x20 grid centred in `r`.
    fn icon(
        &self,
        rt: &ID2D1RenderTarget,
        h: Hit,
        r: RectF,
        c: D2D1_COLOR_F,
        s: f32,
        st: &Style,
    ) -> Result<()> {
        let (cx, cy) = (r.cx(), r.cy());
        let k = s;
        let m = |x: f32, y: f32| p(cx + x * k, cy + y * k);
        let br = self.painter.brush(rt, c)?;
        let lw = 1.7 * k;
        let rs = &self.painter.round;
        let poly = |pts: &[(f32, f32)], closed: bool, fill: bool, w: f32| -> Result<()> {
            let g = unsafe { self.painter.d2d.CreatePathGeometry()? };
            unsafe {
                let sk = g.Open()?;
                sk.BeginFigure(
                    pt(m(pts[0].0, pts[0].1)),
                    if fill {
                        D2D1_FIGURE_BEGIN_FILLED
                    } else {
                        D2D1_FIGURE_BEGIN_HOLLOW
                    },
                );
                let v: Vec<D2D_POINT_2F> = pts[1..].iter().map(|q| pt(m(q.0, q.1))).collect();
                sk.AddLines(&v);
                sk.EndFigure(if closed {
                    D2D1_FIGURE_END_CLOSED
                } else {
                    D2D1_FIGURE_END_OPEN
                });
                sk.Close()?;
                if fill {
                    rt.FillGeometry(&g, &br, None::<&ID2D1Brush>);
                }
                rt.DrawGeometry(&g, &br, w, rs);
            }
            Ok(())
        };
        let line = |a: (f32, f32), b: (f32, f32), w: f32| unsafe {
            rt.DrawLine(pt(m(a.0, a.1)), pt(m(b.0, b.1)), &br, w, rs)
        };
        match h {
            Hit::Tool(Tool::Select) => poly(
                &[
                    (-4.0, -8.0),
                    (-4.0, 7.0),
                    (0.0, 3.2),
                    (2.6, 8.6),
                    (4.8, 7.6),
                    (2.2, 2.2),
                    (7.0, 2.0),
                ],
                true,
                true,
                1.0,
            )?,
            Hit::Tool(Tool::Pen) => {
                let g = unsafe { self.painter.d2d.CreatePathGeometry()? };
                unsafe {
                    let sk = g.Open()?;
                    sk.BeginFigure(pt(m(-7.5, 5.0)), D2D1_FIGURE_BEGIN_HOLLOW);
                    sk.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: pt(m(-4.0, -9.0)),
                        point2: pt(m(1.0, 10.0)),
                        point3: pt(m(7.5, -5.0)),
                    });
                    sk.EndFigure(D2D1_FIGURE_END_OPEN);
                    sk.Close()?;
                    rt.DrawGeometry(&g, &br, 2.0 * k, rs);
                }
            }
            Hit::Tool(Tool::Line) => line((-7.0, 7.0), (7.0, -7.0), 2.0 * k),
            Hit::Tool(Tool::Arrow) => {
                let g = self
                    .painter
                    .arrow_path(m(-7.0, 7.0), m(7.5, -7.5), 2.2 * k)?;
                unsafe {
                    rt.FillGeometry(&g, &br, None::<&ID2D1Brush>);
                }
            }
            Hit::Tool(Tool::Rect) => unsafe {
                rt.DrawRoundedRectangle(
                    &rrect(
                        RectF::new(cx - 7.5 * k, cy - 5.5 * k, cx + 7.5 * k, cy + 5.5 * k),
                        2.5 * k,
                    ),
                    &br,
                    lw,
                    rs,
                );
            },
            Hit::Tool(Tool::Ellipse) => unsafe {
                rt.DrawEllipse(
                    &D2D1_ELLIPSE {
                        point: pt(m(0.0, 0.0)),
                        radiusX: 7.5 * k,
                        radiusY: 6.0 * k,
                    },
                    &br,
                    lw,
                    rs,
                );
            },
            Hit::Tool(Tool::Marker) => {
                let hb = self
                    .painter
                    .brush(rt, D2D1_COLOR_F { a: c.a * 0.55, ..c })?;
                unsafe {
                    rt.DrawLine(pt(m(-6.0, 4.0)), pt(m(6.0, -4.0)), &hb, 6.5 * k, rs);
                }
                line((-7.5, 8.0), (7.5, 8.0), 1.4 * k);
            }
            Hit::Tool(Tool::Text) => {
                line((-6.5, -7.0), (6.5, -7.0), 2.2 * k);
                line((0.0, -7.0), (0.0, 8.0), 2.2 * k);
            }
            Hit::Tool(Tool::Counter) => {
                let rad = 8.0 * k;
                unsafe {
                    rt.FillEllipse(
                        &D2D1_ELLIPSE {
                            point: pt(m(0.0, 0.0)),
                            radiusX: rad,
                            radiusY: rad,
                        },
                        &br,
                    );
                }
                let f = self.painter.format(11.0 * k, DWRITE_FONT_WEIGHT_BOLD)?;
                unsafe {
                    let _ = f.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_CENTER);
                    let _ = f.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_CENTER);
                }
                let l = self.painter.layout("1", &f, rad * 2.0, rad * 2.0)?;
                unsafe {
                    rt.DrawTextLayout(
                        pt(m(-8.0, -8.4)),
                        &l,
                        &self.painter.brush(rt, rgba(0.07, 0.08, 0.1, 1.0))?,
                        D2D1_DRAW_TEXT_OPTIONS_NONE,
                    );
                }
            }
            Hit::Tool(Tool::Blur) => {
                for (i, (x, y)) in [
                    (-5.0, -5.0),
                    (0.0, -5.0),
                    (5.0, -5.0),
                    (-5.0, 0.0),
                    (0.0, 0.0),
                    (5.0, 0.0),
                    (-5.0, 5.0),
                    (0.0, 5.0),
                    (5.0, 5.0),
                ]
                .iter()
                .enumerate()
                {
                    let a = [0.35, 0.6, 0.35, 0.6, 1.0, 0.6, 0.35, 0.6, 0.35][i];
                    unsafe {
                        rt.FillEllipse(
                            &D2D1_ELLIPSE {
                                point: pt(m(*x, *y)),
                                radiusX: 2.3 * k,
                                radiusY: 2.3 * k,
                            },
                            &self.painter.brush(rt, D2D1_COLOR_F { a: c.a * a, ..c })?,
                        );
                    }
                }
            }
            Hit::Tool(Tool::Pixelate) => {
                for yy in 0..3 {
                    for xx in 0..3 {
                        let a = if (xx + yy) % 2 == 0 { 1.0 } else { 0.35 };
                        let q = m(-7.5 + xx as f32 * 5.0, -7.5 + yy as f32 * 5.0);
                        unsafe {
                            rt.FillRectangle(
                                &rc(RectF::xywh(q.x, q.y, 5.0 * k, 5.0 * k)),
                                &self.painter.brush(rt, D2D1_COLOR_F { a: c.a * a, ..c })?,
                            );
                        }
                    }
                }
            }
            Hit::Ocr => {
                for (sx, sy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                    poly(
                        &[
                            (sx * 8.0, sy * 4.0),
                            (sx * 8.0, sy * 8.0),
                            (sx * 4.0, sy * 8.0),
                        ],
                        false,
                        false,
                        1.6 * k,
                    )?;
                }
                line((-4.0, -3.0), (4.0, -3.0), 1.6 * k);
                line((-4.0, 0.5), (4.0, 0.5), 1.6 * k);
                line((-4.0, 4.0), (1.5, 4.0), 1.6 * k);
            }
            Hit::Translate => {
                let f = self
                    .painter
                    .format(10.5 * k, DWRITE_FONT_WEIGHT_SEMI_BOLD)?;
                let l = self.painter.layout("\u{6587}", &f, 40.0 * k, 40.0 * k)?;
                unsafe {
                    rt.DrawTextLayout(pt(m(-9.0, -10.0)), &l, &br, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
                let f2 = self.painter.format(11.5 * k, DWRITE_FONT_WEIGHT_BOLD)?;
                let l2 = self.painter.layout("A", &f2, 40.0 * k, 40.0 * k)?;
                unsafe {
                    rt.DrawTextLayout(pt(m(1.0, -3.0)), &l2, &br, D2D1_DRAW_TEXT_OPTIONS_NONE);
                }
            }
            Hit::Lang => {
                let code = self.tr.target.to_uppercase();
                let fs = 11.5 * k;
                let (tw, th) = self.measure(&code, fs, true);
                self.label(
                    rt,
                    &code,
                    p(cx - tw * 0.5 - 3.0 * k, cy - th * 0.5),
                    fs,
                    c,
                    true,
                )?;
                poly(
                    &[
                        (tw * 0.5 / k + 0.5, -1.0),
                        (tw * 0.5 / k + 3.0, 1.5),
                        (tw * 0.5 / k + 5.5, -1.0),
                    ],
                    false,
                    false,
                    1.4 * k,
                )?;
            }
            Hit::Undo | Hit::Redo => {
                let sgn = if h == Hit::Undo { 1.0 } else { -1.0 };
                let g = unsafe { self.painter.d2d.CreatePathGeometry()? };
                unsafe {
                    let sk = g.Open()?;
                    sk.BeginFigure(pt(m(sgn * 6.5, 6.5)), D2D1_FIGURE_BEGIN_HOLLOW);
                    sk.AddBezier(&D2D1_BEZIER_SEGMENT {
                        point1: pt(m(sgn * 7.5, -2.0)),
                        point2: pt(m(sgn * 2.0, -5.0)),
                        point3: pt(m(-sgn * 6.0, -3.0)),
                    });
                    sk.EndFigure(D2D1_FIGURE_END_OPEN);
                    sk.Close()?;
                    rt.DrawGeometry(&g, &br, 1.8 * k, rs);
                }
                poly(
                    &[(-sgn * 2.5, -7.0), (-sgn * 6.5, -3.0), (-sgn * 2.5, 1.0)],
                    false,
                    false,
                    1.8 * k,
                )?;
            }
            Hit::Save => {
                line((0.0, -8.0), (0.0, 3.5), 1.8 * k);
                poly(
                    &[(-4.5, -1.0), (0.0, 3.5), (4.5, -1.0)],
                    false,
                    false,
                    1.8 * k,
                )?;
                poly(
                    &[(-7.5, 3.0), (-7.5, 7.5), (7.5, 7.5), (7.5, 3.0)],
                    false,
                    false,
                    1.8 * k,
                )?;
            }
            Hit::Copy => unsafe {
                rt.DrawRoundedRectangle(
                    &rrect(
                        RectF::new(cx - 7.0 * k, cy - 3.0 * k, cx + 3.0 * k, cy + 7.5 * k),
                        2.0 * k,
                    ),
                    &br,
                    lw,
                    rs,
                );
                rt.DrawRoundedRectangle(
                    &rrect(
                        RectF::new(cx - 3.0 * k, cy - 7.5 * k, cx + 7.0 * k, cy + 3.0 * k),
                        2.0 * k,
                    ),
                    &br,
                    lw,
                    rs,
                );
            },
            Hit::Close => {
                line((-6.0, -6.0), (6.0, 6.0), 2.0 * k);
                line((6.0, -6.0), (-6.0, 6.0), 2.0 * k);
            }
            Hit::Done => poly(
                &[(-7.5, 0.5), (-2.5, 5.5), (7.5, -5.5)],
                false,
                false,
                2.3 * k,
            )?,
            Hit::Fill => {
                let b = RectF::new(cx - 7.0 * k, cy - 7.0 * k, cx + 7.0 * k, cy + 7.0 * k);
                unsafe {
                    match st.fill {
                        Fill::Solid => rt.FillRoundedRectangle(&rrect(b, 3.0 * k), &br),
                        Fill::Soft => rt.FillRoundedRectangle(
                            &rrect(b, 3.0 * k),
                            &self
                                .painter
                                .brush(rt, D2D1_COLOR_F { a: c.a * 0.35, ..c })?,
                        ),
                        Fill::None => {}
                    }
                    rt.DrawRoundedRectangle(&rrect(b, 3.0 * k), &br, lw, rs);
                }
            }
            Hit::Shadow => unsafe {
                let b = RectF::new(cx - 7.0 * k, cy - 7.0 * k, cx + 4.0 * k, cy + 4.0 * k);
                rt.FillRoundedRectangle(
                    &rrect(b.translate(3.0 * k, 3.0 * k), 3.0 * k),
                    &self
                        .painter
                        .brush(rt, D2D1_COLOR_F { a: c.a * 0.35, ..c })?,
                );
                rt.FillRoundedRectangle(
                    &rrect(b, 3.0 * k),
                    &self.painter.brush(rt, rgba(0.075, 0.08, 0.095, 1.0))?,
                );
                rt.DrawRoundedRectangle(&rrect(b, 3.0 * k), &br, lw, rs);
            },
            _ => {}
        }
        Ok(())
    }
}

pub fn lang_name(code: &str) -> &'static str {
    match code.to_ascii_lowercase().as_str() {
        "ru" => "Russian",
        "en" => "English",
        "uk" => "Ukrainian",
        "be" => "Belarusian",
        "kk" => "Kazakh",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" => "Portuguese",
        "pl" => "Polish",
        "tr" => "Turkish",
        "zh-cn" | "zh" => "Chinese",
        "zh-tw" => "Chinese (Trad.)",
        "ja" => "Japanese",
        "ko" => "Korean",
        "ar" => "Arabic",
        "hi" => "Hindi",
        "nl" => "Dutch",
        "cs" => "Czech",
        "sv" => "Swedish",
        "fi" => "Finnish",
        "el" => "Greek",
        "he" | "iw" => "Hebrew",
        "vi" => "Vietnamese",
        "th" => "Thai",
        "id" => "Indonesian",
        _ => "",
    }
}
