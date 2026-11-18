//! Screenshot history: an index in %LOCALAPPDATA%\Refract-screener\history.json and
//! the History window, a virtualised grid of previews drawn with Direct2D like the
//! Settings window.
//!
//! Every shot gets one JPEG preview (`toast::thumbnail`), shared with its notification.
//! The window decodes only the previews it shows, on a background thread, straight to
//! the card size, and keeps a bounded cache of them. Entries whose file was deleted
//! are dropped lazily when the window opens (if they have no live link).

use crate::encode::{self, Image};
use crate::gdi::*;
use crate::share::Link;
use crate::win::{hwnd_from, hwnd_to, log, shell_open, shell_select, W};
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use windows::core::w;
use windows::Win32::Foundation::*;
use windows::Win32::Graphics::Direct2D::Common::*;
use windows::Win32::Graphics::Direct2D::*;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::HiDpi::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// Entries kept; older ones leave the index (their files stay where they are).
pub const MAX: usize = 500;

const WM_APP_HIST_THUMB: u32 = WM_APP + 82; // lparam = Box<Decoded>
const WM_APP_HIST_COPY: u32 = WM_APP + 83; // lparam = Box<(u64, Option<(Image, Option<Vec<u8>>)>)>
const WM_APP_HIST_UNSHARED: u32 = WM_APP + 84; // lparam = Box<(u64, Result<(), String>)>
const WM_APP_HIST_CLEAN: u32 = WM_APP + 85; // lparam = Box<Vec<u64>> entries whose file is gone
const TIMER_SCROLL: usize = 1;
const TIMER_STATUS: usize = 2;

// ---------------------------------------------------------------- index

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq)]
#[serde(default)]
pub struct Entry {
    pub id: u64,
    /// Saved file; empty when the shot was only shared.
    pub path: String,
    /// Unix seconds.
    pub time: u64,
    /// File size in bytes.
    pub size: u64,
    pub w: u32,
    pub h: u32,
    /// Preview file name in `toast::thumbs_dir()`.
    pub thumb: String,
    pub url: String,
    pub delete_token: String,
    /// Link expiry, Unix seconds (0 = unknown).
    pub expires: u64,
    /// The file is gone (found by the lazy check); not stored.
    #[serde(skip)]
    pub missing: bool,
}

impl Entry {
    pub fn link_alive(&self, now: u64) -> bool {
        !self.url.is_empty() && (self.expires == 0 || self.expires > now)
    }
    pub fn file(&self) -> Option<PathBuf> {
        (!self.path.is_empty() && !self.missing).then(|| PathBuf::from(&self.path))
    }
    /// The link id: last path segment of the URL.
    pub fn link_id(&self) -> &str {
        self.url.rsplit('/').next().unwrap_or("")
    }
}

#[derive(Serialize, Deserialize, Default)]
#[serde(default)]
struct Disk {
    version: u32,
    entries: Vec<Entry>,
}

pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// The index file plus the previews it owns. Oldest entry first.
pub struct Store {
    path: PathBuf,
    thumbs: PathBuf,
    pub entries: Vec<Entry>,
    last_id: u64,
}

impl Store {
    pub fn load(path: PathBuf, thumbs: PathBuf) -> Store {
        let entries = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice::<Disk>(&b).ok())
            .map(|d| d.entries)
            .unwrap_or_default();
        let last_id = entries.iter().map(|e| e.id).max().unwrap_or(0);
        Store {
            path,
            thumbs,
            entries,
            last_id,
        }
    }

    pub fn save(&self) {
        let disk = Disk {
            version: 1,
            entries: self.entries.clone(),
        };
        let Ok(text) = serde_json::to_vec(&disk) else {
            return;
        };
        if let Some(d) = self.path.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        let tmp = self.path.with_extension("json.tmp");
        if std::fs::write(&tmp, text).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }

    /// A fresh id: milliseconds since 1970, strictly increasing.
    pub fn new_id(&mut self) -> u64 {
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        self.last_id = ms.max(self.last_id + 1);
        self.last_id
    }

    fn drop_thumb(&self, e: &Entry) {
        if !e.thumb.is_empty() && !e.thumb.contains(['/', '\\']) {
            let _ = std::fs::remove_file(self.thumbs.join(&e.thumb));
        }
    }

    pub fn add(&mut self, e: Entry) {
        self.last_id = self.last_id.max(e.id);
        self.entries.retain(|x| x.id != e.id);
        self.entries.push(e);
        let extra = self.entries.len().saturating_sub(MAX);
        for old in self.entries.drain(..extra).collect::<Vec<_>>() {
            self.drop_thumb(&old);
        }
    }

    pub fn get(&self, id: u64) -> Option<&Entry> {
        self.entries.iter().find(|e| e.id == id)
    }

    pub fn get_mut(&mut self, id: u64) -> Option<&mut Entry> {
        self.entries.iter_mut().find(|e| e.id == id)
    }

    pub fn remove(&mut self, id: u64) -> Option<Entry> {
        let i = self.entries.iter().position(|e| e.id == id)?;
        let e = self.entries.remove(i);
        self.drop_thumb(&e);
        Some(e)
    }

    /// `gone`: entries whose file no longer exists. They leave the index unless their
    /// link still works (then they stay, marked missing).
    pub fn prune(&mut self, gone: &[u64], now: u64) -> bool {
        let gone: HashSet<u64> = gone.iter().copied().collect();
        let mut changed = false;
        for e in self.entries.iter_mut() {
            if gone.contains(&e.id) {
                e.missing = true;
            }
        }
        let drop: Vec<u64> = self
            .entries
            .iter()
            .filter(|e| (e.missing || e.path.is_empty()) && !e.link_alive(now))
            .map(|e| e.id)
            .collect();
        for id in drop {
            self.remove(id);
            changed = true;
        }
        changed
    }

    /// Preview files no entry refers to (shots that were only copied, old versions).
    pub fn orphans(&self, older_than: std::time::Duration) -> Vec<PathBuf> {
        let used: HashSet<&str> = self.entries.iter().map(|e| e.thumb.as_str()).collect();
        let Ok(rd) = std::fs::read_dir(&self.thumbs) else {
            return Vec::new();
        };
        rd.flatten()
            .filter(|d| {
                let name = d.file_name();
                let old = d
                    .metadata()
                    .and_then(|m| m.modified())
                    .ok()
                    .and_then(|t| t.elapsed().ok())
                    .is_some_and(|age| age > older_than);
                old && !used.contains(name.to_string_lossy().as_ref())
            })
            .map(|d| d.path())
            .collect()
    }

    pub fn thumb_path(&self, e: &Entry) -> Option<PathBuf> {
        (!e.thumb.is_empty()).then(|| self.thumbs.join(&e.thumb))
    }
}

fn index_path() -> PathBuf {
    crate::toast::thumbs_dir()
        .parent()
        .map(|d| d.join("history.json"))
        .unwrap_or_else(|| PathBuf::from("history.json"))
}

thread_local! {
    static STORE: RefCell<Option<Store>> = const { RefCell::new(None) };
    /// Uploads in flight: entry id -> progress in 1/1000.
    static UPLOADS: RefCell<HashMap<u64, u16>> = RefCell::new(HashMap::new());
}

fn store<R>(f: impl FnOnce(&mut Store) -> R) -> R {
    STORE.with(|s| {
        let mut s = s.borrow_mut();
        let st = s.get_or_insert_with(|| Store::load(index_path(), crate::toast::thumbs_dir()));
        f(st)
    })
}

pub fn new_id() -> u64 {
    store(|s| s.new_id())
}

/// Records a shot (main thread).
pub fn add(e: Entry) {
    store(|s| {
        s.add(e);
        s.save();
    });
    refresh();
}

pub fn thumb_path(id: u64) -> Option<PathBuf> {
    store(|s| s.get(id).and_then(|e| s.thumb_path(e)))
}

