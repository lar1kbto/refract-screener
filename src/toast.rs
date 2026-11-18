//! Windows notifications in the style of Snipping Tool (a toast with a preview
//! of the shot that opens it on click) and the "saved" sound.

use crate::encode::{self, Image};
use std::path::{Path, PathBuf};
use windows::core::HSTRING;
use windows::Data::Xml::Dom::XmlDocument;
use windows::Win32::Media::Audio::*;
use windows::UI::Notifications::{ToastNotification, ToastNotificationManager};

pub const AUMID: &str = "Refract.Screener";

static SAVE_SOUND: &[u8] = include_bytes!("../assets/save.wav");

/// Plays the confirmation sound (asynchronously, from memory).
pub fn play_save_sound() {
    unsafe {
        let _ = PlaySoundW(
            windows::core::PCWSTR(SAVE_SOUND.as_ptr() as *const u16),
            None,
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

fn data_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("Refract-screener")
}

/// Gives the app its name and icon in notifications (no package needed).
pub fn register() {
    let dir = data_dir();
    let _ = std::fs::create_dir_all(&dir);
    let icon = dir.join("icon.png");
    let _ = std::fs::write(&icon, crate::icon::MARK_PNG);
    use windows::Win32::System::Registry::*;
    let path = crate::win::W::new(&format!(r"Software\Classes\AppUserModelId\{AUMID}"));
    unsafe {
        let mut k = HKEY::default();
        if RegCreateKeyExW(
            HKEY_CURRENT_USER,
            path.p(),
            0,
            windows::core::PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_WRITE,
            None,
            &mut k,
            None,
        ) == windows::Win32::Foundation::ERROR_SUCCESS
        {
            let set = |name: &str, val: &str| {
                let n = crate::win::W::new(name);
                let v = crate::win::wide(val);
                let b = std::slice::from_raw_parts(v.as_ptr() as *const u8, v.len() * 2);
                let _ = RegSetValueExW(k, n.p(), 0, REG_SZ, Some(b));
            };
            set("DisplayName", "Refract-screener");
            set("IconUri", &icon.display().to_string());
            set("IconBackgroundColor", "FF16181E");
            let _ = RegCloseKey(k);
        }
        let id = crate::win::W::new(AUMID);
        let _ = windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID(id.p());
    }
}

fn xml(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn file_uri(p: &Path) -> String {
    let s = p.display().to_string().replace('\\', "/");
    let mut out = String::from("file:///");
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' | b'/' | b':' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Folder of shot previews. One JPEG per shot, shared by the notification and the
/// History window; `history` deletes the ones no entry refers to.
pub fn thumbs_dir() -> PathBuf {
    data_dir().join("thumbs")
}

/// Longest side of a preview: sharp in a notification at 200 % scaling, ~60 KB as JPEG.
pub const THUMB_MAX: u32 = 640;

/// Box-filter downscale (each target pixel averages the source pixels under it), so
/// thin text and UI lines blend instead of flickering in and out.
pub fn downscale(img: &Image, max: u32) -> Image {
    let (w, h) = (img.w as usize, img.h as usize);
    let k = (max as f32 / w as f32).min(max as f32 / h as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * k).round() as usize).max(1),
        ((h as f32 * k).round() as usize).max(1),
    );
    let mut out = vec![0u8; tw * th * 4];
    let mut acc = vec![0u32; tw * 3];
    let xs: Vec<(usize, usize)> = (0..tw)
        .map(|tx| {
            let x0 = tx * w / tw;
            (x0, ((tx + 1) * w / tw).max(x0 + 1).min(w))
        })
        .collect();
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        acc.iter_mut().for_each(|a| *a = 0);
        for y in y0..y1 {
            let row = &img.bgra[y * w * 4..(y + 1) * w * 4];
            for (tx, &(x0, x1)) in xs.iter().enumerate() {
                let a = &mut acc[tx * 3..tx * 3 + 3];
                for px in row[x0 * 4..x1 * 4].chunks_exact(4) {
                    a[0] += px[0] as u32;
                    a[1] += px[1] as u32;
                    a[2] += px[2] as u32;
                }
            }
        }
        for (tx, &(x0, x1)) in xs.iter().enumerate() {
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (ty * tw + tx) * 4;
            out[o] = (acc[tx * 3] / n) as u8;
            out[o + 1] = (acc[tx * 3 + 1] / n) as u8;
            out[o + 2] = (acc[tx * 3 + 2] / n) as u8;
            out[o + 3] = 255;
        }
    }
    Image {
        w: tw as u32,
        h: th as u32,
        bgra: out,
    }
}

/// Writes `img` as a JPEG with Windows' own encoder (WIC).
pub fn write_jpeg(img: &Image, path: &Path, quality: f32) -> bool {
    use windows::core::{PWSTR, VARIANT};
    use windows::Win32::Foundation::GENERIC_WRITE;
    use windows::Win32::Graphics::Imaging::*;
    use windows::Win32::System::Com::StructuredStorage::*;
    use windows::Win32::System::Com::*;
    let r = (|| -> windows::core::Result<()> {
        unsafe {
            let wic: IWICImagingFactory =
                CoCreateInstance(&CLSID_WICImagingFactory, None, CLSCTX_INPROC_SERVER)?;
            let stream = wic.CreateStream()?;
            let p = crate::win::W::new(&path.display().to_string());
            stream.InitializeFromFilename(p.p(), GENERIC_WRITE.0)?;
            let enc = wic.CreateEncoder(&GUID_ContainerFormatJpeg, std::ptr::null())?;
            enc.Initialize(&stream, WICBitmapEncoderNoCache)?;
            let mut frame: Option<IWICBitmapFrameEncode> = None;
            let mut bag: Option<IPropertyBag2> = None;
            enc.CreateNewFrame(&mut frame, &mut bag)?;
            let frame = frame.ok_or_else(windows::core::Error::empty)?;
            if let Some(bag) = &bag {
                let mut name = crate::win::wide("ImageQuality");
                let prop = PROPBAG2 {
                    pstrName: PWSTR(name.as_mut_ptr()),
                    ..Default::default()
                };
                let _ = bag.Write(1, &prop, &VARIANT::from(quality));
            }
            frame.Initialize(bag.as_ref())?;
            frame.SetSize(img.w, img.h)?;
            // The JPEG encoder takes 24-bit BGR (it would silently switch a 32-bit format).
            let mut fmt = GUID_WICPixelFormat24bppBGR;
            frame.SetPixelFormat(&mut fmt)?;
            if fmt != GUID_WICPixelFormat24bppBGR {
                return Err(windows::core::Error::empty());
            }
            let bgr: Vec<u8> = img
                .bgra
                .chunks_exact(4)
                .flat_map(|p| [p[0], p[1], p[2]])
                .collect();
            frame.WritePixels(img.h, img.w * 3, &bgr)?;
            frame.Commit()?;
            enc.Commit()?;
            stream.Commit(STGC_DEFAULT)
        }
    })();
    if r.is_err() {
        let _ = std::fs::remove_file(path);
    }
    r.is_ok()
}

/// Preview of a shot for the notification and the History window (Windows limits
/// toast images to ~1024 px / 3 MB and takes PNG or JPEG).
pub fn thumbnail(img: &Image) -> Option<PathBuf> {
    let dir = thumbs_dir();
    let _ = std::fs::create_dir_all(&dir);
    let thumb = downscale(img, THUMB_MAX);
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = (0..100)
        .map(|i| dir.join(format!("{}.jpg", t + i)))
        .find(|p| !p.exists())?;
    if write_jpeg(&thumb, &path, 0.9) {
        return Some(path);
    }
    // No JPEG encoder: fall back to PNG.
    let path = path.with_extension("png");
    std::fs::write(&path, encode::png(&thumb, 0).ok()?).ok()?;
    Some(path)
}

pub struct Toast<'a> {
    pub title: &'a str,
    pub body: &'a str,
    pub image: Option<&'a Path>,
    /// Opened when the notification is clicked.
    pub open: Option<&'a Path>,
    /// "Open folder" button.
    pub folder: Option<&'a Path>,
    /// refract-screener: link opened on click (instead of `open`).
    pub launch_uri: Option<&'a str>,
    /// Extra button: (label, refract-screener: link).
    pub button: Option<(&'a str, &'a str)>,
}

