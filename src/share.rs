//! "Share link": a screenshot goes to the Refract cloud (`POST {update_url}/api/s`),
//! the short link goes to the clipboard and a notification shows the shot.
//!
//! The bytes uploaded are the ones written to disk (one encode per shot). A PNG over
//! the server limit is re-encoded as lossless WebP, usually 30-50 % smaller. Builds
//! without an update server (the public source) have no sharing at all.

use crate::config::Config;
use crate::encode::Image;
use crate::win::*;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use windows::Win32::Foundation::*;
use windows::Win32::UI::WindowsAndMessaging::*;

/// The server refuses bigger bodies.
pub const LIMIT: usize = 15 * 1024 * 1024;

/// To the main window: wparam = history entry id, lparam = progress in 1/1000.
pub const WM_APP_SHARE_PROGRESS: u32 = WM_APP + 80;
/// To the main window: lparam = Box<Done>.
pub const WM_APP_SHARED: u32 = WM_APP + 81;

/// Server for links: the update server (config `update_url`, by default the one baked
/// in at build time). None = sharing is hidden.
pub fn base(cfg: &Config) -> Option<String> {
    let s = cfg.update_url.trim().trim_end_matches('/');
    (s.starts_with("https://") || s.starts_with("http://")).then(|| s.to_string())
}

pub fn enabled(cfg: &Config) -> bool {
    base(cfg).is_some()
}

#[derive(Clone, Debug, PartialEq)]
pub struct Link {
    pub id: String,
    pub url: String,
    pub delete_token: String,
    /// Unix seconds; 0 = unknown.
    pub expires: u64,
}

pub struct Done {
    pub entry: u64,
    pub result: Result<Link, String>,
}

/// MIME type from the magic bytes: the server accepts exactly these two.
pub fn kind(data: &[u8]) -> Option<&'static str> {
    if data.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if data.len() > 12 && &data[..4] == b"RIFF" && &data[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

fn host(base: &str) -> &str {
    let rest = base.split_once("://").map(|x| x.1).unwrap_or(base);
    rest.split(['/', ':']).next().unwrap_or(rest)
}

fn net_error(base: &str, _e: crate::net::NetError) -> String {
    format!(
        "Cannot reach {}. Check your internet connection.",
        host(base)
    )
}

fn http_error(code: u32, body: &[u8]) -> String {
    let server = serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error")?.as_str().map(String::from));
    match code {
        413 => "The screenshot is larger than 15 MB.".into(),
        429 => "Too many uploads from this network. Try again in an hour.".into(),
        404 | 501 | 503 => "Sharing is not available on this server right now.".into(),
        _ => server.unwrap_or_else(|| format!("The server answered HTTP {code}.")),
    }
}

/// "2026-12-15T10:20:30Z" to Unix seconds.
pub fn parse_iso(s: &str) -> Option<u64> {
    let b = s.as_bytes();
    if b.len() < 19 {
        return None;
    }
    let n = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, m, d) = (n(0..4)?, n(5..7)?, n(8..10)?);
    let (hh, mm, ss) = (n(11..13)?, n(14..16)?, n(17..19)?);
    // Days from civil (Howard Hinnant).
    let y2 = if m <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hh * 3600 + mm * 60 + ss).ok()
}

/// Uploads PNG or WebP bytes. `sent(done, total)` follows the upload.
pub fn upload(base: &str, data: &[u8], sent: impl FnMut(u64, u64)) -> Result<Link, String> {
    let Some(mime) = kind(data) else {
        return Err("Only PNG and WebP screenshots can be shared.".into());
    };
    if data.len() > LIMIT {
        return Err("The screenshot is larger than 15 MB.".into());
    }
    let headers = format!(
        "Content-Type: {mime}\r\nX-Refract-Client: screener/{}",
        env!("CARGO_PKG_VERSION")
    );
    let r = crate::net::request("POST", &format!("{base}/api/s"), &headers, data, sent)
        .map_err(|e| net_error(base, e))?;
    if r.status != 200 && r.status != 201 {
        return Err(http_error(r.status, &r.body));
    }
    let v: serde_json::Value = serde_json::from_slice(&r.body)
        .map_err(|_| "The server sent a broken answer.".to_string())?;
    let s = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let link = Link {
        id: s("id"),
        url: s("url"),
        delete_token: s("deleteToken"),
        expires: parse_iso(&s("expires")).unwrap_or(0),
    };
    let id_ok = !link.id.is_empty() && link.id.bytes().all(|c| c.is_ascii_alphanumeric());
    if !id_ok || !link.url.starts_with("http") || link.delete_token.is_empty() {
        return Err("The server sent a broken answer.".into());
    }
    Ok(link)
}