pub fn file_name(thumb: &Path) -> String {
    thumb
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// Newest entry, for "Share last screenshot".
pub fn last() -> Option<Entry> {
    store(|s| s.entries.last().cloned())
}

pub fn uploading(id: u64) -> bool {
    UPLOADS.with(|u| u.borrow().contains_key(&id))
}

pub fn set_progress(id: u64, permille: u16) {
    UPLOADS.with(|u| u.borrow_mut().insert(id, permille.min(1000)));
    refresh();
}

/// An upload finished: Some = the new link, None = it failed.
pub fn set_link(id: u64, link: Option<&Link>) {
    UPLOADS.with(|u| u.borrow_mut().remove(&id));
    store(|s| {
        match link {
            Some(l) => {
                if let Some(e) = s.get_mut(id) {
                    e.url = l.url.clone();
                    e.delete_token = l.delete_token.clone();
                    e.expires = l.expires;
                }
            }
            None => {
                // A shot that was only being shared has nothing left to show.
                if s.get(id)
                    .is_some_and(|e| e.path.is_empty() && e.url.is_empty())
                {
                    s.remove(id);
                }
            }
        }
        s.save();
    });
    refresh();
}

/// Starts sharing an entry: copies the link again when it still works, otherwise
/// uploads the file. Returns false when there is nothing to share.
pub fn share(main: HWND, id: u64) -> bool {
    let Some(base) = crate::update_base() else {
        return false;
    };
    let Some(e) = store(|s| s.get(id).cloned()) else {
        return false;
    };
    if uploading(id) {
        return true;
    }
    if e.link_alive(now()) {
        let link = Link {
            id: e.link_id().to_string(),
            url: e.url.clone(),
            delete_token: e.delete_token.clone(),
            expires: e.expires,
        };
        crate::share::finished(
            main,
            crate::share::Done {
                entry: id,
                result: Ok(link),
            },
            false,
        );
        return true;
    }
    let Some(path) = e.file() else { return false };
    set_progress(id, 0);
    crate::share::start_file(main, id, base, path);
    true
}

// ---------------------------------------------------------------- decoding

/// Decodes a PNG (Windows WIC) or WebP (libwebp) to opaque BGRA.
pub fn decode(data: &[u8]) -> Option<Image> {
    if crate::share::kind(data) == Some("image/webp") {
        let (w, h, rgb) = encode::webp_decode_rgb(data)?;
        let mut bgra = Vec::with_capacity(rgb.len() / 3 * 4);
        for p in rgb.chunks_exact(3) {
            bgra.extend_from_slice(&[p[2], p[1], p[0], 255]);
        }
        return Some(Image { w, h, bgra });
    }
    wic_decode(data, None)
}

/// WIC decode, optionally scaled down to fit `fit` (w, h) with a high-quality filter.
fn wic_decode(data: &[u8], fit: Option<(u32, u32)>) -> Option<Image> {
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::*;
    unsafe {
        let wic: IWICImagingFactory =
            CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER).ok()?;
        let stream = wic.CreateStream().ok()?;
        stream.InitializeFromMemory(data).ok()?;
        let dec = wic
            .CreateDecoderFromStream(&stream, std::ptr::null(), WICDecodeMetadataCacheOnDemand)
            .ok()?;
        let frame = dec.GetFrame(0).ok()?;
        let (mut w, mut h) = (0u32, 0u32);
        frame.GetSize(&mut w, &mut h).ok()?;
        if w == 0 || h == 0 {
            return None;
        }
        let source: IWICBitmapSource = match fit {
            Some((fw, fh)) if w > fw || h > fh => {
                let k = (fw as f32 / w as f32).min(fh as f32 / h as f32);
                let (tw, th) = (
                    ((w as f32 * k).round() as u32).max(1),
                    ((h as f32 * k).round() as u32).max(1),
                );
                let sc = wic.CreateBitmapScaler().ok()?;
                sc.Initialize(&frame, tw, th, WICBitmapInterpolationModeHighQualityCubic)
                    .ok()?;
                w = tw;
                h = th;
                windows::core::Interface::cast(&sc).ok()?
            }
            _ => windows::core::Interface::cast(&frame).ok()?,
        };
        let conv = wic.CreateFormatConverter().ok()?;
        conv.Initialize(
            &source,
            &GUID_WICPixelFormat32bppBGRA,
            WICBitmapDitherTypeNone,
            None,
            0.0,
            WICBitmapPaletteTypeCustom,
        )
        .ok()?;
        let mut buf = vec![0u8; (w * h * 4) as usize];
        conv.CopyPixels(std::ptr::null(), w * 4, &mut buf).ok()?;
        for a in buf.iter_mut().skip(3).step_by(4) {
            *a = 255;
        }
        Some(Image { w, h, bgra: buf })
    }
}

/// A preview for a card: the JPEG preview, or (if it is missing) the file itself.
fn load_preview(thumb: Option<&Path>, file: Option<&Path>, fit: (u32, u32)) -> Option<Image> {
    if let Some(img) = thumb
        .and_then(|t| std::fs::read(t).ok())
        .and_then(|d| wic_decode(&d, Some(fit)))
    {
        return Some(img);
    }
    let data = std::fs::read(file?).ok()?;
    if crate::share::kind(&data) == Some("image/webp") {
        let img = decode(&data)?;
        Some(crate::toast::downscale(&img, fit.0.max(fit.1)))
    } else {
        wic_decode(&data, Some(fit))
    }
}

struct Job {
    hwnd: isize,
    id: u64,
    thumb: Option<PathBuf>,
    file: Option<PathBuf>,
    fit: (u32, u32),
    gen: u32,
}

struct Decoded {
    id: u64,
    gen: u32,
    img: Option<Image>,
}

/// One background decoder. The newest request goes first (what is on screen now) and
/// requests for cards that scrolled away are skipped.
struct Loader {
    jobs: Mutex<Vec<Job>>,
    wake: Condvar,
    wanted: Mutex<HashSet<u64>>,
}

static LOADER: OnceLock<Arc<Loader>> = OnceLock::new();

fn loader() -> &'static Arc<Loader> {
    LOADER.get_or_init(|| {
        let l = Arc::new(Loader {
            jobs: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            wanted: Mutex::new(HashSet::new()),
        });
        let lw = l.clone();
        std::thread::spawn(move || {
            unsafe {
                let _ = windows::Win32::System::Com::CoInitializeEx(
                    None,
                    windows::Win32::System::Com::COINIT_MULTITHREADED,
                );
            }
            loop {
                let job = {
                    let mut q = lw.jobs.lock().unwrap();
                    while q.is_empty() {
                        q = lw.wake.wait(q).unwrap();
                    }
                    q.pop().unwrap()
                };
                if !lw.wanted.lock().unwrap().contains(&job.id) {
                    continue;
                }
                let img = load_preview(job.thumb.as_deref(), job.file.as_deref(), job.fit);
                let p = Box::into_raw(Box::new(Decoded {
                    id: job.id,
                    gen: job.gen,
                    img,
                }));
                unsafe {
                    if PostMessageW(
                        hwnd_from(job.hwnd),
                        WM_APP_HIST_THUMB,
                        WPARAM(0),
                        LPARAM(p as isize),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(p));
                    }
                }
            }
        });
        l
    })
}

