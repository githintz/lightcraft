//! What the network providers share: timeouts, reading capped bodies, mapping HTTP statuses
//! to [`CloudError`]s, percent-encoding, base64 and UTC timestamps. Native only.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use lightcraft_fetch::http::{self, HttpError, Limits, Response, Url};

use crate::{CloudError, Result};

/// Largest error or listing body read (an XML error, one listing page).
pub const MAX_META_BODY: u64 = 8 * 1024 * 1024;

/// Connection settings shared by a provider's requests; `cancel` stops a transfer in progress
/// from another thread.
#[derive(Clone, Debug)]
pub struct Net {
    pub connect: Duration,
    pub stall: Duration,
    pub cancel: Arc<AtomicBool>,
}

impl Default for Net {
    fn default() -> Self {
        Net { connect: Duration::from_secs(15), stall: Duration::from_secs(60), cancel: Arc::new(AtomicBool::new(false)) }
    }
}

impl From<HttpError> for CloudError {
    fn from(e: HttpError) -> Self {
        match e {
            HttpError::Cancelled => CloudError::Io("cancelled".into()),
            other => CloudError::Io(other.to_string()),
        }
    }
}

impl Net {
    pub fn send(&self, method: &str, url: &Url, headers: &[(&str, String)], body: &[u8]) -> Result<Response> {
        let limits = Limits { connect: self.connect, stall: self.stall, cancel: &self.cancel };
        Ok(http::request(method, url, headers, body, &limits)?)
    }

    /// The whole body, refused with [`CloudError::TooLarge`] once it passes `max` (before reading
    /// when the server announces its length).
    pub fn body(&self, resp: &mut Response, max: u64) -> Result<Vec<u8>> {
        if let Some(n) = resp.content_length()
            && n > max
        {
            return Err(CloudError::TooLarge { size: n, max });
        }
        let limits = Limits { connect: self.connect, stall: self.stall, cancel: &self.cancel };
        let mut out = Vec::with_capacity(resp.content_length().and_then(|n| usize::try_from(n).ok()).unwrap_or(0).min(64 << 20));
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            let n = resp.read(&mut buf, &limits)?;
            if n == 0 {
                return Ok(out);
            }
            out.extend_from_slice(buf.get(..n).unwrap_or_default());
            if out.len() as u64 > max {
                return Err(CloudError::TooLarge { size: out.len() as u64, max });
            }
        }
    }

    /// Turn a failed response into an error, with the server's message when it sends a short one.
    pub fn status_error(&self, what: &str, resp: &mut Response) -> CloudError {
        let status = resp.status;
        let detail =
            self.body(resp, 64 * 1024).ok().map(|b| server_message(&b)).filter(|m| !m.is_empty()).map(|m| format!(": {m}")).unwrap_or_default();
        match status {
            404 => CloudError::NotFound,
            401 | 403 => CloudError::Denied(format!("{what}: HTTP {status}{detail}")),
            413 => CloudError::TooLarge { size: 0, max: 0 },
            507 => CloudError::QuotaExceeded,
            _ => CloudError::Io(format!("{what}: HTTP {status}{detail}")),
        }
    }
}

/// Longest wait a server's `Retry-After` may ask for.
pub const MAX_RETRY_AFTER: Duration = Duration::from_secs(120);

/// The wait a `Retry-After: <seconds>` header asks for, capped at [`MAX_RETRY_AFTER`].
pub fn retry_after(resp: &Response) -> Option<Duration> {
    let secs: u64 = resp.header("Retry-After")?.trim().parse().ok()?;
    Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER))
}

impl Net {
    /// Wait before retry number `attempt` (from 0): `base` doubled each time, or what the server
    /// asked for, whichever is longer. A raised cancel flag ends the wait with an error.
    pub fn backoff(&self, base: Duration, attempt: u32, server: Option<Duration>) -> Result<()> {
        let wait = base.saturating_mul(1u32 << attempt.min(6)).max(server.unwrap_or_default()).min(MAX_RETRY_AFTER);
        let now = std::time::Instant::now();
        let deadline = now.checked_add(wait).unwrap_or(now);
        loop {
            if self.cancel.load(std::sync::atomic::Ordering::Relaxed) {
                return Err(CloudError::Io("cancelled".into()));
            }
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                return Ok(());
            }
            std::thread::sleep(left.min(Duration::from_millis(200)));
        }
    }
}

/// The `<Code>`/`<Message>` of an S3-style XML error, else the start of the body as text.
fn server_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let tag = |name: &str| {
        let open = format!("<{name}>");
        let start = text.find(&open)? + open.len();
        let end = text.get(start..)?.find('<')? + start;
        text.get(start..end).map(str::trim).map(str::to_string)
    };
    match (tag("Code"), tag("Message")) {
        (Some(c), Some(m)) => format!("{c} ({m})"),
        (Some(c), None) => c,
        // an HTML or XML page says no more than the status does
        _ if text.trim_start().starts_with('<') => String::new(),
        _ => text.chars().filter(|c| !c.is_control()).take(200).collect::<String>().trim().to_string(),
    }
}

/// This computer (`localhost`, `127.x.x.x`, `::1`): the one place credentials may go over plain
/// `http://` (local test servers, a loopback redirect).
pub fn is_local(host: &str) -> bool {
    host == "localhost" || host == "::1" || host.starts_with("127.")
}