/// Shows a Windows notification. Returns false if notifications are unavailable.
pub fn show(t: &Toast) -> bool {
    show_tagged(t, None)
}

const GROUP: &str = "refract";

/// Like `show`; a notification with the same tag replaces the earlier one
/// (an upload progress bar turns into "Link copied").
pub fn show_tagged(t: &Toast, tag: Option<&str>) -> bool {
    let launch = match (t.launch_uri, t.open) {
        (Some(u), _) => format!(" activationType=\"protocol\" launch=\"{}\"", xml(u)),
        (None, Some(p)) => format!(
            " activationType=\"protocol\" launch=\"{}\"",
            xml(&file_uri(p))
        ),
        _ => String::new(),
    };
    let image = t
        .image
        .map(|p| format!("<image src=\"{}\" />", xml(&file_uri(p))))
        .unwrap_or_default();
    let mut buttons = String::new();
    if let Some((label, uri)) = t.button {
        buttons.push_str(&format!(
            "<action content=\"{}\" activationType=\"protocol\" arguments=\"{}\" />",
            xml(label),
            xml(uri)
        ));
    }
    if let Some(p) = t.folder {
        buttons.push_str(&format!(
            "<action content=\"Open folder\" activationType=\"protocol\" arguments=\"{}\" />",
            xml(&file_uri(p))
        ));
    }
    let actions = if buttons.is_empty() {
        String::new()
    } else {
        format!("<actions>{buttons}</actions>")
    };
    let body = if t.body.is_empty() {
        String::new()
    } else {
        format!("<text>{}</text>", xml(t.body))
    };
    let doc = format!(
        "<toast{launch}><visual><binding template=\"ToastGeneric\"><text>{}</text>{body}{image}</binding></visual>{actions}<audio silent=\"true\" /></toast>",
        xml(t.title)
    );
    let r = (|| -> windows::core::Result<()> {
        let x = XmlDocument::new()?;
        x.LoadXml(&HSTRING::from(doc))?;
        let toast = ToastNotification::CreateToastNotification(&x)?;
        if let Some(tag) = tag {
            toast.SetTag(&HSTRING::from(tag))?;
            toast.SetGroup(&HSTRING::from(GROUP))?;
        }
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&toast)
    })();
    if let Err(e) = &r {
        crate::win::log(&format!("toast failed: {e}"));
    }
    r.is_ok()
}

