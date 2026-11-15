//! Small HTTPS client (WinHTTP: system proxy, Windows TLS) and SHA-256 (CNG),
//! shared by the app (auto-update) and RefractScreenerSetup.exe.

use windows::core::{w, PCWSTR};
use windows::Win32::Networking::WinHttp::*;
use windows::Win32::Security::Cryptography::*;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

pub fn sha256(data: &[u8]) -> String {
    let mut out = [0u8; 32];
    unsafe {
        let _ = BCryptHash(BCRYPT_SHA256_ALG_HANDLE, None, data, &mut out);
    }
    out.iter().map(|b| format!("{b:02x}")).collect()
}

/// HTTP(S) GET with WinHTTP (system proxy settings, TLS from Windows).
pub fn http_get(url: &str, mut progress: impl FnMut(u64, u64)) -> Result<Vec<u8>, String> {
    let net = |what: &str| {
        format!("Cannot reach the update server ({what}). Check your internet connection.")
    };
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err("bad url".into());
    };
    let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(if secure { 443 } else { 80 })),
        None => (hostport, if secure { 443 } else { 80 }),
    };
    unsafe {
        let agent = wide(concat!(
            "Refract-screener-setup/",
            env!("CARGO_PKG_VERSION")
        ));
        let mut session = WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            WINHTTP_ACCESS_TYPE_AUTOMATIC_PROXY,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        );
        if session.is_null() {
            // Windows before 8.1 has no automatic proxy mode.
            session = WinHttpOpen(
                PCWSTR(agent.as_ptr()),
                WINHTTP_ACCESS_TYPE_DEFAULT_PROXY,
                PCWSTR::null(),
                PCWSTR::null(),
                0,
            );
        }
        if session.is_null() {
            return Err(net("session"));
        }
        let _ = WinHttpSetTimeouts(session, 15_000, 15_000, 30_000, 60_000);
        let h = wide(host);
        let conn = WinHttpConnect(session, PCWSTR(h.as_ptr()), port, 0);
        let p = wide(path);
        let req = if conn.is_null() {
            std::ptr::null_mut()
        } else {
            WinHttpOpenRequest(
                conn,
                w!("GET"),
                PCWSTR(p.as_ptr()),
                PCWSTR::null(),
                PCWSTR::null(),
                std::ptr::null(),
                if secure {
                    WINHTTP_FLAG_SECURE
                } else {
                    WINHTTP_OPEN_REQUEST_FLAGS(0)
                },
            )
        };
        let close = || {
            if !req.is_null() {
                let _ = WinHttpCloseHandle(req);
            }
            if !conn.is_null() {
                let _ = WinHttpCloseHandle(conn);
            }
            let _ = WinHttpCloseHandle(session);
        };
        if req.is_null() {
            close();
            return Err(net("connect"));
        }
        if WinHttpSendRequest(req, None, None, 0, 0, 0).is_err()
            || WinHttpReceiveResponse(req, std::ptr::null_mut()).is_err()
        {
            close();
            return Err(net("request"));
        }
        let mut code = 0u32;
        let mut len = 4u32;
        let _ = WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut code as *mut _ as *mut _),
            &mut len,
            std::ptr::null_mut(),
        );
        let mut total = 0u64;
        let mut tl = 8u32;
        let _ = WinHttpQueryHeaders(
            req,
            WINHTTP_QUERY_CONTENT_LENGTH | WINHTTP_QUERY_FLAG_NUMBER64,
            PCWSTR::null(),
            Some(&mut total as *mut _ as *mut _),
            &mut tl,
            std::ptr::null_mut(),
        );
        if code != 200 {
            close();
            return Err(if code == 404 || code == 503 {
                "Refract-screener is not available for download right now.".into()
            } else {
                format!("The update server answered HTTP {code}.")
            });
        }
        let mut out = Vec::with_capacity(total as usize);
        let mut buf = vec![0u8; 256 * 1024];
        loop {
            let mut n = 0u32;
            if WinHttpReadData(req, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut n).is_err() {
                close();
                return Err(net("download interrupted"));
            }
            if n == 0 {
                break;
            }
            out.extend_from_slice(&buf[..n as usize]);
            progress(out.len() as u64, total);
            if out.len() > 512 * 1024 * 1024 {
                close();
                return Err("download too large".into());
            }
        }
        close();
        Ok(out)
    }
}