// ---------------------------------------------------------------- window state

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Act {
    Open,
    Copy,
    Folder,
    Share,
    Unshare,
    Delete,
    OpenDir,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Hover {
    Card(u64),
    Button(Act),
    Bar,
}

struct Thumb {
    /// None = could not be decoded.
    bmp: Option<ID2D1Bitmap>,
    used: u64,
}

#[derive(Default)]
struct State {
    hwnd: Option<HWND>,
    k: f32,
    /// Scroll offset in device pixels, and where smooth scrolling is heading.
    scroll: f32,
    target: f32,
    sel: Option<u64>,
    hover: Option<Hover>,
    /// Scrollbar drag: (mouse y, scroll) at the start.
    drag: Option<(i32, f32)>,
    thumbs: HashMap<u64, Thumb>,
    pending: HashSet<u64>,
    frame: u64,
    gen: u32,
    /// Size the previews are decoded at (device pixels).
    fit: (u32, u32),
    status: String,
    status_bad: bool,
}

thread_local! {
    static S: RefCell<State> = RefCell::new(State::default());
}

fn st<R>(f: impl FnOnce(&mut State) -> R) -> R {
    S.with(|s| f(&mut s.borrow_mut()))
}

/// Cached previews kept beyond the visible ones (each ~250 KB at 150 % scaling).
const CACHE: usize = 96;

/// Grid geometry in device pixels.
#[derive(Clone, Copy, Debug)]
struct Grid {
    x0: f32,
    top: f32,
    bottom: f32,
    cols: usize,
    cw: f32,
    ch: f32,
    th: f32,
    gap: f32,
    content: f32,
}

impl Grid {
    fn new(rc: &RECT, k: f32, n: usize) -> Grid {
        let side = 28.0 * k;
        let gap = 16.0 * k;
        let width = (rc.right - rc.left) as f32 - side * 2.0;
        let cols = (((width + gap) / (210.0 * k + gap)).floor() as usize).max(1);
        let cw = (width - gap * (cols - 1) as f32) / cols as f32;
        let th = ((cw - 12.0 * k) * 0.6).round();
        let ch = 6.0 * k + th + 58.0 * k;
        let rows = n.div_ceil(cols);
        let top = 100.0 * k;
        let bottom = (rc.bottom as f32 - 68.0 * k).max(top + 10.0);
        Grid {
            x0: side,
            top,
            bottom,
            cols,
            cw,
            ch,
            th,
            gap,
            content: if rows == 0 {
                0.0
            } else {
                rows as f32 * (ch + gap) - gap + 16.0 * k
            },
        }
    }
    fn view(&self) -> f32 {
        self.bottom - self.top
    }
    fn max_scroll(&self) -> f32 {
        (self.content - self.view()).max(0.0)
    }
    /// Card rectangle (screen coordinates) of the i-th shown entry.
    fn card(&self, i: usize, scroll: f32) -> D2D_RECT_F {
        let (r, c) = (i / self.cols, i % self.cols);
        let x = self.x0 + c as f32 * (self.cw + self.gap);
        let y = self.top + 8.0 + r as f32 * (self.ch + self.gap) - scroll;
        D2D_RECT_F {
            left: x,
            top: y,
            right: x + self.cw,
            bottom: y + self.ch,
        }
    }
    /// Shown entries that intersect the viewport, with one row of margin.
    fn visible(&self, n: usize, scroll: f32) -> std::ops::Range<usize> {
        let row = self.ch + self.gap;
        let first = ((scroll - row) / row).floor().max(0.0) as usize;
        let last = ((scroll + self.view() + row) / row).ceil().max(0.0) as usize;
        (first * self.cols).min(n)..((last + 1) * self.cols).min(n)
    }
}

/// Entries newest first (what the grid shows).
fn shown() -> Vec<u64> {
    store(|s| s.entries.iter().rev().map(|e| e.id).collect())
}

fn client(h: HWND) -> RECT {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(h, &mut rc);
    }
    rc
}

pub fn refresh() {
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            let _ = InvalidateRect(h, None, FALSE);
        }
    }
}

/// A line in the History window footer (if the window is open).
pub fn note(msg: &str, bad: bool) {
    if st(|s| s.hwnd).is_some() {
        set_status(msg, bad);
    }
}

fn set_status(msg: &str, bad: bool) {
    st(|s| {
        s.status = msg.to_string();
        s.status_bad = bad;
    });
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            SetTimer(h, TIMER_STATUS, if bad { 6000 } else { 3000 }, None);
        }
    }
    refresh();
}

// ---------------------------------------------------------------- drawing

fn local_time(t: u64) -> Option<windows::Win32::Foundation::SYSTEMTIME> {
    use windows::Win32::System::Time::*;
    let ft = (t + 11_644_473_600) * 10_000_000;
    let f = FILETIME {
        dwLowDateTime: ft as u32,
        dwHighDateTime: (ft >> 32) as u32,
    };
    unsafe {
        let mut utc = SYSTEMTIME::default();
        FileTimeToSystemTime(&f, &mut utc).ok()?;
        let mut loc = SYSTEMTIME::default();
        SystemTimeToTzSpecificLocalTime(None, &utc, &mut loc).ok()?;
        Some(loc)
    }
}

fn when(t: u64) -> String {
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (Some(a), Some(n)) = (local_time(t), local_time(now())) else {
        return String::new();
    };
    let day = |s: &SYSTEMTIME| s.wYear as i32 * 400 + s.wMonth as i32 * 32 + s.wDay as i32;
    let clock = format!("{:02}:{:02}", a.wHour, a.wMinute);
    let yesterday = local_time(now().saturating_sub(86_400)).map(|y| day(&y));
    if day(&a) == day(&n) {
        format!("Today {clock}")
    } else if Some(day(&a)) == yesterday {
        format!("Yesterday {clock}")
    } else if a.wYear == n.wYear {
        format!(
            "{} {} {clock}",
            a.wDay,
            MONTHS[(a.wMonth as usize + 11) % 12]
        )
    } else {
        format!(
            "{} {} {}",
            a.wDay,
            MONTHS[(a.wMonth as usize + 11) % 12],
            a.wYear
        )
    }
}

fn human(b: u64) -> String {
    if b >= 1024 * 1024 {
        format!("{:.1} MB", b as f64 / 1048576.0)
    } else {
        format!("{} KB", b.div_ceil(1024))
    }
}

fn r_i(r: &D2D_RECT_F) -> RECT {
    rect(
        r.left.round() as i32,
        r.top.round() as i32,
        r.right.round() as i32,
        r.bottom.round() as i32,
    )
}

fn rr(r: D2D_RECT_F, rad: f32) -> D2D1_ROUNDED_RECT {
    D2D1_ROUNDED_RECT {
        rect: r,
        radiusX: rad,
        radiusY: rad,
    }
}

/// Footer buttons for the selected entry, right to left, with their widths.
fn footer_buttons(
    e: Option<&Entry>,
    k: f32,
    share_on: bool,
) -> Vec<(Act, &'static str, f32, bool)> {
    let now = now();
    let file = e.and_then(|e| e.file()).is_some();
    let link = e.is_some_and(|e| e.link_alive(now));
    let busy = e.is_some_and(|e| uploading(e.id));
    let mut v = vec![(Act::Delete, "Delete...", 92.0 * k, e.is_some())];
    if link {
        v.push((Act::Unshare, "Delete link...", 116.0 * k, !busy));
    }
    if share_on {
        let label = if link { "Copy link" } else { "Share link" };
        v.push((Act::Share, label, 108.0 * k, !busy && (file || link)));
    }
    v.push((Act::Folder, "Show in folder", 132.0 * k, file));
    v.push((Act::Copy, "Copy", 76.0 * k, file));
    v.push((Act::Open, "Open", 76.0 * k, file || link));
    v
}

/// Clickable rectangles outside the grid.
fn buttons(rc: &RECT, k: f32) -> Vec<(RECT, Act, &'static str, bool, bool)> {
    let sel = st(|s| s.sel).and_then(|id| store(|s| s.get(id).cloned()));
    let share_on = crate::update_base().is_some();
    let mut out = Vec::new();
    let px = |v: f32| (v * k) as i32;
    out.push((
        rect(
            rc.right - px(28.0) - px(128.0),
            px(30.0),
            rc.right - px(28.0),
            px(64.0),
        ),
        Act::OpenDir,
        "Open folder",
        false,
        true,
    ));
    let mut x = rc.right as f32 - 28.0 * k;
    let (t, b) = (rc.bottom - px(51.0), rc.bottom - px(17.0));
    let empty = store(|s| s.entries.is_empty());
    for (act, label, w, on) in footer_buttons(sel.as_ref(), k, share_on) {
        if empty {
            break;
        }
        let r = rect((x - w) as i32, t, x as i32, b);
        out.push((r, act, label, act == Act::Open, on));
        x -= w + 8.0 * k;
    }
    out
}

/// Scrollbar thumb (screen rectangle) if the content overflows.
fn bar(g: &Grid, rc: &RECT, k: f32, scroll: f32) -> Option<D2D_RECT_F> {
    let max = g.max_scroll();
    if max <= 0.0 {
        return None;
    }
    let view = g.view();
    let h = (view * view / g.content).max(36.0 * k);
    let y = g.top + (view - h) * (scroll / max);
    let x = rc.right as f32 - 12.0 * k;
    Some(D2D_RECT_F {
        left: x,
        top: y + 2.0,
        right: x + 6.0 * k,
        bottom: y + h - 2.0,
    })
}

