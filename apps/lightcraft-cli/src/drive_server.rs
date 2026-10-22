//! `lightcraft-cli drive`: a small HTTP server on this computer for the drive dashboard (the
//! Storage Distributor web app): it serves the dashboard's files and passes its requests on as
//! `drive.*` / `cloud.*` commands, to a headless session on a library or to the running app
//! (`--connect`). Uploads are written to the drive's inbox and handed over with
//! `drive.upload {staged}`; finished downloads are read from its outbox. `docs/cloud-drive.md`.
//!
//! Only loopback: it binds 127.0.0.1, refuses requests whose Host isn't this server (DNS
//! rebinding), and API calls must carry an `X-LightCraft-Drive` header, which a page on another
//! site can't send without a CORS preflight this server never grants.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::Duration;

use lightcraft_mcp::Backend;
use serde_json::{Value, json};

pub const DEFAULT_PORT: u16 = 8790;
/// The header every API request carries.
pub const API_HEADER: &str = "x-lightcraft-drive";
/// Longest request head (request line and headers).
const MAX_HEAD: usize = 16 * 1024;
/// Largest JSON request body.
const MAX_JSON: u64 = 1024 * 1024;
/// Largest upload (the engine stores files up to 4 GiB in this version).
const MAX_UPLOAD: u64 = 4 * 1024 * 1024 * 1024;
/// Connections served at once.
const MAX_CONNECTIONS: usize = 64;

/// One command for the session thread, and where its answer goes.
struct Call {
    command: String,
    params: Value,
    reply: mpsc::Sender<Result<Value, String>>,
}

#[derive(Clone)]
struct Ctx {
    calls: mpsc::Sender<Call>,
    ui: Option<PathBuf>,
    hosts: Arc<Vec<String>>,
}

impl Ctx {
    fn call(&self, command: &str, params: Value) -> Result<Value, String> {
        let (tx, rx) = mpsc::channel();
        self.calls.send(Call { command: command.to_string(), params, reply: tx }).map_err(|_| "LightCraft stopped".to_string())?;
        rx.recv().map_err(|_| "LightCraft stopped".to_string())?
    }
}

/// Commands the dashboard may run: the drive and cloud storage ones, except those that open
/// another library.
pub fn allowed(command: &str) -> bool {
    (command.starts_with("drive.") || command.starts_with("cloud.")) && !matches!(command, "cloud.restore" | "cloud.finishRestore")
}

/// Serve until the process is stopped. `backend` is made on the session thread (a headless
/// session isn't `Send`).
pub fn serve(port: u16, ui: Option<PathBuf>, backend: impl FnOnce() -> Result<Box<dyn Backend>, String> + Send + 'static) -> Result<(), String> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = TcpListener::bind(addr).map_err(|e| format!("can't listen on {addr}: {e}"))?;
    let port = listener.local_addr().map(|a| a.port()).unwrap_or(port);
    let (calls, rx) = mpsc::channel::<Call>();
    let (ready_tx, ready_rx) = mpsc::channel::<Result<String, String>>();
    std::thread::Builder::new()
        .name("lc-drive-session".into())
        .spawn(move || {
            let mut b = match backend() {
                Ok(b) => {
                    let _ = ready_tx.send(Ok(b.describe()));
                    b
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            for c in rx {
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    b.call("engine.execute", json!({"command": c.command, "params": c.params}))
                }))
                .unwrap_or_else(|_| Err(format!("{} stopped unexpectedly", c.command)));
                let _ = c.reply.send(r);
            }
        })
        .map_err(|e| format!("can't start: {e}"))?;
    let describe = ready_rx.recv().map_err(|_| "LightCraft didn't start".to_string())??;
    if let Some(ui) = &ui
        && !ui.join("index.html").is_file()
    {
        return Err(format!("{} has no index.html (build the dashboard with `npm run build` and pass its dist folder)", ui.display()));
    }
    let hosts = Arc::new(vec![format!("127.0.0.1:{port}"), format!("localhost:{port}"), format!("[::1]:{port}")]);
    let ctx = Ctx { calls, ui, hosts };
    eprintln!("lightcraft-cli drive: {describe}; the dashboard is at http://127.0.0.1:{port}/ (Ctrl+C stops)");
    if ctx.ui.is_none() {
        eprintln!("lightcraft-cli drive: no --ui folder: serving the API only (for the dashboard's dev server)");
    }
    let active = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        if active.load(Ordering::Relaxed) >= MAX_CONNECTIONS {
            let mut s = stream;
            let _ = respond(&mut s, 503, "text/plain", b"busy", &[]);
            continue;
        }
        active.fetch_add(1, Ordering::Relaxed);
        let (ctx, mine) = (ctx.clone(), active.clone());
        let spawned = std::thread::Builder::new().name("lc-drive-http".into()).spawn(move || {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| handle(stream, &ctx)));
            mine.fetch_sub(1, Ordering::Relaxed);
        });
        if spawned.is_err() {
            active.fetch_sub(1, Ordering::Relaxed);
        }
    }
    Ok(())
}

