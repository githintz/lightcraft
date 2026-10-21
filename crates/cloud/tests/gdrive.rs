//! The Google Drive provider and the OAuth sign-in against a fake Drive on `127.0.0.1`: the
//! subset of the v3 REST API LightCraft uses (search queries, multipart and resumable uploads,
//! downloads, deletes, quota, the token endpoint), with fault injection. Offline.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use lightcraft_cloud::crypto::OsRandom;
use lightcraft_cloud::gdrive::{GoogleDriveConfig, GoogleDriveProvider, MULTIPART_MAX};
use lightcraft_cloud::oauth::{LoopbackFlow, OAuthClient, pkce_challenge};
use lightcraft_cloud::{CloudError, Net, ObjectName, Provider, RemoteRef, contract, store_blob};
use serde_json::{Value, json};

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const REFRESH: &str = "good-refresh";

#[derive(Clone, Debug)]
struct File {
    name: String,
    parents: Vec<String>,
    mime: String,
    data: Vec<u8>,
}

struct Session {
    file: Option<String>,
    meta: Value,
    total: usize,
    data: Vec<u8>,
}

#[derive(Default)]
struct State {
    port: u16,
    /// Insertion order is creation order.
    files: Vec<(String, File)>,
    sessions: HashMap<String, Session>,
    next: u64,
    tokens: Vec<String>,
    refreshes: u32,
    page_size: usize,
    /// The next N API calls answer 429 / 403 rateLimitExceeded, alternately.
    rate_limited: u32,
    /// The next N upload chunks keep half their bytes and answer 503.
    failing_chunks: u32,
    /// Status queries (`Content-Range: bytes */N`) on upload sessions.
    asks: u32,
    /// Sign-in: the PKCE challenge and code the fake expects.
    challenge: Option<String>,
    /// Upload sessions answer every chunk with a held size that alternates between one chunk
    /// and none (a hostile or broken server).
    flapping: bool,
    /// A session address on another server.
    evil_location: bool,
    log: Vec<String>,
}

impl State {
    fn id(&mut self) -> String {
        self.next += 1;
        format!("id_{}", self.next)
    }
    fn file(&self, id: &str) -> Option<&File> {
        self.files.iter().find(|(i, _)| i == id).map(|(_, f)| f)
    }
    fn create(&mut self, meta: &Value, data: Vec<u8>) -> String {
        let id = self.id();
        let parents = meta["parents"].as_array().map(|a| a.iter().filter_map(|p| p.as_str().map(str::to_string)).collect()).unwrap_or_default();
        let file = File {
            name: meta["name"].as_str().unwrap_or("untitled").to_string(),
            parents,
            mime: meta["mimeType"].as_str().unwrap_or("application/octet-stream").to_string(),
            data,
        };
        self.files.push((id.clone(), file));
        id
    }
}

type Shared = Arc<Mutex<State>>;

fn start() -> (Shared, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: Shared = Arc::new(Mutex::new(State { port, page_size: 1000, ..State::default() }));
    let s = state.clone();
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let _ = serve(&s, conn);
        }
    });
    (state, format!("http://127.0.0.1:{port}"))
}

fn decode(s: &str) -> String {
    let b = s.replace('+', " ");
    let b = b.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            out.push(u8::from_str_radix(std::str::from_utf8(&b[i + 1..i + 3]).unwrap(), 16).unwrap());
            i += 3;
        } else {
            out.push(b[i]);
            i += 1;
        }
    }
    String::from_utf8(out).unwrap()
}