unsafe fn draw(dc: HDC, rc: RECT) {
    let k = st(|s| s.k.max(1.0));
    let px = |v: f32| (v * k) as i32;
    if generation() != st(|s| s.gen) {
        // New render target: the cached bitmaps belong to the old one.
        st(|s| {
            s.gen = generation();
            s.thumbs.clear();
            s.pending.clear();
        });
    }
    fill(dc, rc, BG);

    let ids = shown();
    let now = now();
    let total: u64 = store(|s| {
        s.entries
            .iter()
            .filter(|e| !e.missing)
            .map(|e| e.size)
            .sum()
    });

    // Header.
    logo(dc, px(28.0), px(22.0), px(44.0));
    let mut t = rect(px(84.0), px(22.0), rc.right - px(180.0), px(50.0));
    text(
        dc,
        "History",
        &mut t,
        21.0 * k,
        600,
        INK,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    let sub = match ids.len() {
        0 => "Your screenshots will appear here".to_string(),
        1 => format!("1 screenshot  \u{00B7}  {}", human(total)),
        n => format!("{n} screenshots  \u{00B7}  {}", human(total)),
    };
    let mut t = rect(px(84.0), px(50.0), rc.right - px(180.0), px(72.0));
    text(
        dc,
        &sub,
        &mut t,
        13.0 * k,
        400,
        DIM,
        DT_SINGLELINE | DT_LEFT | DT_TOP,
    );
    fill(
        dc,
        rect(px(28.0), px(86.0), rc.right - px(28.0), px(86.0) + 1),
        EDGE,
    );

    let g = Grid::new(&rc, k, ids.len());
    let (scroll, sel, hover) = st(|s| {
        s.scroll = s.scroll.clamp(0.0, g.max_scroll());
        s.target = s.target.clamp(0.0, g.max_scroll());
        (s.scroll, s.sel, s.hover)
    });

    if ids.is_empty() {
        let mut t = rect(
            0,
            (g.top + g.view() * 0.38) as i32,
            rc.right,
            (g.top + g.view() * 0.38) as i32 + px(30.0),
        );
        text(
            dc,
            "No screenshots yet",
            &mut t,
            17.0 * k,
            600,
            INK,
            DT_SINGLELINE | DT_CENTER | DT_TOP,
        );
        let mut t = rect(0, t.top + px(32.0), rc.right, t.top + px(56.0));
        text(
            dc,
            "Press PrintScreen or Win+Shift+S. Saved and shared shots show up here.",
            &mut t,
            13.5 * k,
            400,
            DIM,
            DT_SINGLELINE | DT_CENTER | DT_TOP,
        );
    }

    // Grid, clipped to its area.
    let fit = ((288.0 * k).round() as u32, (180.0 * k).round() as u32);
    if st(|s| s.fit) != fit {
        st(|s| {
            s.fit = fit;
            s.thumbs.clear();
            s.pending.clear();
        });
    }
    let clip = D2D_RECT_F {
        left: 0.0,
        top: g.top,
        right: rc.right as f32,
        bottom: g.bottom,
    };
    with_rt(|rt| rt.PushAxisAlignedClip(&clip, D2D1_ANTIALIAS_MODE_ALIASED));
    let range = g.visible(ids.len(), scroll);
    let tick = st(|s| {
        s.frame += 1;
        s.frame
    });
    let hwnd = st(|s| s.hwnd).unwrap_or_default();
    let mut wanted = HashSet::new();
    let mut jobs = Vec::new();
    for i in range.clone() {
        let id = ids[i];
        wanted.insert(id);
        let Some(e) = store(|s| s.get(id).cloned()) else {
            continue;
        };
        let card = g.card(i, scroll);
        let is_sel = sel == Some(id);
        let is_hover = hover == Some(Hover::Card(id));
        let edge = if is_sel {
            MINT
        } else if is_hover {
            rgb(0x3A, 0x5A, 0x4B)
        } else {
            EDGE
        };
        frame_rect(
            dc,
            card,
            14.0 * k,
            if is_hover || is_sel {
                rgb(0x15, 0x1F, 0x1A)
            } else {
                PANEL
            },
            edge,
            if is_sel { 2.0 } else { 1.0 },
        );

        // Preview box.
        let tb = D2D_RECT_F {
            left: card.left + 6.0 * k,
            top: card.top + 6.0 * k,
            right: card.right - 6.0 * k,
            bottom: card.top + 6.0 * k + g.th,
        };
        if let Some(b) = brush_a(rgb(0x07, 0x0B, 0x09), 1.0) {
            with_rt(|rt| rt.FillRoundedRectangle(&rr(tb, 9.0 * k), &b));
        }
        let bmp = st(|s| {
            s.thumbs.get_mut(&id).map(|t| {
                t.used = tick;
                t.bmp.clone()
            })
        });
        match bmp {
            Some(Some(b)) => {
                let sz = b.GetSize();
                let bw = tb.right - tb.left;
                let bh = tb.bottom - tb.top;
                let s = (bw / sz.width).min(bh / sz.height);
                let (w, h) = (sz.width * s, sz.height * s);
                let dst = D2D_RECT_F {
                    left: (tb.left + (bw - w) * 0.5).round(),
                    top: (tb.top + (bh - h) * 0.5).round(),
                    right: (tb.left + (bw + w) * 0.5).round(),
                    bottom: (tb.top + (bh + h) * 0.5).round(),
                };
                image(
                    &b,
                    dst,
                    if (w - bw).abs() < 2.0 || (h - bh).abs() < 2.0 {
                        9.0 * k
                    } else {
                        3.0 * k
                    },
                );
            }
            Some(None) => {
                let mut t = r_i(&tb);
                text(
                    dc,
                    if e.file().is_some() {
                        "No preview"
                    } else {
                        "File deleted"
                    },
                    &mut t,
                    12.5 * k,
                    500,
                    FAINT,
                    DT_SINGLELINE | DT_CENTER | DT_VCENTER,
                );
            }
            None => {
                if st(|s| s.pending.insert(id)) {
                    jobs.push(Job {
                        hwnd: hwnd_to(hwnd),
                        id,
                        thumb: store(|s| s.thumb_path(&e)),
                        file: e.file(),
                        fit,
                        gen: generation(),
                    });
                }
            }
        }
        if e.missing {
            let mut t = r_i(&tb);
            t.top = t.bottom - px(26.0);
            if let Some(b) = brush_a(rgb(0x07, 0x0B, 0x09), 0.8) {
                with_rt(|rt| {
                    rt.FillRectangle(
                        &D2D_RECT_F {
                            top: t.top as f32,
                            ..tb
                        },
                        &b,
                    )
                });
            }
            text(
                dc,
                "File deleted",
                &mut t,
                12.0 * k,
                500,
                DIM,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER,
            );
        }
        // Upload progress along the bottom of the preview.
        if let Some(p) = UPLOADS.with(|u| u.borrow().get(&id).copied()) {
            let y = tb.bottom - 4.0 * k;
            let track = D2D_RECT_F {
                left: tb.left + 8.0 * k,
                top: y - 3.0 * k,
                right: tb.right - 8.0 * k,
                bottom: y,
            };
            if let (Some(bg), Some(fg)) = (brush_a(rgb(0x07, 0x0B, 0x09), 0.85), brush_a(MINT, 1.0))
            {
                with_rt(|rt| {
                    rt.FillRoundedRectangle(&rr(track, 1.5 * k), &bg);
                    let w = (track.right - track.left) * (p as f32 / 1000.0).max(0.04);
                    rt.FillRoundedRectangle(
                        &rr(
                            D2D_RECT_F {
                                right: track.left + w,
                                ..track
                            },
                            1.5 * k,
                        ),
                        &fg,
                    );
                });
            }
        }

        // Text under the preview.
        let ty = tb.bottom + 10.0 * k;
        let l = card.left + 12.0 * k;
        let r = card.right - 12.0 * k;
        let mut t = rect(l as i32, ty as i32, r as i32, (ty + 20.0 * k) as i32);
        text(
            dc,
            &when(e.time),
            &mut t,
            13.5 * k,
            500,
            INK,
            DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        );
        if e.size > 0 && !e.missing {
            text(
                dc,
                &human(e.size),
                &mut t,
                12.5 * k,
                400,
                DIM,
                DT_SINGLELINE | DT_RIGHT | DT_TOP,
            );
        }
        let ty2 = ty + 21.0 * k;
        let ext = Path::new(&e.path)
            .extension()
            .map(|x| x.to_string_lossy().to_uppercase())
            .unwrap_or_default();
        let dims = if ext.is_empty() {
            format!("{} \u{00D7} {}", e.w, e.h)
        } else {
            format!("{} \u{00D7} {}  \u{00B7}  {ext}", e.w, e.h)
        };
        let mut t = rect(l as i32, ty2 as i32, r as i32, (ty2 + 18.0 * k) as i32);
        text(
            dc,
            &dims,
            &mut t,
            12.0 * k,
            400,
            FAINT,
            DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        );
        if e.link_alive(now) {
            chip(dc, r, ty2 - 1.0 * k, k, "Link");
        } else if uploading(id) {
            chip(dc, r, ty2 - 1.0 * k, k, "Uploading");
        }
    }
    with_rt(|rt| rt.PopAxisAlignedClip());
    if let Some(b) = bar(&g, &rc, k, scroll) {
        let hot = matches!(hover, Some(Hover::Bar)) || st(|s| s.drag.is_some());
        if let Some(br) = brush_a(if hot { DIM } else { FAINT }, if hot { 0.9 } else { 0.55 }) {
            with_rt(|rt| rt.FillRoundedRectangle(&rr(b, 3.0 * k), &br));
        }
    }

    // Decode requests, newest last so they go first.
    if !jobs.is_empty() || !wanted.is_empty() {
        let l = loader();
        *l.wanted.lock().unwrap() = wanted.clone();
        if !jobs.is_empty() {
            let mut q = l.jobs.lock().unwrap();
            q.retain(|j| wanted.contains(&j.id));
            q.extend(jobs.into_iter().rev());
            l.wake.notify_one();
        }
    }
    // Requests that were dropped from the queue must be asked again later.
    st(|s| s.pending.retain(|id| wanted.contains(id)));
    // Keep the cache bounded: drop the least recently drawn previews.
    st(|s| {
        if s.thumbs.len() > CACHE {
            let mut v: Vec<(u64, u64)> = s.thumbs.iter().map(|(id, t)| (t.used, *id)).collect();
            v.sort();
            let n = s.thumbs.len() - CACHE;
            for (_, id) in v.into_iter().take(n) {
                if !wanted.contains(&id) {
                    s.thumbs.remove(&id);
                }
            }
        }
    });

    // Footer.
    fill(
        dc,
        rect(
            px(28.0),
            rc.bottom - px(68.0),
            rc.right - px(28.0),
            rc.bottom - px(68.0) + 1,
        ),
        EDGE,
    );
    let btns = buttons(&rc, k);
    let mut left_edge = rc.right;
    for (r, act, label, primary, on) in &btns {
        if *act != Act::OpenDir {
            left_edge = left_edge.min(r.left);
        }
        let hov = hover == Some(Hover::Button(*act));
        if *on {
            button(dc, *r, label, *primary, hov, k);
        } else {
            frame(dc, *r, px(20.0), BG, rgb(0x18, 0x22, 0x1E));
            let mut t = *r;
            text(
                dc,
                label,
                &mut t,
                13.5 * k,
                500,
                FAINT,
                DT_SINGLELINE | DT_CENTER | DT_VCENTER,
            );
        }
    }
    let (status, bad) = st(|s| (s.status.clone(), s.status_bad));
    let info_r = rect(
        px(28.0),
        rc.bottom - px(56.0),
        left_edge - px(16.0),
        rc.bottom - px(12.0),
    );
    if !status.is_empty() {
        let mut t = info_r;
        text(
            dc,
            &status,
            &mut t,
            13.5 * k,
            500,
            if bad { rgb(0xFF, 0x8A, 0x80) } else { MINT },
            DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
        );
    } else if let Some(e) = sel.and_then(|id| store(|s| s.get(id).cloned())) {
        let mut t = rect(info_r.left, info_r.top, info_r.right, info_r.top + px(22.0));
        let name = if e.path.is_empty() {
            e.url.clone()
        } else {
            e.path.clone()
        };
        text(
            dc,
            &name,
            &mut t,
            13.0 * k,
            500,
            INK,
            DT_SINGLELINE | DT_LEFT | DT_TOP | DT_PATH_ELLIPSIS,
        );
        let detail = if let Some(p) = UPLOADS.with(|u| u.borrow().get(&e.id).copied()) {
            format!("Uploading  {}%", p / 10)
        } else if e.link_alive(now) {
            let days = if e.expires == 0 {
                30
            } else {
                (e.expires - now).div_ceil(86_400)
            };
            format!(
                "{}  \u{00B7}  link works {} more day{}",
                e.url,
                days,
                if days == 1 { "" } else { "s" }
            )
        } else if e.missing {
            "The file was deleted or moved".to_string()
        } else {
            format!("{} \u{00D7} {}  \u{00B7}  {}", e.w, e.h, human(e.size))
        };
        let mut t = rect(
            info_r.left,
            info_r.top + px(22.0),
            info_r.right,
            info_r.bottom,
        );
        text(
            dc,
            &detail,
            &mut t,
            12.0 * k,
            400,
            DIM,
            DT_SINGLELINE | DT_LEFT | DT_TOP | DT_END_ELLIPSIS,
        );
    } else if !ids.is_empty() {
        let mut t = info_r;
        text(
            dc,
            "Select a screenshot. Double-click opens it, right-click shows all actions.",
            &mut t,
            12.5 * k,
            400,
            FAINT,
            DT_SINGLELINE | DT_LEFT | DT_VCENTER | DT_END_ELLIPSIS,
        );
    }
}

/// Rounded rectangle with a border of width `bw`.
unsafe fn frame_rect(_dc: HDC, r: D2D_RECT_F, rad: f32, fill_c: COLORREF, edge: COLORREF, bw: f32) {
    let inner = D2D_RECT_F {
        left: r.left + bw * 0.5,
        top: r.top + bw * 0.5,
        right: r.right - bw * 0.5,
        bottom: r.bottom - bw * 0.5,
    };
    if let (Some(f), Some(e)) = (brush_a(fill_c, 1.0), brush_a(edge, 1.0)) {
        with_rt(|rt| {
            rt.FillRoundedRectangle(&rr(inner, rad * 0.5), &f);
            rt.DrawRoundedRectangle(&rr(inner, rad * 0.5), &e, bw, None);
        });
    }
}

/// Small mint label, right-aligned at `right`.
unsafe fn chip(dc: HDC, right: f32, top: f32, k: f32, label: &str) {
    let w = (label.chars().count() as f32 * 7.0 + 16.0) * k;
    let r = D2D_RECT_F {
        left: right - w,
        top,
        right,
        bottom: top + 18.0 * k,
    };
    if let Some(b) = brush_a(MINT, 0.14) {
        with_rt(|rt| rt.FillRoundedRectangle(&rr(r, 6.0 * k), &b));
    }
    let mut t = r_i(&r);
    text(
        dc,
        label,
        &mut t,
        11.5 * k,
        600,
        MINT,
        DT_SINGLELINE | DT_CENTER | DT_VCENTER,
    );
}

// ---------------------------------------------------------------- input

fn hit(x: i32, y: i32) -> Option<Hover> {
    let h = st(|s| s.hwnd)?;
    let rc = client(h);
    let k = st(|s| s.k.max(1.0));
    for (r, act, _, _, on) in buttons(&rc, k) {
        if inside(&r, x, y) {
            return on.then_some(Hover::Button(act));
        }
    }
    let ids = shown();
    let g = Grid::new(&rc, k, ids.len());
    let scroll = st(|s| s.scroll);
    if let Some(b) = bar(&g, &rc, k, scroll) {
        if (x as f32) >= b.left - 6.0 * k && (y as f32) >= g.top && (y as f32) < g.bottom {
            return Some(Hover::Bar);
        }
    }
    let (fx, fy) = (x as f32, y as f32);
    if fy < g.top || fy >= g.bottom {
        return None;
    }
    for i in g.visible(ids.len(), scroll) {
        let c = g.card(i, scroll);
        if fx >= c.left && fx < c.right && fy >= c.top && fy < c.bottom {
            return Some(Hover::Card(ids[i]));
        }
    }
    None
}

fn selected() -> Option<Entry> {
    st(|s| s.sel).and_then(|id| store(|s| s.get(id).cloned()))
}

/// Scrolls so the selected card is fully visible.
fn reveal(id: u64) {
    let Some(h) = st(|s| s.hwnd) else { return };
    let rc = client(h);
    let k = st(|s| s.k.max(1.0));
    let ids = shown();
    let Some(i) = ids.iter().position(|x| *x == id) else {
        return;
    };
    let g = Grid::new(&rc, k, ids.len());
    let c = g.card(i, 0.0);
    st(|s| {
        let top = c.top - g.top - 8.0;
        let bottom = c.bottom - g.top + 8.0;
        if top < s.target {
            s.target = top.max(0.0);
        } else if bottom > s.target + g.view() {
            s.target = (bottom - g.view()).min(g.max_scroll());
        }
    });
    animate();
}

fn animate() {
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            SetTimer(h, TIMER_SCROLL, 8, None);
        }
    }
}