struct Request {
    method: String,
    path: String,
    query: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

impl Request {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn param(&self, name: &str) -> Option<&str> {
        self.query.iter().find(|(k, _)| k == name).map(|(_, v)| v.as_str())
    }

    fn content_length(&self) -> Option<u64> {
        self.header("content-length").and_then(|v| v.trim().parse().ok())
    }
}

/// `%XX` and `+` decoded; `None` for a broken escape or bytes that aren't UTF-8.
pub fn percent_decode(s: &str, plus_is_space: bool) -> Option<String> {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        match c {
            b'%' => {
                let hex = s.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' if plus_is_space => {
                out.push(b' ');
                i += 1;
            }
            c => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

fn percent_encode(s: &str) -> String {
    s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect()
}

/// Read the request line and headers.
fn read_head(r: &mut BufReader<TcpStream>) -> Result<Request, (u16, &'static str)> {
    let mut lines = Vec::new();
    let mut total = 0usize;
    loop {
        let mut line = String::new();
        let n = r.by_ref().take((MAX_HEAD - total.min(MAX_HEAD)) as u64 + 1).read_line(&mut line).map_err(|_| (400, "bad request"))?;
        if n == 0 {
            return Err((400, "bad request"));
        }
        total += n;
        if total > MAX_HEAD {
            return Err((431, "request head too large"));
        }
        let line = line.trim_end_matches(['\r', '\n']).to_string();
        if line.is_empty() {
            break;
        }
        lines.push(line);
        if lines.len() > 100 {
            return Err((431, "too many headers"));
        }
    }
    let mut it = lines.into_iter();
    let first = it.next().ok_or((400, "bad request"))?;
    let mut parts = first.split(' ');
    let (Some(method), Some(target)) = (parts.next(), parts.next()) else { return Err((400, "bad request")) };
    let (raw_path, raw_query) = target.split_once('?').unwrap_or((target, ""));
    let path = percent_decode(raw_path, false).ok_or((400, "bad path"))?;
    let mut query = Vec::new();
    for pair in raw_query.split('&').filter(|p| !p.is_empty()) {
        let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
        query.push((percent_decode(k, true).ok_or((400, "bad query"))?, percent_decode(v, true).ok_or((400, "bad query"))?));
    }
    let headers = it.filter_map(|l| l.split_once(':').map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))).collect();
    Ok(Request { method: method.to_string(), path, query, headers })
}

fn status_text(code: u16) -> &'static str {
    match code {
        200 => "OK",
        206 => "Partial Content",
        400 => "Bad Request",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        411 => "Length Required",
        413 => "Payload Too Large",
        416 => "Range Not Satisfiable",
        431 => "Request Header Fields Too Large",
        503 => "Service Unavailable",
        _ => "Error",
    }
}

fn head_bytes(code: u16, content_type: &str, len: u64, extra: &[(&str, String)]) -> Vec<u8> {
    let mut h = format!(
        "HTTP/1.1 {code} {}\r\nContent-Type: {content_type}\r\nContent-Length: {len}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n",
        status_text(code)
    );
    for (k, v) in extra {
        h.push_str(&format!("{k}: {v}\r\n"));
    }
    h.push_str("\r\n");
    h.into_bytes()
}

fn respond(s: &mut TcpStream, code: u16, content_type: &str, body: &[u8], extra: &[(&str, String)]) -> std::io::Result<()> {
    s.write_all(&head_bytes(code, content_type, body.len() as u64, extra))?;
    s.write_all(body)?;
    s.flush()
}

fn respond_json(s: &mut TcpStream, code: u16, v: &Value) -> std::io::Result<()> {
    respond(s, code, "application/json; charset=utf-8", v.to_string().as_bytes(), &[("Cache-Control", "no-store".into())])
}

fn api_error(s: &mut TcpStream, code: u16, msg: &str) -> std::io::Result<()> {
    respond_json(s, code, &json!({"ok": false, "error": msg}))
}

