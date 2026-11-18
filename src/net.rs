//! Small HTTPS client (WinHTTP: system proxy, Windows TLS) and SHA-256 (CNG),
//! shared by the app (auto-update, share links) and RefractScreenerSetup.exe.

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

/// Session, connection and request handles; closed in reverse order on drop.
struct Handles {
    session: *mut core::ffi::c_void,
    conn: *mut core::ffi::c_void,
    req: *mut core::ffi::c_void,
}

impl Drop for Handles {
    fn drop(&mut self) {
        unsafe {
            for h in [self.req, self.conn, self.session] {
                if !h.is_null() {
                    let _ = WinHttpCloseHandle(h);
                }
            }
        }
    }
}

/// Why a request failed before an HTTP status arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NetError {
    BadUrl,
    Session,
    Connect,
    Send,
    Receive,
    Read,
    TooLarge,
}

impl NetError {
    pub fn what(self) -> &'static str {
        match self {
            NetError::BadUrl => "bad url",
            NetError::Session => "session",
            NetError::Connect => "connect",
            NetError::Send => "request",
            NetError::Receive => "request",
            NetError::Read => "download interrupted",
            NetError::TooLarge => "download too large",
        }
    }
}

unsafe fn open(method: PCWSTR, url: &str, agent: &str) -> Result<Handles, NetError> {
    let (secure, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(NetError::BadUrl);
    };
    let (hostport, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let path = if path.is_empty() { "/" } else { path };
    let (host, port) = match hostport.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>().unwrap_or(if secure { 443 } else { 80 })),
        None => (hostport, if secure { 443 } else { 80 }),
    };
    let agent = wide(agent);
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
    let mut hs = Handles {
        session,
        conn: std::ptr::null_mut(),
        req: std::ptr::null_mut(),
    };
    if session.is_null() {
        return Err(NetError::Session);
    }
    let _ = WinHttpSetTimeouts(session, 15_000, 15_000, 30_000, 60_000);
    let h = wide(host);
    hs.conn = WinHttpConnect(session, PCWSTR(h.as_ptr()), port, 0);
    if hs.conn.is_null() {
        return Err(NetError::Connect);
    }
    let p = wide(path);
    hs.req = WinHttpOpenRequest(
        hs.conn,
        method,
        PCWSTR(p.as_ptr()),
        PCWSTR::null(),
        PCWSTR::null(),
        std::ptr::null(),
        if secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        },
    );
    if hs.req.is_null() {
        return Err(NetError::Connect);
    }
    Ok(hs)
}

unsafe fn status(req: *mut core::ffi::c_void) -> (u32, u64) {
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
    (code, total)
}

unsafe fn read_body(
    req: *mut core::ffi::c_void,
    total: u64,
    limit: usize,
    mut progress: impl FnMut(u64, u64),
) -> Result<Vec<u8>, NetError> {
    let mut out = Vec::with_capacity((total as usize).min(limit));
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        let mut n = 0u32;
        if WinHttpReadData(req, buf.as_mut_ptr() as *mut _, buf.len() as u32, &mut n).is_err() {
            return Err(NetError::Read);
        }
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n as usize]);
        progress(out.len() as u64, total);
        if out.len() > limit {
            return Err(NetError::TooLarge);
        }
    }
    Ok(out)
}

/// HTTP(S) GET with WinHTTP (system proxy settings, TLS from Windows).
pub fn http_get(url: &str, progress: impl FnMut(u64, u64)) -> Result<Vec<u8>, String> {
    let net = |what: &str| {
        format!("Cannot reach the update server ({what}). Check your internet connection.")
    };
    unsafe {
        let hs = open(
            w!("GET"),
            url,
            concat!("Refract-screener-setup/", env!("CARGO_PKG_VERSION")),
        )
        .map_err(|e| net(e.what()))?;
        if WinHttpSendRequest(hs.req, None, None, 0, 0, 0).is_err()
            || WinHttpReceiveResponse(hs.req, std::ptr::null_mut()).is_err()
        {
            return Err(net("request"));
        }
        let (code, total) = status(hs.req);
        if code != 200 {
            return Err(if code == 404 || code == 503 {
                "Refract-screener is not available for download right now.".into()
            } else {
                format!("The update server answered HTTP {code}.")
            });
        }
        read_body(hs.req, total, 512 * 1024 * 1024, progress).map_err(|e| match e {
            NetError::TooLarge => e.what().to_string(),
            _ => net(e.what()),
        })
    }
}

#[allow(dead_code)] // RefractScreenerSetup.exe only downloads
pub struct Response {
    pub status: u32,
    pub body: Vec<u8>,
}

#[allow(dead_code)]
/// Any request with an optional body. The body goes out in 64 KB pieces and
/// `sent(done, total)` is called after each, so a caller can show upload progress.
/// `headers` are extra "Name: value" lines separated by "\r\n". Any HTTP status is
/// returned as Ok; only network failures are errors. The response body is capped at 1 MB.
pub fn request(
    method: &str,
    url: &str,
    headers: &str,
    body: &[u8],
    mut sent: impl FnMut(u64, u64),
) -> Result<Response, NetError> {
    unsafe {
        let m = wide(method);
        let hs = open(
            PCWSTR(m.as_ptr()),
            url,
            concat!("Refract-screener/", env!("CARGO_PKG_VERSION")),
        )?;
        let hdr: Vec<u16> = headers.encode_utf16().collect();
        let total = body.len() as u64;
        if WinHttpSendRequest(
            hs.req,
            if hdr.is_empty() { None } else { Some(&hdr) },
            None,
            0,
            body.len() as u32,
            0,
        )
        .is_err()
        {
            return Err(NetError::Send);
        }
        let mut done = 0usize;
        for chunk in body.chunks(64 * 1024) {
            let mut n = 0u32;
            if WinHttpWriteData(
                hs.req,
                Some(chunk.as_ptr() as *const _),
                chunk.len() as u32,
                &mut n,
            )
            .is_err()
                || n as usize != chunk.len()
            {
                return Err(NetError::Send);
            }
            done += chunk.len();
            sent(done as u64, total);
        }
        if WinHttpReceiveResponse(hs.req, std::ptr::null_mut()).is_err() {
            return Err(NetError::Receive);
        }
        let (code, len) = status(hs.req);
        let body = read_body(hs.req, len, 1024 * 1024, |_, _| {})?;
        Ok(Response { status: code, body })
    }
}