fn main_hwnd() -> HWND {
    crate::win::find_window(crate::win::MAIN_CLASS).unwrap_or_default()
}

fn act(a: Act) {
    let hwnd = st(|s| s.hwnd).unwrap_or_default();
    if a == Act::OpenDir {
        let dir = crate::settings_snapshot().dir;
        shell_open(&dir.display().to_string());
        return;
    }
    let Some(e) = selected() else { return };
    match a {
        Act::Open => match e.file() {
            Some(p) => shell_open(&p.display().to_string()),
            None if e.link_alive(now()) => shell_open(&e.url),
            None => {}
        },
        Act::Folder => {
            if let Some(p) = e.file() {
                shell_select(&p);
            }
        }
        Act::Copy => {
            let Some(p) = e.file() else { return };
            let (id, h) = (e.id, hwnd_to(hwnd));
            set_status("Copying...", false);
            std::thread::spawn(move || {
                unsafe {
                    let _ = windows::Win32::System::Com::CoInitializeEx(
                        None,
                        windows::Win32::System::Com::COINIT_MULTITHREADED,
                    );
                }
                let r = std::fs::read(&p).ok().and_then(|data| {
                    let img = decode(&data)?;
                    let png = if crate::share::kind(&data) == Some("image/png") {
                        Some(data)
                    } else {
                        encode::png(&img, 0).ok()
                    };
                    Some((img, png))
                });
                let b = Box::into_raw(Box::new((id, r)));
                unsafe {
                    if PostMessageW(
                        hwnd_from(h),
                        WM_APP_HIST_COPY,
                        WPARAM(0),
                        LPARAM(b as isize),
                    )
                    .is_err()
                    {
                        drop(Box::from_raw(b));
                    }
                }
            });
        }
        Act::Share => {
            let had_link = e.link_alive(now());
            if share(main_hwnd(), e.id) && !had_link {
                set_status("Uploading...", false);
            }
        }
        Act::Unshare => {
            let Some(base) = crate::update_base() else {
                return;
            };
            if !confirm(
                hwnd,
                "Delete the link?\n\nAnyone who opens it will see that the screenshot is gone. The file on this PC stays.",
            ) {
                return;
            }
            set_status("Deleting the link...", false);
            crate::share::start_delete(
                hwnd,
                WM_APP_HIST_UNSHARED,
                e.id,
                base,
                e.link_id().to_string(),
                e.delete_token.clone(),
            );
        }
        Act::Delete => {
            let link = e.link_alive(now());
            let msg = match (e.file().is_some(), link) {
                (true, true) => "Delete this screenshot and its link?\n\nThe file goes to the Recycle Bin and the link stops working.",
                (true, false) => "Move this screenshot to the Recycle Bin?",
                (false, true) => "Delete the link?\n\nAnyone who opens it will see that the screenshot is gone.",
                (false, false) => "Remove this entry from the history?",
            };
            if !confirm(hwnd, msg) {
                return;
            }
            if let Some(p) = e.file() {
                if !recycle(hwnd, &p) && p.exists() {
                    set_status("Windows did not let us delete the file.", true);
                    return;
                }
            }
            if link {
                if let Some(base) = crate::update_base() {
                    crate::share::start_delete(
                        hwnd,
                        WM_APP_HIST_UNSHARED,
                        0,
                        base,
                        e.link_id().to_string(),
                        e.delete_token.clone(),
                    );
                }
            }
            // Select the neighbour so the keyboard keeps working.
            let ids = shown();
            let i = ids.iter().position(|x| *x == e.id).unwrap_or(0);
            let next = ids
                .get(i + 1)
                .or_else(|| i.checked_sub(1).and_then(|j| ids.get(j)))
                .copied();
            store(|s| {
                s.remove(e.id);
                s.save();
            });
            st(|s| {
                s.thumbs.remove(&e.id);
                s.sel = next;
            });
            set_status("Deleted", false);
        }
        Act::OpenDir => {}
    }
    refresh();
}