fn handle(stream: TcpStream, ctx: &Ctx) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(60)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(60)));
    let Ok(mut out) = stream.try_clone() else { return };
    let mut reader = BufReader::with_capacity(256 * 1024, stream);
    let req = match read_head(&mut reader) {
        Ok(r) => r,
        Err((code, msg)) => {
            let _ = respond(&mut out, code, "text/plain", msg.as_bytes(), &[]);
            return;
        }
    };
    // DNS rebinding: a page on another name that resolves here must not reach the API
    if !req.header("host").is_some_and(|h| ctx.hosts.iter().any(|x| x.eq_ignore_ascii_case(h))) {
        let _ = respond(&mut out, 403, "text/plain", b"unknown host", &[]);
        return;
    }
    let r = if let Some(rest) = req.path.strip_prefix("/api/") {
        api(rest, &req, &mut reader, &mut out, ctx)
    } else if matches!(req.method.as_str(), "GET" | "HEAD") {
        static_file(&req, &mut out, ctx)
    } else {
        respond(&mut out, 405, "text/plain", b"method not allowed", &[])
    };
    // a client that went away mid-answer is not an error worth reporting
    let _ = r;
}

fn api(route: &str, req: &Request, body: &mut BufReader<TcpStream>, out: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    // file downloads are plain navigations (no custom header): never from another site
    if route.starts_with("file/") && req.method == "GET" {
        if req.header("sec-fetch-site").is_some_and(|s| !matches!(s, "same-origin" | "none")) {
            return api_error(out, 403, "cross-site request");
        }
        return send_download(route.trim_start_matches("file/"), req, out, ctx);
    }
    if req.header(API_HEADER).is_none() {
        return api_error(out, 403, "missing X-LightCraft-Drive header");
    }
    match (req.method.as_str(), route) {
        ("GET", "health") => respond_json(out, 200, &json!({"ok": true, "app": "lightcraft", "version": env!("CARGO_PKG_VERSION")})),
        ("POST", "call") => {
            let Some(len) = req.content_length() else { return api_error(out, 411, "Content-Length required") };
            if len > MAX_JSON {
                return api_error(out, 413, "request too large");
            }
            let mut buf = Vec::new();
            body.take(len).read_to_end(&mut buf)?;
            let Ok(v) = serde_json::from_slice::<Value>(&buf) else { return api_error(out, 400, "the body is not JSON") };
            let Some(command) = v.get("command").and_then(Value::as_str) else { return api_error(out, 400, "missing `command`") };
            if !allowed(command) {
                return api_error(out, 403, &format!("{command} isn't available to the dashboard"));
            }
            let params = v.get("params").cloned().filter(Value::is_object).unwrap_or_else(|| json!({}));
            match ctx.call(command, params) {
                Ok(result) => respond_json(out, 200, &json!({"ok": true, "result": result})),
                Err(e) => respond_json(out, 200, &json!({"ok": false, "error": e})),
            }
        }
        ("PUT" | "POST", "upload") => receive_upload(req, body, out, ctx),
        _ => api_error(out, 404, "no such API"),
    }
}

static UPLOADS: AtomicU64 = AtomicU64::new(0);

/// Write the body to the drive's inbox and queue it with `drive.upload {staged}`.
fn receive_upload(req: &Request, body: &mut BufReader<TcpStream>, out: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let Some(len) = req.content_length() else { return api_error(out, 411, "Content-Length required") };
    if len > MAX_UPLOAD {
        return api_error(out, 413, "files over 4 GB can't be stored yet");
    }
    let Some(name) = req.param("name").filter(|n| !n.trim().is_empty()) else { return api_error(out, 400, "missing `name`") };
    let status = match ctx.call("drive.status", json!({})) {
        Ok(s) => s,
        Err(e) => return api_error(out, 200, &e),
    };
    let Some(inbox) = status.get("inbox").and_then(Value::as_str).map(PathBuf::from) else {
        return api_error(out, 200, "the drive is locked: unlock it first");
    };
    if let Err(e) = std::fs::create_dir_all(&inbox) {
        return api_error(out, 200, &format!("can't create the upload inbox: {e}"));
    }
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let path = inbox.join(format!("upload-{}-{nanos}-{}.part", std::process::id(), UPLOADS.fetch_add(1, Ordering::Relaxed)));
    let copied = (|| -> std::io::Result<u64> {
        let mut f = std::fs::File::create(&path)?;
        let n = std::io::copy(&mut body.take(len), &mut f)?;
        f.sync_all()?;
        Ok(n)
    })();
    match copied {
        Ok(n) if n == len => {}
        Ok(_) => {
            let _ = std::fs::remove_file(&path);
            return api_error(out, 400, "the upload was cut off");
        }
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            return api_error(out, 200, &format!("can't save the upload: {e}"));
        }
    }
    let mut params = json!({"path": path.to_string_lossy(), "name": name, "staged": true});
    for key in ["parent", "level"] {
        if let Some(v) = req.param(key).filter(|v| !v.is_empty()) {
            params[key] = json!(v);
        }
    }
    if let Some(c) = req.param("copies").and_then(|c| c.parse::<u64>().ok()) {
        params["copies"] = json!(c);
    }
    match ctx.call("drive.upload", params) {
        Ok(result) => respond_json(out, 200, &json!({"ok": true, "result": result})),
        Err(e) => {
            let _ = std::fs::remove_file(&path);
            respond_json(out, 200, &json!({"ok": false, "error": e}))
        }
    }
}