/// A notification with a live progress bar (uploads that take more than a moment).
pub struct Progress {
    notifier: windows::UI::Notifications::ToastNotifier,
    tag: HSTRING,
    seq: u32,
}

impl Progress {
    pub fn show(tag: &str, title: &str) -> Option<Progress> {
        use windows::UI::Notifications::NotificationData;
        let doc = format!(
            "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><progress value=\"{{v}}\" valueStringOverride=\"{{s}}\" status=\"{{t}}\" /></binding></visual><audio silent=\"true\" /></toast>",
            xml(title)
        );
        let r = (|| -> windows::core::Result<Progress> {
            let x = XmlDocument::new()?;
            x.LoadXml(&HSTRING::from(doc))?;
            let toast = ToastNotification::CreateToastNotification(&x)?;
            let tag = HSTRING::from(tag);
            toast.SetTag(&tag)?;
            toast.SetGroup(&HSTRING::from(GROUP))?;
            let data = NotificationData::new()?;
            let v = data.Values()?;
            v.Insert(&HSTRING::from("v"), &HSTRING::from("0"))?;
            v.Insert(&HSTRING::from("s"), &HSTRING::from(""))?;
            v.Insert(&HSTRING::from("t"), &HSTRING::from(""))?;
            data.SetSequenceNumber(1)?;
            toast.SetData(&data)?;
            let notifier =
                ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
            notifier.Show(&toast)?;
            Ok(Progress {
                notifier,
                tag,
                seq: 1,
            })
        })();
        r.ok()
    }

    /// `value` 0..1; `right` is shown under the bar on the right, `left` on the left.
    pub fn set(&mut self, value: f32, left: &str, right: &str) {
        use windows::UI::Notifications::NotificationData;
        self.seq += 1;
        let _ = (|| -> windows::core::Result<()> {
            let data = NotificationData::new()?;
            let v = data.Values()?;
            v.Insert(
                &HSTRING::from("v"),
                &HSTRING::from(format!("{:.3}", value.clamp(0.0, 1.0))),
            )?;
            v.Insert(&HSTRING::from("s"), &HSTRING::from(right))?;
            v.Insert(&HSTRING::from("t"), &HSTRING::from(left))?;
            data.SetSequenceNumber(self.seq)?;
            self.notifier
                .UpdateWithTagAndGroup(&data, &self.tag, &HSTRING::from(GROUP))?;
            Ok(())
        })();
    }
}

/// Removes a notification shown with `show_tagged` / `Progress`.
pub fn remove(tag: &str) {
    let _ = (|| -> windows::core::Result<()> {
        ToastNotificationManager::History()?.RemoveGroupedTagWithId(
            &HSTRING::from(tag),
            &HSTRING::from(GROUP),
            &HSTRING::from(AUMID),
        )
    })();
}