fn params(query: &str) -> HashMap<String, String> {
    query.split('&').filter(|p| !p.is_empty()).map(|p| p.split_once('=').unwrap_or((p, ""))).map(|(k, v)| (decode(k), decode(v))).collect()
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn read_request(conn: &mut TcpStream) -> Option<Request> {
    let mut buf = Vec::new();
    let mut byte = [0u8; 4096];
    let head_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = conn.read(&mut byte).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&byte[..n]);
    };
    let head = String::from_utf8(buf[..head_end].to_vec()).ok()?;
    let mut lines = head.lines();
    let mut first = lines.next()?.split_whitespace();
    let method = first.next()?.to_string();
    let target = first.next()?.to_string();
    let headers: HashMap<String, String> =
        lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect();
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    let mut body = buf[head_end..].to_vec();
    while body.len() < len {
        let n = conn.read(&mut byte).ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&byte[..n]);
    }
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    Some(Request { method, path: path.to_string(), query: params(query), headers, body })
}

fn reply(conn: &mut TcpStream, status: u16, headers: &[(&str, String)], body: &[u8]) {
    let mut head = format!("HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n", body.len());
    for (k, v) in headers {
        head.push_str(&format!("{k}: {v}\r\n"));
    }
    head.push_str("\r\n");
    let _ = conn.write_all(head.as_bytes());
    let _ = conn.write_all(body);
}

fn json_reply(conn: &mut TcpStream, status: u16, v: Value) {
    reply(conn, status, &[("Content-Type", "application/json".into())], v.to_string().as_bytes());
}

fn error(conn: &mut TcpStream, status: u16, reason: &str, message: &str) {
    json_reply(conn, status, json!({"error": {"code": status, "message": message, "errors": [{"reason": reason}]}}));
}

/// A Drive search query as LightCraft writes it: clauses joined by ` and `.
fn matches(q: &str, f: &File) -> bool {
    fn unquote(s: &str) -> String {
        let s = s.trim().trim_start_matches('\'').trim_end_matches('\'');
        s.replace("\\'", "'").replace("\\\\", "\\")
    }
    q.split(" and ").all(|clause| {
        let clause = clause.trim();
        if let Some(v) = clause.strip_prefix("name = ") {
            f.name == unquote(v)
        } else if let Some(v) = clause.strip_prefix("mimeType = ") {
            f.mime == unquote(v)
        } else if let Some(v) = clause.strip_suffix(" in parents") {
            f.parents.contains(&unquote(v))
        } else {
            clause == "trashed = false"
        }
    })
}

fn multipart(req: &Request) -> (Value, Vec<u8>) {
    let ct = &req.headers["content-type"];
    let boundary = ct.split("boundary=").nth(1).unwrap();
    let sep = format!("--{boundary}\r\n");
    let body = &req.body;
    let find = |from: usize, pat: &[u8]| body[from..].windows(pat.len()).position(|w| w == pat).map(|p| p + from).unwrap();
    let p1 = find(0, sep.as_bytes()) + sep.len();
    let j = find(p1, b"\r\n\r\n") + 4;
    let j_end = find(j, sep.as_bytes());
    let meta: Value = serde_json::from_slice(&body[j..j_end - 2]).unwrap();
    let p2 = j_end + sep.len();
    let d = find(p2, b"\r\n\r\n") + 4;
    let end = find(d, format!("\r\n--{boundary}--").as_bytes());
    (meta, body[d..end].to_vec())
}