/// The content type for a downloaded file shown in the browser; `None` when it must be saved
/// rather than shown (HTML, SVG, scripts: they could run in this origin).
pub fn inline_type(name: &str) -> Option<&'static str> {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    Some(match ext.as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        "mp4" | "m4v" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" | "opus" => "audio/ogg",
        "m4a" | "aac" => "audio/mp4",
        "flac" => "audio/flac",
        "txt" | "md" | "csv" | "log" | "json" | "xml" | "yml" | "yaml" | "toml" | "rs" | "ts" | "js" | "py" | "sh" | "css" | "html" | "htm"
        | "svg" => "text/plain; charset=utf-8",
        _ => return None,
    })
}

/// `bytes=a-b` / `bytes=a-` / `bytes=-n` within `len`: (start, end inclusive).
pub fn parse_range(v: &str, len: u64) -> Option<(u64, u64)> {
    let spec = v.trim().strip_prefix("bytes=")?;
    if spec.contains(',') || len == 0 {
        return None;
    }
    let (a, b) = spec.split_once('-')?;
    let (start, end) = match (a.trim(), b.trim()) {
        ("", n) => {
            let n: u64 = n.parse().ok()?;
            (len.saturating_sub(n), len - 1)
        }
        (a, "") => (a.parse().ok()?, len - 1),
        (a, b) => (a.parse().ok()?, b.parse::<u64>().ok()?.min(len - 1)),
    };
    (start <= end && start < len).then_some((start, end))
}

fn send_download(task: &str, req: &Request, out: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let info = match ctx.call("drive.file", json!({"task": task})) {
        Ok(v) => v,
        Err(e) => return api_error(out, 404, &e),
    };
    let (Some(path), Some(name)) = (info.get("path").and_then(Value::as_str), info.get("name").and_then(Value::as_str)) else {
        return api_error(out, 404, "no such download");
    };
    let Ok(mut f) = std::fs::File::open(path) else { return api_error(out, 404, "the download is gone") };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let inline = req.param("inline").is_some_and(|v| v == "1");
    let shown = if inline { inline_type(name) } else { None };
    let ascii: String = name.chars().map(|c| if c.is_ascii_graphic() && c != '"' && c != '\\' || c == ' ' { c } else { '_' }).collect();
    let disposition =
        format!("{}; filename=\"{ascii}\"; filename*=UTF-8''{}", if shown.is_some() { "inline" } else { "attachment" }, percent_encode(name));
    let mut extra = vec![
        ("Content-Disposition", disposition),
        ("Cache-Control", "private, max-age=600".to_string()),
        ("Content-Security-Policy", "sandbox; default-src 'none'; img-src 'self'; media-src 'self'; style-src 'unsafe-inline'".to_string()),
        ("Accept-Ranges", "bytes".to_string()),
    ];
    let content_type = shown.unwrap_or("application/octet-stream");
    let range = req.header("range").map(|r| parse_range(r, len));
    let (code, start, count) = match range {
        None => (200, 0, len),
        Some(Some((a, b))) => {
            extra.push(("Content-Range", format!("bytes {a}-{b}/{len}")));
            (206, a, b - a + 1)
        }
        Some(None) => {
            return respond(out, 416, "text/plain", b"bad range", &[("Content-Range", format!("bytes */{len}"))]);
        }
    };
    out.write_all(&head_bytes(code, content_type, count, &extra))?;
    if req.method == "HEAD" {
        return out.flush();
    }
    std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(start))?;
    std::io::copy(&mut f.take(count), out)?;
    out.flush()
}

