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

/// Small preview for the notification (Windows limits toast images to ~1024 px / 3 MB).
pub fn thumbnail(img: &Image) -> Option<PathBuf> {
    let dir = data_dir().join("thumbs");
    let _ = std::fs::create_dir_all(&dir);
    // Keep only the latest previews.
    if let Ok(rd) = std::fs::read_dir(&dir) {
        let mut files: Vec<_> = rd
            .flatten()
            .filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path())))
            .collect();
        files.sort();
        let n = files.len();
        for (_, p) in files.into_iter().take(n.saturating_sub(15)) {
            let _ = std::fs::remove_file(p);
        }
    }
    let (w, h) = (img.w as usize, img.h as usize);
    let k = (720.0 / w as f32).min(720.0 / h as f32).min(1.0);
    let (tw, th) = (
        ((w as f32 * k).round() as usize).max(1),
        ((h as f32 * k).round() as usize).max(1),
    );
    // Box filter: average all source pixels that fall into each target pixel.
    let mut out = vec![0u8; tw * th * 4];
    for ty in 0..th {
        let y0 = ty * h / th;
        let y1 = ((ty + 1) * h / th).max(y0 + 1).min(h);
        for tx in 0..tw {
            let x0 = tx * w / tw;
            let x1 = ((tx + 1) * w / tw).max(x0 + 1).min(w);
            let mut s = [0u32; 3];
            for y in y0..y1 {
                let row = y * w * 4;
                for x in x0..x1 {
                    let i = row + x * 4;
                    s[0] += img.bgra[i] as u32;
                    s[1] += img.bgra[i + 1] as u32;
                    s[2] += img.bgra[i + 2] as u32;
                }
            }
            let n = ((y1 - y0) * (x1 - x0)) as u32;
            let o = (ty * tw + tx) * 4;
            out[o] = (s[0] / n) as u8;
            out[o + 1] = (s[1] / n) as u8;
            out[o + 2] = (s[2] / n) as u8;
            out[o + 3] = 255;
        }
    }
    let thumb = Image {
        w: tw as u32,
        h: th as u32,
        bgra: out,
    };
    let data = encode::png(&thumb, 0).ok()?;
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = dir.join(format!("{t}.png"));
    std::fs::write(&path, data).ok()?;
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
        let notifier = ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(AUMID))?;
        notifier.Show(&toast)
    })();
    if let Err(e) = &r {
        crate::win::log(&format!("toast failed: {e}"));
    }
    r.is_ok()
}