fn confirm(owner: HWND, text: &str) -> bool {
    let t = W::new(text);
    let c = W::new("Refract-screener History");
    unsafe {
        MessageBoxW(
            owner,
            t.p(),
            c.p(),
            MB_YESNO | MB_ICONQUESTION | MB_DEFBUTTON2,
        ) == IDYES
    }
}

/// Moves a file to the Recycle Bin.
fn recycle(owner: HWND, p: &Path) -> bool {
    use windows::Win32::UI::Shell::*;
    let mut from: Vec<u16> = p.display().to_string().encode_utf16().collect();
    from.extend_from_slice(&[0, 0]);
    let mut op = SHFILEOPSTRUCTW {
        hwnd: owner,
        wFunc: FO_DELETE,
        pFrom: windows::core::PCWSTR(from.as_ptr()),
        fFlags: (FOF_ALLOWUNDO | FOF_NOCONFIRMATION | FOF_SILENT | FOF_NOERRORUI).0 as u16,
        ..Default::default()
    };
    let r = unsafe { SHFileOperationW(&mut op) };
    if r != 0 {
        log(&format!("recycle {} failed: {r}", p.display()));
    }
    r == 0 && !op.fAnyOperationsAborted.as_bool()
}

fn context_menu(x: i32, y: i32) {
    let Some(h) = st(|s| s.hwnd) else { return };
    let Some(e) = selected() else { return };
    let share_on = crate::update_base().is_some();
    let k = st(|s| s.k.max(1.0));
    unsafe {
        let Ok(m) = CreatePopupMenu() else { return };
        let ids: [(Act, &str); 6] = [
            (Act::Open, "Open\tEnter"),
            (Act::Copy, "Copy image\tCtrl+C"),
            (Act::Folder, "Show in folder"),
            (
                Act::Share,
                if e.link_alive(now()) {
                    "Copy link\tCtrl+Shift+C"
                } else {
                    "Share link\tCtrl+Shift+C"
                },
            ),
            (Act::Unshare, "Delete link..."),
            (Act::Delete, "Delete...\tDel"),
        ];
        let allowed: Vec<(Act, bool)> = footer_buttons(Some(&e), k, share_on)
            .into_iter()
            .map(|(a, _, _, on)| (a, on))
            .collect();
        for (i, (a, label)) in ids.iter().enumerate() {
            let Some((_, on)) = allowed.iter().find(|(x, _)| x == a) else {
                continue;
            };
            if *a == Act::Delete {
                let _ = AppendMenuW(m, MF_SEPARATOR, 0, windows::core::PCWSTR::null());
            }
            let t = W::new(label);
            let flags = if *on {
                MF_STRING
            } else {
                MF_STRING | MF_GRAYED
            };
            let _ = AppendMenuW(m, flags, i + 1, t.p());
        }
        let mut pt = POINT { x, y };
        let _ = ClientToScreen(h, &mut pt);
        let cmd = TrackPopupMenuEx(m, (TPM_RETURNCMD | TPM_RIGHTBUTTON).0, pt.x, pt.y, h, None);
        let _ = DestroyMenu(m);
        if cmd.0 > 0 {
            if let Some((a, _)) = ids.get(cmd.0 as usize - 1) {
                act(*a);
            }
        }
    }
}