/// RFC 3986 percent-encoding of everything but unreserved characters (and `/` when `keep_slash`).
pub fn uri_encode(s: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') || (keep_slash && b == b'/') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Undo percent-encoding; `None` for malformed escapes or non-UTF-8 results.
pub fn uri_decode(s: &str) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let c = *b.get(i)?;
        if c == b'%' {
            let hex = std::str::from_utf8(b.get(i + 1..i + 3)?).ok()?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(c);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

pub fn base64(data: &[u8]) -> String {
    const A: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk.first().copied().unwrap_or(0), chunk.get(1).copied().unwrap_or(0), chunk.get(2).copied().unwrap_or(0)];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18u32, 12, 6, 0].iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(A[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Base64 with the URL-safe alphabet and no padding (RFC 4648 §5), as PKCE and JWTs use.
pub fn base64url(data: &[u8]) -> String {
    base64(data)
        .trim_end_matches('=')
        .chars()
        .map(|c| match c {
            '+' => '-',
            '/' => '_',
            c => c,
        })
        .collect()
}

/// `application/x-www-form-urlencoded` body or query string from name/value pairs.
pub fn form(pairs: &[(&str, &str)]) -> String {
    pairs.iter().map(|(k, v)| format!("{}={}", uri_encode(k, false), uri_encode(v, false))).collect::<Vec<_>>().join("&")
}

/// The pairs of a form-encoded query string (`+` is a space); malformed escapes are dropped.
pub fn parse_form(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|p| !p.is_empty())
        .filter_map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            Some((uri_decode(&k.replace('+', " "))?, uri_decode(&v.replace('+', " "))?))
        })
        .collect()
}

pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Seconds since 1970 as `(YYYYMMDDTHHMMSSZ, YYYYMMDD)` in UTC.
pub fn amz_date(unix: u64) -> (String, String) {
    let days = i64::try_from(unix / 86_400).unwrap_or(0);
    let secs = unix % 86_400;
    // civil-from-days (H. Hinnant's algorithm, public domain)
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    let date = format!("{y:04}{m:02}{d:02}");
    (format!("{date}T{:02}{:02}{:02}Z", secs / 3600, secs / 60 % 60, secs % 60), date)
}

pub fn now_unix() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// The text between `<name>` and `</name>` for every element named `name` (any namespace
/// prefix), in document order. Enough for S3 listings and WebDAV multistatus replies; entity
/// references are decoded.
pub fn xml_elements(xml: &[u8], name: &str) -> Result<Vec<String>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_reader(xml);
    let mut out = Vec::new();
    let mut depth_in: Option<String> = None;
    let mut buf = Vec::new();
    loop {
        let ev = reader.read_event_into(&mut buf).map_err(|e| CloudError::Corrupt(format!("XML: {e}")))?;
        match ev {
            Event::Start(e) if e.local_name().as_ref() == name.as_bytes() => depth_in = Some(String::new()),
            Event::Text(t) => {
                if let Some(s) = depth_in.as_mut() {
                    let text = t.decode().map_err(|e| CloudError::Corrupt(format!("XML: {e}")))?;
                    s.push_str(&quick_xml::escape::unescape(&text).map_err(|e| CloudError::Corrupt(format!("XML: {e}")))?);
                }
            }
            Event::GeneralRef(r) => {
                if let Some(s) = depth_in.as_mut() {
                    let name = r.decode().map_err(|e| CloudError::Corrupt(format!("XML: {e}")))?;
                    let entity = format!("&{name};");
                    let text = quick_xml::escape::unescape(&entity).map_err(|e| CloudError::Corrupt(format!("XML: {e}")))?;
                    s.push_str(&text);
                }
            }
            Event::End(e) if e.local_name().as_ref() == name.as_bytes() => {
                if let Some(s) = depth_in.take() {
                    out.push(s);
                }
            }
            Event::Eof => return Ok(out),
            _ => {}
        }
        buf.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates() {
        assert_eq!(amz_date(1_440_938_160), ("20150830T123600Z".into(), "20150830".into()));
        assert_eq!(amz_date(0).0, "19700101T000000Z");
        assert_eq!(amz_date(951_782_400).1, "20000229");
        assert_eq!(amz_date(1_791_635_696).0, "20261010T123456Z");
    }

    #[test]
    fn encodings() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"Aladdin:open sesame"), "QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert_eq!(uri_encode("a+b=/ é", false), "a%2Bb%3D%2F%20%C3%A9");
        assert_eq!(uri_encode("lc/blobs", true), "lc/blobs");
        assert_eq!(uri_decode("a%2Bb%20c").as_deref(), Some("a+b c"));
        assert_eq!(uri_decode("bad%2"), None);
        assert_eq!(uri_decode("%FF"), None);
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(form(&[("a b", "c&d=é")]), "a%20b=c%26d%3D%C3%A9");
        assert_eq!(
            parse_form("code=4%2F0A+x&state=s&&bad=%zz&flag"),
            [("code".into(), "4/0A x".into()), ("state".into(), "s".into()), ("flag".into(), String::new())]
        );
    }

    #[test]
    fn xml_text_with_prefixes_and_entities() {
        let xml = b"<?xml version=\"1.0\"?><d:multistatus xmlns:d=\"DAV:\"><d:href>/a%20b/</d:href><D:href>/x&amp;y</D:href></d:multistatus>";
        assert_eq!(xml_elements(xml, "href").unwrap(), ["/a%20b/", "/x&y"]);
        // unterminated documents never panic
        let _ = xml_elements(b"<a><b>text", "b");
        assert_eq!(server_message(b"<Error><Code>NoSuchBucket</Code><Message>gone</Message></Error>"), "NoSuchBucket (gone)");
        assert_eq!(server_message(b"<html><h1>401 Access not authorized</h1></html>"), "");
        assert_eq!(server_message(b"quota exceeded\r\n"), "quota exceeded");
    }
}