/// Deletes an uploaded shot. A link that is already gone counts as deleted.
pub fn delete(base: &str, id: &str, token: &str) -> Result<(), String> {
    if id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric()) {
        return Err("Bad link.".into());
    }
    let headers = format!(
        "X-Delete-Token: {token}\r\nX-Refract-Client: screener/{}",
        env!("CARGO_PKG_VERSION")
    );
    let r = crate::net::request(
        "DELETE",
        &format!("{base}/api/s/{id}"),
        &headers,
        &[],
        |_, _| {},
    )
    .map_err(|e| net_error(base, e))?;
    match r.status {
        200 | 204 | 404 | 410 => Ok(()),
        403 => Err("The server did not accept the delete key.".into()),
        c => Err(http_error(c, &r.body)),
    }
}

/// Makes the bytes fit the server limit: a big PNG becomes lossless WebP.
fn fit(data: Vec<u8>, img: Option<&Image>) -> Result<Vec<u8>, String> {
    if data.len() <= LIMIT {
        return Ok(data);
    }
    let decoded;
    let img = match img {
        Some(i) => i,
        None if kind(&data) == Some("image/png") => {
            decoded = crate::history::decode(&data).ok_or("Cannot read the screenshot.")?;
            &decoded
        }
        None => return Err("The screenshot is larger than 15 MB.".into()),
    };
    let webp = crate::encode::webp(img)?;
    if webp.len() <= LIMIT {
        Ok(webp)
    } else {
        Err("The screenshot is larger than 15 MB, even as WebP.".into())
    }
}

fn post(main: isize, msg: u32, wp: usize, lp: isize) -> bool {
    unsafe { PostMessageW(hwnd_from(main), msg, WPARAM(wp), LPARAM(lp)).is_ok() }
}

fn mb(n: u64) -> String {
    format!("{:.1} MB", n as f64 / 1048576.0)
}

pub fn toast_tag(entry: u64) -> String {
    format!("share-{entry}")
}

/// Uploads on the calling (background) thread and reports to the main window:
/// progress for the History window, a progress notification when the upload takes
/// longer than a moment, and the result.
pub fn run(
    main: isize,
    entry: u64,
    base: &str,
    data: Result<Vec<u8>, String>,
    img: Option<&Image>,
) {
    let t0 = Instant::now();
    let result = data.and_then(|d| fit(d, img)).and_then(|data| {
        let mut last = Instant::now() - Duration::from_secs(1);
        let mut toast: Option<crate::toast::Progress> = None;
        let mut toast_at = Instant::now();
        let r = upload(base, &data, |done, total| {
            let now = Instant::now();
            if done < total && now - last < Duration::from_millis(80) {
                return;
            }
            last = now;
            let f = done as f32 / total.max(1) as f32;
            post(
                main,
                WM_APP_SHARE_PROGRESS,
                entry as usize,
                (f * 1000.0) as isize,
            );
            // Slow upload: show a progress bar in a notification.
            if toast.is_none() && t0.elapsed() > Duration::from_millis(900) && f < 0.8 {
                unsafe {
                    let _ = windows::Win32::System::WinRT::RoInitialize(
                        windows::Win32::System::WinRT::RO_INIT_MULTITHREADED,
                    );
                }
                toast = crate::toast::Progress::show(&toast_tag(entry), "Uploading screenshot");
            }
            if let Some(t) = toast.as_mut() {
                if now - toast_at > Duration::from_millis(250) || done == total {
                    toast_at = now;
                    t.set(f, &format!("{} of {}", mb(done), mb(total)), "");
                }
            }
        });
        if r.is_err() && toast.is_some() {
            crate::toast::remove(&toast_tag(entry));
        }
        r
    });
    match &result {
        Ok(l) => log(&format!(
            "shared entry {entry} as {} in {} ms",
            l.url,
            t0.elapsed().as_millis()
        )),
        Err(e) => log(&format!("share of entry {entry} failed: {e}")),
    }
    let p = Box::into_raw(Box::new(Done { entry, result }));
    if !post(main, WM_APP_SHARED, 0, p as isize) {
        drop(unsafe { Box::from_raw(p) });
    }
}