fn key(vk: VIRTUAL_KEY) {
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let shift = unsafe { GetKeyState(VK_SHIFT.0 as i32) } < 0;
    let Some(h) = st(|s| s.hwnd) else { return };
    let ids = shown();
    if ids.is_empty() {
        if vk == VK_ESCAPE {
            unsafe {
                let _ = DestroyWindow(h);
            }
        }
        return;
    }
    let rc = client(h);
    let k = st(|s| s.k.max(1.0));
    let g = Grid::new(&rc, k, ids.len());
    let cur = st(|s| s.sel).and_then(|id| ids.iter().position(|x| *x == id));
    let rows_per_page = ((g.view() / (g.ch + g.gap)).floor() as usize).max(1);
    let mv = |d: isize| {
        let i = match cur {
            None => 0,
            Some(i) => (i as isize + d).clamp(0, ids.len() as isize - 1) as usize,
        };
        st(|s| s.sel = Some(ids[i]));
        reveal(ids[i]);
        refresh();
    };
    let cols = g.cols as isize;
    match vk {
        VK_ESCAPE => unsafe {
            let _ = DestroyWindow(h);
        },
        VK_LEFT => mv(-1),
        VK_RIGHT => mv(1),
        VK_UP => mv(-cols),
        VK_DOWN => mv(cols),
        VK_PRIOR => mv(-cols * rows_per_page as isize),
        VK_NEXT => mv(cols * rows_per_page as isize),
        VK_HOME => mv(-(ids.len() as isize)),
        VK_END => mv(ids.len() as isize),
        VK_RETURN => act(Act::Open),
        VK_DELETE => act(Act::Delete),
        VK_C if ctrl && shift => act(Act::Share),
        VK_C if ctrl => act(Act::Copy),
        _ => {}
    }
}

// ---------------------------------------------------------------- window

/// Lazy check: which entries lost their file. Runs off the UI thread.
fn check_files(hwnd: HWND) {
    let list: Vec<(u64, String)> = store(|s| {
        s.entries
            .iter()
            .filter(|e| !e.path.is_empty())
            .map(|e| (e.id, e.path.clone()))
            .collect()
    });
    let orphans = store(|s| s.orphans(std::time::Duration::from_secs(600)));
    let h = hwnd_to(hwnd);
    std::thread::spawn(move || {
        for p in orphans {
            let _ = std::fs::remove_file(p);
        }
        let gone: Vec<u64> = list
            .into_iter()
            .filter(|(_, p)| !Path::new(p).exists())
            .map(|(id, _)| id)
            .collect();
        let b = Box::into_raw(Box::new(gone));
        unsafe {
            if PostMessageW(
                hwnd_from(h),
                WM_APP_HIST_CLEAN,
                WPARAM(0),
                LPARAM(b as isize),
            )
            .is_err()
            {
                drop(Box::from_raw(b));
            }
        }
    });
}

pub fn show() {
    if let Some(h) = st(|s| s.hwnd) {
        unsafe {
            let _ = ShowWindow(h, SW_RESTORE);
            let _ = SetForegroundWindow(h);
        }
        return;
    }
    unsafe {
        static ONCE: std::sync::Once = std::sync::Once::new();
        let inst = GetModuleHandleW(None).unwrap_or_default();
        ONCE.call_once(|| {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_DBLCLKS,
                lpfnWndProc: Some(proc_),
                hInstance: inst.into(),
                hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                hIcon: crate::icon::make(32),
                hIconSm: crate::icon::make(16),
                lpszClassName: w!("RefractScreenerHistory"),
                ..Default::default()
            };
            RegisterClassExW(&wc);
        });
        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTOPRIMARY);
        let (mut dx, mut dy) = (96u32, 96u32);
        let _ = GetDpiForMonitor(mon, MDT_EFFECTIVE_DPI, &mut dx, &mut dy);
        let k = dx as f32 / 96.0;
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        let style = WS_OVERLAPPEDWINDOW;
        let (cw, ch) = (
            ((1000.0 * k) as i32).min(wa.right - wa.left - 40),
            ((700.0 * k) as i32).min(wa.bottom - wa.top - 40),
        );
        let mut r = rect(0, 0, cw, ch);
        let _ = AdjustWindowRectExForDpi(&mut r, style, FALSE, WINDOW_EX_STYLE(0), dx);
        let (w, h) = (r.right - r.left, r.bottom - r.top);
        let x = wa.left + ((wa.right - wa.left - w) / 2).max(0);
        let y = wa.top + ((wa.bottom - wa.top - h) / 2).max(0);
        st(|s| {
            s.k = k;
            s.scroll = 0.0;
            s.target = 0.0;
            s.status.clear();
            s.sel = None;
        });
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("RefractScreenerHistory"),
            w!("Refract-screener History"),
            style,
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
        dark_frame(hwnd);
        st(|s| s.hwnd = Some(hwnd));
        check_files(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
    }
}