/// The file under `root` for a URL path; `None` for anything that could leave it.
pub fn safe_static_path(root: &Path, url_path: &str) -> Option<PathBuf> {
    let mut p = root.to_path_buf();
    for seg in url_path.split('/').filter(|s| !s.is_empty()) {
        if seg.starts_with('.') || seg.contains('\\') || seg.contains('\0') || seg.contains(':') {
            return None;
        }
        let c = Path::new(seg).components().next()?;
        if !matches!(c, Component::Normal(_)) {
            return None;
        }
        p.push(seg);
    }
    Some(p)
}

fn static_type(path: &Path) -> &'static str {
    match path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).as_deref() {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("ico") => "image/x-icon",
        Some("json") => "application/json",
        Some("webmanifest") => "application/manifest+json",
        Some("woff2") => "font/woff2",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

const NO_UI: &str = "<!doctype html><meta charset=utf-8><title>LightCraft Drive</title><body style=\"font-family:system-ui;max-width:40rem;margin:4rem auto;line-height:1.5\"><h1>LightCraft Drive</h1><p>This server runs the drive's API only. Build the dashboard (<code>npm run build</code> in the storage-distributor project) and start it with <code>lightcraft-cli drive --ui path/to/dist</code>, or run the dashboard's dev server (<code>npm run dev</code>).</p>";

fn static_file(req: &Request, out: &mut TcpStream, ctx: &Ctx) -> std::io::Result<()> {
    let Some(root) = &ctx.ui else {
        return respond(out, 200, "text/html; charset=utf-8", NO_UI.as_bytes(), &[]);
    };
    let Some(mut path) = safe_static_path(root, &req.path) else { return respond(out, 404, "text/plain", b"not found", &[]) };
    if path.is_dir() {
        path = path.join("index.html");
    }
    if !path.is_file() {
        // the app routes with #hashes; anything else unknown is the app itself
        if Path::new(&req.path).extension().is_some() {
            return respond(out, 404, "text/plain", b"not found", &[]);
        }
        path = root.join("index.html");
    }
    let Ok(data) = std::fs::read(&path) else { return respond(out, 404, "text/plain", b"not found", &[]) };
    let cache = if path.components().any(|c| c.as_os_str() == "assets") { "public, max-age=31536000, immutable" } else { "no-cache" };
    let mut extra = vec![("Cache-Control", cache.to_string())];
    if static_type(&path).starts_with("text/html") {
        extra.push(("Content-Security-Policy", "default-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; style-src 'self' 'unsafe-inline'; frame-src 'self'; object-src 'none'; base-uri 'none'; frame-ancestors 'none'".to_string()));
    }
    if req.method == "HEAD" {
        out.write_all(&head_bytes(200, static_type(&path), data.len() as u64, &extra))?;
        return out.flush();
    }
    respond(out, 200, static_type(&path), &data, &extra)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decoding_and_paths() {
        assert_eq!(percent_decode("a%20b+c", true).as_deref(), Some("a b c"));
        assert_eq!(percent_decode("a+b", false).as_deref(), Some("a+b"));
        assert_eq!(percent_decode("%E2%9C%93", false).as_deref(), Some("✓"));
        assert_eq!(percent_decode("%zz", false), None);
        assert_eq!(percent_decode("%4", false), None);
        assert_eq!(percent_decode("%FF", false), None, "not UTF-8");
        let root = Path::new("/srv/ui");
        assert_eq!(safe_static_path(root, "/assets/app.js"), Some(PathBuf::from("/srv/ui/assets/app.js")));
        for bad in ["/../etc/passwd", "/a/../../x", "/.git/config", "/a\\..\\b", "/C:/x"] {
            assert_eq!(safe_static_path(root, bad), None, "{bad}");
        }
        assert_eq!(percent_encode("a b/é"), "a%20b%2F%C3%A9");
    }

    #[test]
    fn ranges_and_types() {
        assert_eq!(parse_range("bytes=0-9", 100), Some((0, 9)));
        assert_eq!(parse_range("bytes=90-", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=-10", 100), Some((90, 99)));
        assert_eq!(parse_range("bytes=50-500", 100), Some((50, 99)));
        assert_eq!(parse_range("bytes=100-", 100), None);
        assert_eq!(parse_range("bytes=5-1", 100), None);
        assert_eq!(parse_range("bytes=0-1,4-5", 100), None);
        assert_eq!(parse_range("bytes=0-", 0), None);
        assert_eq!(inline_type("a.JPG"), Some("image/jpeg"));
        assert_eq!(inline_type("page.html"), Some("text/plain; charset=utf-8"), "never rendered as HTML");
        assert_eq!(inline_type("archive.zip"), None);
        assert!(allowed("drive.list") && allowed("cloud.status"));
        assert!(!allowed("cloud.restore") && !allowed("photo.delete") && !allowed("library.import"));
    }
}