/// Shares a saved file (History window, tray). PNG and WebP files go as they are.
pub fn start_file(main: HWND, entry: u64, base: String, path: PathBuf) {
    let main = hwnd_to(main);
    std::thread::spawn(move || {
        unsafe {
            let _ = windows::Win32::System::Com::CoInitializeEx(
                None,
                windows::Win32::System::Com::COINIT_MULTITHREADED,
            );
        }
        let data =
            std::fs::read(&path).map_err(|e| format!("Cannot read {}: {e}", file_label(&path)));
        run(main, entry, &base, data, None);
    });
}

fn file_label(p: &Path) -> String {
    p.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| p.display().to_string())
}

/// Main thread: an upload finished. Saves the link in the history, copies it and
/// tells the user.
pub fn finished(owner: HWND, d: Done, sound: bool) {
    let thumb = crate::history::thumb_path(d.entry);
    let tag = toast_tag(d.entry);
    match d.result {
        Ok(link) => {
            crate::history::set_link(d.entry, Some(&link));
            crate::history::note("Link copied to the clipboard", false);
            let copied = crate::encode::copy_text(owner, &link.url);
            if sound {
                crate::toast::play_save_sound();
            }
            let title = if copied {
                "Link copied"
            } else {
                "Screenshot uploaded"
            };
            let days = link
                .expires
                .checked_sub(crate::history::now())
                .map(|s| ((s + 43_200) / 86_400).max(1))
                .unwrap_or(30);
            let body = format!(
                "{}\nPaste it anywhere with Ctrl+V. The link works for {days} days.",
                link.url
            );
            let shown = crate::toast::show_tagged(
                &crate::toast::Toast {
                    title,
                    body: &body,
                    image: thumb.as_deref(),
                    open: None,
                    folder: None,
                    launch_uri: Some(&link.url),
                    button: Some(("History", "refract-screener:history")),
                },
                Some(&tag),
            );
            if !shown {
                crate::notify_public(title, &link.url);
            }
        }
        Err(e) => {
            crate::history::set_link(d.entry, None);
            crate::history::note(&e, true);
            let shown = crate::toast::show_tagged(
                &crate::toast::Toast {
                    title: "Could not share the screenshot",
                    body: &e,
                    image: None,
                    open: None,
                    folder: None,
                    launch_uri: None,
                    button: None,
                },
                Some(&tag),
            );
            if !shown {
                crate::notify_public("Could not share the screenshot", &e);
            }
        }
    }
}