fn serve(state: &Shared, mut conn: TcpStream) -> Option<()> {
    let req = read_request(&mut conn)?;
    let mut s = state.lock().unwrap();
    let page = if req.query.contains_key("pageToken") { " (next page)" } else { "" };
    s.log.push(format!("{} {}{page}", req.method, req.path));

    if req.path == "/token" {
        let form = params(std::str::from_utf8(&req.body).ok()?);
        let ok = match form.get("grant_type").map(String::as_str) {
            Some("refresh_token") => form.get("refresh_token").map(String::as_str) == Some(REFRESH),
            Some("authorization_code") => {
                form.get("code").map(String::as_str) == Some("the-code")
                    && s.challenge.as_deref() == form.get("code_verifier").map(|v| pkce_challenge(v)).as_deref()
                    && form.get("redirect_uri").is_some_and(|r| r.starts_with("http://127.0.0.1:"))
            }
            _ => false,
        };
        if !ok || form.get("client_id").map(String::as_str) != Some("cid") {
            json_reply(&mut conn, 400, json!({"error": "invalid_grant", "error_description": "Token has been expired or revoked."}));
            return Some(());
        }
        s.refreshes += 1;
        let token = format!("at-{}", s.refreshes);
        s.tokens.push(token.clone());
        json_reply(&mut conn, 200, json!({"access_token": token, "expires_in": 3599, "refresh_token": REFRESH, "token_type": "Bearer"}));
        return Some(());
    }

    if let Some(sid) = req.path.strip_prefix("/upload/session/") {
        let range = req.headers.get("content-range").cloned().unwrap_or_default();
        if s.flapping {
            s.next += 1;
            let hdr: Vec<(&str, String)> = if s.next.is_multiple_of(2) { vec![("Range", "bytes=0-262143".into())] } else { vec![] };
            reply(&mut conn, 308, &hdr, b"");
            return Some(());
        }
        let fail = s.failing_chunks > 0 && !range.contains('*');
        let Some(sess) = s.sessions.get_mut(sid) else {
            json_reply(&mut conn, 404, json!({}));
            return Some(());
        };
        if !range.contains('*') {
            let (span, _) = range.strip_prefix("bytes ").unwrap().split_once('/').unwrap();
            let (a, _) = span.split_once('-').unwrap();
            let a: usize = a.parse().unwrap();
            assert_eq!(a, sess.data.len(), "chunks are sent from where the server stopped");
            if fail {
                sess.data.extend_from_slice(&req.body[..req.body.len() / 2]);
            } else {
                sess.data.extend_from_slice(&req.body);
            }
        }
        if fail {
            s.failing_chunks -= 1;
            json_reply(&mut conn, 503, json!({}));
            return Some(());
        }
        if range.contains('*') {
            s.asks += 1;
        }
        let sess = s.sessions.get(sid).unwrap();
        if sess.data.len() < sess.total {
            let held = sess.data.len();
            let hdr: Vec<(&str, String)> = if held == 0 { vec![] } else { vec![("Range", format!("bytes=0-{}", held - 1))] };
            reply(&mut conn, 308, &hdr, b"");
            return Some(());
        }
        let sess = s.sessions.remove(sid).unwrap();
        let id = match sess.file {
            Some(id) => {
                s.files.iter_mut().find(|(i, _)| *i == id).unwrap().1.data = sess.data;
                id
            }
            None => s.create(&sess.meta, sess.data),
        };
        json_reply(&mut conn, 200, json!({"id": id}));
        return Some(());
    }

    let token = req.headers.get("authorization").and_then(|a| a.strip_prefix("Bearer ")).unwrap_or_default();
    if !s.tokens.iter().any(|t| t == token) {
        error(&mut conn, 401, "authError", "Invalid Credentials");
        return Some(());
    }
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        if s.rate_limited.is_multiple_of(2) {
            error(&mut conn, 429, "rateLimitExceeded", "Too many requests");
        } else {
            error(&mut conn, 403, "userRateLimitExceeded", "User Rate Limit Exceeded");
        }
        return Some(());
    }

    let resumable = req.query.get("uploadType").map(String::as_str) == Some("resumable");
    let multi = req.query.get("uploadType").map(String::as_str) == Some("multipart");
    match (req.method.as_str(), req.path.as_str()) {
        ("GET", "/drive/v3/about") => {
            let usage: usize = s.files.iter().map(|(_, f)| f.data.len()).sum();
            json_reply(&mut conn, 200, json!({"storageQuota": {"limit": "1000000000", "usage": usage.to_string()}}));
        }
        ("GET", "/drive/v3/files") => {
            let q = req.query.get("q").cloned().unwrap_or_default();
            let all: Vec<&(String, File)> = s.files.iter().filter(|(_, f)| matches(&q, f)).collect();
            let size = req.query.get("pageSize").and_then(|p| p.parse::<usize>().ok()).unwrap_or(100).min(s.page_size);
            let start: usize = req.query.get("pageToken").map(|t| t.parse().unwrap()).unwrap_or(0);
            let page: Vec<Value> =
                all.iter().skip(start).take(size).map(|(id, f)| json!({"id": id, "name": f.name, "size": f.data.len().to_string()})).collect();
            let mut out = json!({ "files": page });
            if start + size < all.len() {
                out["nextPageToken"] = json!((start + size).to_string());
            }
            json_reply(&mut conn, 200, out);
        }
        ("POST", "/drive/v3/files") => {
            let meta: Value = serde_json::from_slice(&req.body).unwrap();
            let id = s.create(&meta, Vec::new());
            json_reply(&mut conn, 200, json!({ "id": id }));
        }
        ("POST", "/upload/drive/v3/files") | ("PATCH", _) if resumable => {
            let file = req.path.strip_prefix("/upload/drive/v3/files/").map(str::to_string);
            if file.as_ref().is_some_and(|f| s.file(f).is_none()) {
                error(&mut conn, 404, "notFound", "File not found");
                return Some(());
            }
            let total = req.headers["x-upload-content-length"].parse().unwrap();
            let sid = s.id();
            s.sessions.insert(sid.clone(), Session { file, meta: serde_json::from_slice(&req.body).unwrap(), total, data: Vec::new() });
            let host = if s.evil_location { "http://198.51.100.7:1".to_string() } else { format!("http://127.0.0.1:{}", s.port) };
            reply(&mut conn, 200, &[("Location", format!("{host}/upload/session/{sid}"))], b"");
        }
        ("POST", "/upload/drive/v3/files") if multi => {
            let (meta, data) = multipart(&req);
            let id = s.create(&meta, data);
            json_reply(&mut conn, 200, json!({ "id": id }));
        }
        ("PATCH", path) if multi => {
            let id = path.strip_prefix("/upload/drive/v3/files/").unwrap().to_string();
            let (_, data) = multipart(&req);
            match s.files.iter_mut().find(|(i, _)| *i == id) {
                Some((_, f)) => {
                    f.data = data;
                    json_reply(&mut conn, 200, json!({ "id": id }));
                }
                None => error(&mut conn, 404, "notFound", "File not found"),
            }
        }
        ("GET", path) if req.query.get("alt").map(String::as_str) == Some("media") => {
            let id = path.strip_prefix("/drive/v3/files/").unwrap();
            match s.file(id) {
                Some(f) => {
                    let data = f.data.clone();
                    reply(&mut conn, 200, &[], &data);
                }
                None => error(&mut conn, 404, "notFound", "File not found"),
            }
        }
        ("DELETE", path) => {
            let id = path.strip_prefix("/drive/v3/files/").unwrap().to_string();
            let before = s.files.len();
            s.files.retain(|(i, _)| *i != id);
            if s.files.len() < before {
                reply(&mut conn, 204, &[], b"");
            } else {
                error(&mut conn, 404, "notFound", "File not found");
            }
        }
        _ => error(&mut conn, 400, "badRequest", "unsupported by the fake"),
    }
    Some(())
}