unsafe extern "system" fn proc_(h: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    match msg {
        WM_PAINT => {
            paint(h, |dc, rc| draw(dc, rc));
            LRESULT(0)
        }
        WM_ERASEBKGND => LRESULT(1),
        WM_SIZE => {
            refresh();
            LRESULT(0)
        }
        WM_GETMINMAXINFO => {
            let k = st(|s| s.k.max(1.0));
            let mm = &mut *(lp.0 as *mut MINMAXINFO);
            mm.ptMinTrackSize = POINT {
                x: (860.0 * k) as i32,
                y: (480.0 * k) as i32,
            };
            LRESULT(0)
        }
        WM_MOUSEMOVE => {
            let (x, y) = crate::win::lparam_xy(lp);
            if let Some((y0, s0)) = st(|s| s.drag) {
                let rc = client(h);
                let k = st(|s| s.k.max(1.0));
                let g = Grid::new(&rc, k, shown().len());
                let view = g.view();
                let bh = (view * view / g.content.max(1.0)).max(36.0 * k);
                let per = g.max_scroll() / (view - bh).max(1.0);
                st(|s| {
                    s.scroll = (s0 + (y - y0) as f32 * per).clamp(0.0, g.max_scroll());
                    s.target = s.scroll;
                });
                refresh();
                return LRESULT(0);
            }
            let now = hit(x, y);
            if st(|s| std::mem::replace(&mut s.hover, now)) != now {
                refresh();
            }
            let mut tme = TRACKMOUSEEVENT {
                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                dwFlags: TME_LEAVE,
                hwndTrack: h,
                dwHoverTime: 0,
            };
            let _ = TrackMouseEvent(&mut tme);
            LRESULT(0)
        }
        0x02A3 /* WM_MOUSELEAVE */ => {
            st(|s| s.hover = None);
            refresh();
            LRESULT(0)
        }
        WM_SETCURSOR if (lp.0 & 0xFFFF) as u32 == HTCLIENT => {
            set_cursor(matches!(st(|s| s.hover), Some(Hover::Button(_))));
            LRESULT(1)
        }
        WM_LBUTTONDOWN => {
            let (x, y) = crate::win::lparam_xy(lp);
            let _ = SetFocus(h);
            match hit(x, y) {
                Some(Hover::Bar) => {
                    let s0 = st(|s| s.scroll);
                    st(|s| s.drag = Some((y, s0)));
                    SetCapture(h);
                }
                Some(Hover::Card(id)) => {
                    st(|s| s.sel = Some(id));
                    reveal(id);
                    refresh();
                }
                Some(Hover::Button(_)) => {}
                None => {
                    if (y as f32) < Grid::new(&client(h), st(|s| s.k.max(1.0)), 0).bottom {
                        st(|s| s.sel = None);
                        refresh();
                    }
                }
            }
            LRESULT(0)
        }
        WM_LBUTTONUP => {
            if st(|s| s.drag.take()).is_some() {
                let _ = ReleaseCapture();
                refresh();
                return LRESULT(0);
            }
            let (x, y) = crate::win::lparam_xy(lp);
            if let Some(Hover::Button(a)) = hit(x, y) {
                act(a);
            }
            LRESULT(0)
        }
        WM_LBUTTONDBLCLK => {
            let (x, y) = crate::win::lparam_xy(lp);
            if let Some(Hover::Card(id)) = hit(x, y) {
                st(|s| s.sel = Some(id));
                act(Act::Open);
            }
            LRESULT(0)
        }
        WM_RBUTTONUP => {
            let (x, y) = crate::win::lparam_xy(lp);
            if let Some(Hover::Card(id)) = hit(x, y) {
                st(|s| s.sel = Some(id));
                refresh();
                context_menu(x, y);
            }
            LRESULT(0)
        }
        WM_MOUSEWHEEL => {
            let d = ((wp.0 >> 16) & 0xFFFF) as u16 as i16 as f32 / 120.0;
            let k = st(|s| s.k.max(1.0));
            st(|s| s.target -= d * 110.0 * k);
            animate();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_SCROLL => {
            let rc = client(h);
            let k = st(|s| s.k.max(1.0));
            let max = Grid::new(&rc, k, shown().len()).max_scroll();
            let done = st(|s| {
                s.target = s.target.clamp(0.0, max);
                let d = s.target - s.scroll;
                if d.abs() < 0.6 {
                    s.scroll = s.target;
                    true
                } else {
                    s.scroll += d * 0.32;
                    false
                }
            });
            if done {
                let _ = KillTimer(h, TIMER_SCROLL);
            }
            refresh();
            LRESULT(0)
        }
        WM_TIMER if wp.0 == TIMER_STATUS => {
            let _ = KillTimer(h, TIMER_STATUS);
            st(|s| s.status.clear());
            refresh();
            LRESULT(0)
        }
        WM_KEYDOWN => {
            key(VIRTUAL_KEY(wp.0 as u16));
            LRESULT(0)
        }
        WM_ACTIVATE => {
            refresh();
            LRESULT(0)
        }
        WM_APP_HIST_THUMB => {
            let d = *Box::from_raw(lp.0 as *mut Decoded);
            st(|s| s.pending.remove(&d.id));
            if d.gen == generation() {
                let bmp = d.img.and_then(|i| bitmap(&i.bgra, i.w, i.h));
                st(|s| {
                    let used = s.frame;
                    s.thumbs.insert(d.id, Thumb { bmp, used });
                });
            }
            refresh();
            LRESULT(0)
        }
        WM_APP_HIST_COPY => {
            let (_, r) = *Box::from_raw(lp.0 as *mut (u64, Option<(Image, Option<Vec<u8>>)>));
            match r {
                Some((img, png)) if encode::copy_image(h, &img, png.as_deref()) => {
                    set_status("Image copied to the clipboard", false)
                }
                _ => set_status("Could not copy the image.", true),
            }
            LRESULT(0)
        }
        WM_APP_HIST_UNSHARED => {
            let (id, r) = *Box::from_raw(lp.0 as *mut (u64, Result<(), String>));
            match r {
                Ok(()) => {
                    if id != 0 {
                        store(|s| {
                            if let Some(e) = s.get_mut(id) {
                                e.url.clear();
                                e.delete_token.clear();
                                e.expires = 0;
                            }
                            s.prune(&[], now());
                            s.save();
                        });
                        set_status("Link deleted", false);
                    }
                }
                Err(e) => set_status(&e, true),
            }
            LRESULT(0)
        }
        WM_APP_HIST_CLEAN => {
            let gone = *Box::from_raw(lp.0 as *mut Vec<u64>);
            store(|s| {
                if s.prune(&gone, now()) {
                    s.save();
                }
            });
            refresh();
            LRESULT(0)
        }
        WM_DPICHANGED => {
            st(|s| s.k = (wp.0 & 0xFFFF) as f32 / 96.0);
            let r = &*(lp.0 as *const RECT);
            let _ = SetWindowPos(h, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
            refresh();
            LRESULT(0)
        }
        WM_DESTROY => {
            st(|s| {
                *s = State::default();
            });
            if let Some(l) = LOADER.get() {
                l.wanted.lock().unwrap().clear();
                l.jobs.lock().unwrap().clear();
            }
            LRESULT(0)
        }
        _ => DefWindowProcW(h, msg, wp, lp),
    }
}

// ---------------------------------------------------------------- self test

/// Index round trip, the 500-entry cap, preview ownership and the lazy cleanup.
pub fn selftest(dir: &Path, out: &mut dyn FnMut(String)) -> usize {
    let mut fail = 0;
    let root = dir.join("history-test");
    let _ = std::fs::remove_dir_all(&root);
    let thumbs = root.join("thumbs");
    let _ = std::fs::create_dir_all(&thumbs);
    let index = root.join("history.json");
    let mut s = Store::load(index.clone(), thumbs.clone());
    let shot = root.join("shot.png");
    let _ = std::fs::write(&shot, b"png");
    let t = std::time::Instant::now();
    for i in 0..(MAX + 10) {
        let thumb = format!("t{i}.jpg");
        let _ = std::fs::write(thumbs.join(&thumb), b"jpg");
        let id = s.new_id();
        s.add(Entry {
            id,
            path: if i == MAX + 9 {
                root.join("gone.png").display().to_string()
            } else {
                shot.display().to_string()
            },
            time: now(),
            size: 3,
            w: 10,
            h: 10,
            thumb,
            url: if i == MAX + 8 {
                "https://x.test/s/abc".into()
            } else {
                String::new()
            },
            delete_token: String::new(),
            expires: if i == MAX + 8 { now() + 100 } else { 0 },
            missing: false,
        });
    }
    s.save();
    let add_ms = t.elapsed().as_millis();
    let ok_cap = s.entries.len() == MAX;
    let ok_thumbs = !thumbs.join("t0.jpg").exists()
        && !thumbs.join("t9.jpg").exists()
        && thumbs.join("t10.jpg").exists();
    let t = std::time::Instant::now();
    let mut s2 = Store::load(index.clone(), thumbs.clone());
    let load_ms = t.elapsed().as_millis();
    let ok_load = s2.entries == s.entries;
    let ids: Vec<u64> = s2.entries.iter().map(|e| e.id).collect();
    let ok_ids = ids.windows(2).all(|w| w[0] < w[1]);
    // Lazy cleanup: the entry with the deleted file goes, the shared one with a deleted file stays.
    let last = *ids.last().unwrap_or(&0);
    let shared = ids[ids.len() - 2];
    let gone = [last, shared];
    s2.prune(&gone, now());
    let ok_prune = s2.get(last).is_none()
        && s2.get(shared).is_some_and(|e| e.missing)
        && !thumbs.join(format!("t{}.jpg", MAX + 9)).exists();
    let _ = std::fs::write(thumbs.join("orphan.jpg"), b"x");
    let ok_orphan = s2
        .orphans(std::time::Duration::ZERO)
        .iter()
        .any(|p| p.ends_with("orphan.jpg"))
        && !s2
            .orphans(std::time::Duration::ZERO)
            .iter()
            .any(|p| p.ends_with("t20.jpg"));
    let size = std::fs::metadata(&index).map(|m| m.len()).unwrap_or(0);
    for (name, ok) in [
        ("capped at 500", ok_cap),
        ("evicted previews deleted", ok_thumbs),
        ("load == save", ok_load),
        ("ids increase", ok_ids),
        ("lazy cleanup keeps live links", ok_prune),
        ("orphan previews found", ok_orphan),
    ] {
        if !ok {
            fail += 1;
        }
        out(format!("history {name}: {ok}"));
    }
    out(format!(
        "history index of {MAX}: {} KB, {add_ms} ms to fill, {load_ms} ms to load",
        size / 1024
    ));
    let _ = std::fs::remove_dir_all(&root);
    fail
}