/// Deletes a link in the background; the result comes back to `owner` as
/// `msg` with lparam = Box<(u64, Result<(), String>)>.
pub fn start_delete(owner: HWND, msg: u32, entry: u64, base: String, id: String, token: String) {
    let owner = hwnd_to(owner);
    std::thread::spawn(move || {
        let r = delete(&base, &id, &token);
        if let Err(e) = &r {
            log(&format!("delete link {id} failed: {e}"));
        }
        let p = Box::into_raw(Box::new((entry, r)));
        if !post(owner, msg, 0, p as isize) {
            drop(unsafe { Box::from_raw(p) });
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn iso() {
        assert_eq!(super::parse_iso("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            super::parse_iso("2026-11-15T12:00:00Z"),
            Some(1_794_744_000)
        );
    }
}

/// Upload, progress, answer parsing, delete and error mapping against a throwaway
/// HTTP server on 127.0.0.1 (no network needed).
pub fn selftest(png: &[u8], out: &mut dyn FnMut(String)) -> usize {
    use std::io::{Read, Write};
    let Ok(listener) = std::net::TcpListener::bind("127.0.0.1:0") else {
        out("share: cannot open a local port".into());
        return 1;
    };
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(0);
    let base = format!("http://127.0.0.1:{port}");
    let server_base = base.clone();
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for (i, conn) in listener.incoming().take(3).enumerate() {
            let Ok(mut c) = conn else { break };
            let _ = c.set_read_timeout(Some(Duration::from_secs(10)));
            let mut buf = Vec::new();
            let mut tmp = [0u8; 65536];
            let head_end = loop {
                let Ok(n) = c.read(&mut tmp) else { break None };
                if n == 0 {
                    break None;
                }
                buf.extend_from_slice(&tmp[..n]);
                if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break Some(p + 4);
                }
            };
            let Some(head_end) = head_end else { break };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    k.eq_ignore_ascii_case("content-length")
                        .then(|| v.trim().parse::<usize>().ok())?
                })
                .unwrap_or(0);
            while buf.len() < head_end + len {
                let Ok(n) = c.read(&mut tmp) else { break };
                if n == 0 {
                    break;
                }
                buf.extend_from_slice(&tmp[..n]);
            }
            let body = buf[head_end..].to_vec();
            let (code, answer) = match i {
                0 => (
                    "200 OK",
                    format!(
                        "{{\"id\":\"Ab12Cd34Ef\",\"url\":\"{server_base}/s/Ab12Cd34Ef\",\"deleteToken\":\"tok-123\",\"expires\":\"2026-12-15T10:20:30Z\"}}"
                    ),
                ),
                1 if head.contains("X-Delete-Token: tok-123") => ("200 OK", "{\"ok\":true}".into()),
                1 => ("403 Forbidden", "{\"error\":\"Wrong delete key.\"}".into()),
                _ => ("429 Too Many Requests", "{\"error\":\"Slow down.\"}".into()),
            };
            let _ = write!(
                c,
                "HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}",
                answer.len()
            );
            seen.push((head, body));
        }
        seen
    });
    let mut fail = 0;
    let mut check = |name: &str, ok: bool, out: &mut dyn FnMut(String)| {
        if !ok {
            fail += 1;
        }
        out(format!("share {name}: {ok}"));
    };
    let mut calls = 0;
    let mut last = (0, 0);
    let t = Instant::now();
    let up = upload(&base, png, |d, n| {
        calls += 1;
        last = (d, n);
    });
    let ms = t.elapsed().as_millis();
    let up_ok = up.as_ref().is_ok_and(|l| {
        l.id == "Ab12Cd34Ef"
            && l.delete_token == "tok-123"
            && l.url.ends_with("/s/Ab12Cd34Ef")
            && l.expires == parse_iso("2026-12-15T10:20:30Z").unwrap_or(1)
    });
    check("upload answer parsed", up_ok, out);
    check(
        "upload progress reaches the end",
        last == (png.len() as u64, png.len() as u64) && calls >= png.len().div_ceil(65536),
        out,
    );
    let del = delete(&base, "Ab12Cd34Ef", "tok-123");
    check("delete with key", del.is_ok(), out);
    let limited = upload(&base, png, |_, _| {});
    check(
        "HTTP 429 explained",
        limited
            .as_ref()
            .err()
            .is_some_and(|e| e.contains("Too many uploads")),
        out,
    );
    check(
        "non-image refused locally",
        upload(&base, b"GIF89a....", |_, _| {}).is_err(),
        out,
    );
    let seen = server.join().unwrap_or_default();
    let body_ok = seen.first().is_some_and(|(h, b)| {
        h.starts_with("POST /api/s HTTP/1.1")
            && h.contains("Content-Type: image/png")
            && crate::net::sha256(b) == crate::net::sha256(png)
    });
    check("server got the exact bytes", body_ok, out);
    check(
        "delete request shape",
        seen.get(1)
            .is_some_and(|(h, _)| h.starts_with("DELETE /api/s/Ab12Cd34Ef ")),
        out,
    );
    out(format!(
        "share upload of {} KB to localhost: {ms} ms, {calls} progress calls",
        png.len() / 1024
    ));
    fail
}