fn provider(base: &str) -> GoogleDriveProvider {
    let config = GoogleDriveConfig {
        client_id: "cid".into(),
        client_secret: Some("csecret".into()),
        refresh_token: REFRESH.into(),
        folder: "LightCraft".into(),
        api: base.into(),
        token_url: format!("{base}/token"),
    };
    let mut p = GoogleDriveProvider::new(config, "fake drive").unwrap();
    p.retry_base = Duration::from_millis(1);
    p
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

#[test]
fn passes_the_provider_contract() {
    let (state, base) = start();
    let mut p = provider(&base);
    contract::check_provider(&mut p).unwrap();
    let s = state.lock().unwrap();
    let folders: Vec<_> = s.files.iter().filter(|(_, f)| f.mime == FOLDER_MIME).collect();
    assert_eq!(folders.len(), 1, "one app folder, made once");
    assert_eq!((folders[0].1.name.as_str(), folders[0].1.parents.as_slice()), ("LightCraft", ["root".to_string()].as_slice()));
    assert_eq!(s.refreshes, 1, "one access token serves every call");
}

#[test]
fn files_are_named_by_object_name_in_the_app_folder_and_listed_across_pages() {
    let (state, base) = start();
    state.lock().unwrap().page_size = 7;
    let mut p = provider(&base);
    for i in 0..20u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    let listed = p.list().unwrap();
    assert_eq!(listed.len(), 20);
    assert!(listed.windows(2).all(|w| w[0].name < w[1].name));
    let s = state.lock().unwrap();
    let folder = s.files[0].0.clone();
    for l in &listed {
        let f = s.file(&l.remote.key).unwrap();
        assert_eq!(f.name, l.name.as_str());
        assert!(f.name.starts_with("blobs/"));
        assert_eq!(f.parents, std::slice::from_ref(&folder));
    }
    assert_eq!(s.log.iter().filter(|l| *l == "GET /drive/v3/files (next page)").count(), 2, "three pages were read");
    drop(s);
    assert_eq!(p.quota().unwrap().used, 80);
    assert_eq!(p.quota().unwrap().total, Some(1_000_000_000));
}

#[test]
fn large_files_use_a_resumable_upload_that_survives_server_errors() {
    let (state, base) = start();
    let mut p = provider(&base).with_chunk_size(256 * 1024).unwrap();
    let data = bytes(MULTIPART_MAX + 123_457, 1);
    state.lock().unwrap().failing_chunks = 3;
    let (id, remote) = store_blob(&mut p, &data).unwrap();
    assert_eq!(lightcraft_cloud::fetch_blob(&mut p, &remote, &id, data.len() as u64).unwrap(), data);
    {
        let s = state.lock().unwrap();
        assert_eq!(s.failing_chunks, 0);
        assert!(s.sessions.is_empty());
        assert_eq!(s.asks, 3, "after each failed chunk, Google is asked how much it holds");
    }

    // new content for the same name keeps the file id, small (multipart) or large (resumable)
    let name = ObjectName::parse("manifest/snapshot-1.json").unwrap();
    let r1 = p.put(&name, b"one").unwrap();
    let big = bytes(MULTIPART_MAX + 1, 2);
    let r2 = p.put(&name, &big).unwrap();
    let r3 = p.put(&name, b"three").unwrap();
    assert_eq!((&r1, &r1), (&r2, &r3));
    assert_eq!(p.get(&r1, 100).unwrap(), b"three");
    assert_eq!(p.list().unwrap().len(), 2);
}

#[test]
fn an_upload_that_keeps_failing_ends_in_an_error() {
    let (state, base) = start();
    let mut p = provider(&base).with_chunk_size(256 * 1024).unwrap();
    state.lock().unwrap().failing_chunks = 1000;
    let err = store_blob(&mut p, &bytes(MULTIPART_MAX + 1, 3)).unwrap_err();
    assert!(matches!(err, CloudError::Io(_)), "{err:?}");
}

#[test]
fn a_server_that_keeps_moving_the_offset_back_ends_the_upload() {
    let (state, base) = start();
    let mut p = provider(&base).with_chunk_size(256 * 1024).unwrap();
    state.lock().unwrap().flapping = true;
    let err = store_blob(&mut p, &bytes(MULTIPART_MAX + 1, 5)).unwrap_err();
    assert!(matches!(&err, CloudError::Io(m) if m.contains("progress")), "{err:?}");
}

#[test]
fn an_upload_session_on_another_server_is_refused() {
    let (state, base) = start();
    let mut p = provider(&base);
    state.lock().unwrap().evil_location = true;
    let err = store_blob(&mut p, &bytes(MULTIPART_MAX + 1, 4)).unwrap_err();
    assert!(matches!(&err, CloudError::Corrupt(m) if m.contains("another server")), "{err:?}");
    assert!(!state.lock().unwrap().log.iter().any(|l| l.starts_with("PUT")));
}

#[test]
fn expired_tokens_are_refreshed_and_rate_limits_retried() {
    let (state, base) = start();
    let mut p = provider(&base);
    p.quota().unwrap();
    state.lock().unwrap().tokens.clear(); // revoked or expired early
    p.quota().unwrap();
    assert_eq!(state.lock().unwrap().refreshes, 2);

    state.lock().unwrap().rate_limited = 4;
    store_blob(&mut p, b"after the rate limit").unwrap();
    assert_eq!(state.lock().unwrap().rate_limited, 0);

    state.lock().unwrap().rate_limited = 100;
    assert!(matches!(p.quota(), Err(CloudError::Denied(_) | CloudError::Io(_))), "gives up after a few retries");
}

#[test]
fn a_revoked_refresh_token_asks_to_sign_in_again() {
    let (_state, base) = start();
    let mut config = GoogleDriveConfig::new(&OAuthClient::google_drive("cid", None), "revoked");
    config.api = base.clone();
    config.token_url = format!("{base}/token");
    let mut p = GoogleDriveProvider::new(config, "t").unwrap();
    match p.list() {
        Err(CloudError::Denied(m)) => assert!(m.starts_with("sign in again: invalid_grant"), "{m}"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn duplicate_names_resolve_to_the_oldest_file_and_hostile_entries_are_skipped() {
    let (state, base) = start();
    let mut p = provider(&base);
    let folder = p.ensure_root().unwrap();
    {
        let mut s = state.lock().unwrap();
        let file = |name: &str, data: &[u8]| File { name: name.into(), parents: vec![folder.clone()], mime: "x".into(), data: data.to_vec() };
        s.files.push(("old".into(), file("manifest/a", b"old")));
        s.files.push(("new".into(), file("manifest/a", b"new")));
        s.files.push(("bad".into(), file("../../etc/passwd", b"x")));
        s.files.push(("bad_name".into(), file(".hidden", b"x")));
        s.files.push(("evil/id".into(), file("manifest/b", b"x")));
    }
    let listed = p.list().unwrap();
    assert_eq!(listed.len(), 1, "{listed:?}");
    assert_eq!(listed[0].remote, RemoteRef { key: "old".into() });
    let r = p.put(&ObjectName::parse("manifest/a").unwrap(), b"updated").unwrap();
    assert_eq!(r.key, "old");
    assert!(matches!(p.get(&RemoteRef { key: "../x".into() }, 10), Err(CloudError::Corrupt(_))));
}

#[test]
fn sign_in_with_pkce_and_a_loopback_redirect() {
    let (state, base) = start();
    let mut client = OAuthClient::google_drive("cid", Some("csecret".into()));
    client.token_url = format!("{base}/token");
    let flow = LoopbackFlow::start(client, &mut OsRandom).unwrap();
    let url = flow.authorize_url().to_string();
    let query = params(url.split_once('?').unwrap().1);
    state.lock().unwrap().challenge = Some(query["code_challenge"].clone());
    let redirect = flow.redirect_uri().to_string();
    assert_eq!(query["redirect_uri"], redirect);
    let waiter = std::thread::spawn(move || flow.wait(&Net::default(), Duration::from_secs(20)));

    // what the browser (or another program) sends to the loopback port
    let addr = redirect.strip_prefix("http://").unwrap().to_string();
    let get = |target: &str| {
        let mut c = TcpStream::connect(&addr).unwrap();
        c.write_all(format!("GET {target} HTTP/1.1\r\nHost: {addr}\r\n\r\n").as_bytes()).unwrap();
        let mut out = String::new();
        c.read_to_string(&mut out).unwrap();
        out
    };
    assert!(get("/favicon.ico").starts_with("HTTP/1.1 404"));
    assert!(get("/?state=forged&code=the-code").starts_with("HTTP/1.1 404"), "a wrong state is ignored");
    let page = get(&format!("/?state={}&code=the-code&scope=x", query["state"]));
    assert!(page.starts_with("HTTP/1.1 200") && page.contains("Signed in"), "{page}");

    let tokens = waiter.join().unwrap().unwrap();
    assert_eq!(tokens.refresh_token.as_deref(), Some(REFRESH));
    // the account then works with the refresh token it got
    let mut config = GoogleDriveConfig::new(&OAuthClient::google_drive("cid", Some("csecret".into())), tokens.refresh_token.unwrap());
    config.api = base.clone();
    config.token_url = format!("{base}/token");
    contract::check_provider(&mut GoogleDriveProvider::new(config, "signed in").unwrap()).unwrap();
}

#[test]
fn sign_in_can_be_refused_cancelled_or_time_out() {
    let client = OAuthClient::google_drive("cid", None);
    let flow = LoopbackFlow::start(client.clone(), &mut OsRandom).unwrap();
    let state = params(flow.authorize_url().split_once('?').unwrap().1)["state"].clone();
    let addr = flow.redirect_uri().strip_prefix("http://").unwrap().to_string();
    let waiter = std::thread::spawn(move || flow.wait(&Net::default(), Duration::from_secs(20)));
    let mut c = TcpStream::connect(&addr).unwrap();
    c.write_all(format!("GET /?state={state}&error=access_denied HTTP/1.1\r\n\r\n").as_bytes()).unwrap();
    let mut page = String::new();
    c.read_to_string(&mut page).unwrap();
    assert!(page.contains("cancelled"));
    assert!(matches!(waiter.join().unwrap(), Err(CloudError::Denied(m)) if m.contains("access_denied")));

    let flow = LoopbackFlow::start(client.clone(), &mut OsRandom).unwrap();
    let net = Net::default();
    net.cancel.store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(matches!(flow.wait(&net, Duration::from_secs(20)), Err(CloudError::Io(m)) if m.contains("cancelled")));

    let flow = LoopbackFlow::start(client, &mut OsRandom).unwrap();
    assert!(matches!(flow.wait(&Net::default(), Duration::from_millis(100)), Err(CloudError::Io(m)) if m.contains("timed out")));
}

#[test]
fn a_hostile_token_endpoint_never_crashes() {
    // replies the fake can't produce: huge expiry, an empty token, junk
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let replies: Vec<&[u8]> =
        vec![br#"{"access_token":"x","expires_in":18446744073709551615}"#, br#"{"access_token":""}"#, b"not json", br#"{"error":"server_error"}"#];
    let n = replies.len();
    std::thread::spawn(move || {
        let mut replies = replies.into_iter().cycle();
        for mut conn in listener.incoming().flatten() {
            let Some(req) = read_request(&mut conn) else { continue };
            if req.path == "/token" {
                let body = replies.next().unwrap();
                reply(&mut conn, if body.starts_with(br#"{"error""#) { 500 } else { 200 }, &[], body);
            } else {
                // a valid token was handed out: answer like an empty Drive
                json_reply(&mut conn, 200, json!({"storageQuota": {}}));
            }
        }
    });
    for _ in 0..n {
        let mut config = GoogleDriveConfig::new(&OAuthClient::google_drive("cid", None), "r");
        config.api = base.clone();
        config.token_url = format!("{base}/token");
        let _ = GoogleDriveProvider::new(config, "t").unwrap().quota();
    }
}
