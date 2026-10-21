//! The OneDrive provider against a fake Microsoft Graph on `127.0.0.1`: the app folder addressed
//! by path, upload sessions, download redirects, paged children listings and the token endpoint
//! (which rotates refresh tokens, as Microsoft does), with fault injection. Offline.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{REFRESH, Request, bearer, json_reply, read_request, reply, token_endpoint};
use lightcraft_cloud::onedrive::{OneDriveConfig, OneDriveProvider, SIMPLE_MAX};
use lightcraft_cloud::{CloudError, ObjectName, Provider, RemoteRef, contract, fetch_blob, store_blob};
use serde_json::json;

const ROOT: &str = "/v1.0/me/drive/special/approot";

#[derive(Default)]
struct State {
    port: u16,
    files: BTreeMap<String, Vec<u8>>,
    sessions: HashMap<String, (String, usize, Vec<u8>)>,
    next: u64,
    tokens: Vec<String>,
    page_size: usize,
    rate_limited: u32,
    failing_chunks: u32,
    /// Credentials sent to an upload or download address (must never happen).
    leaked: u32,
    evil_next_link: bool,
    evil_download: bool,
    status_queries: u32,
}

type Shared = Arc<Mutex<State>>;

fn start() -> (Shared, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: Shared = Arc::new(Mutex::new(State { port, page_size: 1000, ..State::default() }));
    let s = state.clone();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            if let Some(req) = read_request(&mut conn) {
                serve(&s, &mut conn, &req);
            }
        }
    });
    (state, format!("http://127.0.0.1:{port}"))
}

fn error(conn: &mut TcpStream, status: u16, code: &str) {
    json_reply(conn, status, json!({"error": {"code": code, "message": code}}));
}

fn serve(state: &Shared, conn: &mut TcpStream, req: &Request) {
    let mut s = state.lock().unwrap();
    let base = format!("http://127.0.0.1:{}", s.port);
    if req.path == "/token" {
        let mut tokens = std::mem::take(&mut s.tokens);
        token_endpoint(conn, req, "cid", &mut tokens, true);
        s.tokens = tokens;
        return;
    }
    if let Some(sid) = req.path.strip_prefix("/upload/") {
        if req.headers.contains_key("authorization") {
            s.leaked += 1;
        }
        let fail = s.failing_chunks > 0 && req.method == "PUT";
        let Some((path, total, data)) = s.sessions.get_mut(sid) else { return error(conn, 404, "itemNotFound") };
        if let Some(t) = req.headers.get("content-range").and_then(|r| r.rsplit('/').next()).and_then(|t| t.parse().ok()) {
            *total = t; // Graph learns the size from the chunks' Content-Range
        }
        let total = *total;
        if req.method == "GET" {
            let held = data.len();
            s.status_queries += 1;
            return json_reply(conn, 200, json!({"nextExpectedRanges": [format!("{held}-")]}));
        }
        let range = req.headers["content-range"].strip_prefix("bytes ").unwrap().to_string();
        let first: usize = range.split('-').next().unwrap().parse().unwrap();
        if first != data.len() {
            return error(conn, 416, "invalidRange");
        }
        if fail {
            data.extend_from_slice(&req.body[..req.body.len() / 2]);
            s.failing_chunks -= 1;
            return error(conn, 503, "serviceNotAvailable");
        }
        data.extend_from_slice(&req.body);
        if data.len() < total {
            let held = data.len();
            return json_reply(conn, 202, json!({"nextExpectedRanges": [format!("{held}-")]}));
        }
        let (path, data) = (path.clone(), std::mem::take(data));
        s.sessions.remove(sid);
        let size = data.len();
        s.files.insert(path.clone(), data);
        return json_reply(conn, 201, json!({"id": "x", "name": path, "size": size}));
    }
    if let Some(path) = req.path.strip_prefix("/download/") {
        if req.headers.contains_key("authorization") {
            s.leaked += 1;
        }
        let data = s.files.get(path).cloned();
        return match data {
            Some(d) => reply(conn, 200, &[], &d),
            None => error(conn, 404, "itemNotFound"),
        };
    }

    if !bearer(req).is_some_and(|t| s.tokens.iter().any(|x| x == t)) {
        return error(conn, 401, "InvalidAuthenticationToken");
    }
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        return reply(conn, 429, &[("Retry-After", "0".into())], b"");
    }
    if req.path == "/v1.0/me/drive" {
        let used: usize = s.files.values().map(Vec::len).sum();
        return json_reply(
            conn,
            200,
            json!({"id": "d", "quota": {"total": 5_000_000_000u64, "used": used, "remaining": 5_000_000_000u64 - used as u64}}),
        );
    }
    let Some(rest) = req.path.strip_prefix(ROOT) else { return error(conn, 400, "invalidRequest") };
    let (path, action) = match rest.strip_prefix(":/") {
        Some(inner) => match inner.split_once(":/") {
            Some((p, a)) => (p.to_string(), a.to_string()),
            None => (inner.to_string(), String::new()),
        },
        None => (String::new(), rest.trim_start_matches('/').to_string()),
    };
    match (req.method.as_str(), action.as_str()) {
        ("PUT", "content") => {
            assert_eq!(req.query.get("@microsoft.graph.conflictBehavior").map(String::as_str), Some("replace"));
            s.files.insert(path.clone(), req.body.clone());
            json_reply(conn, 201, json!({"id": "x", "name": path, "size": req.body.len()}));
        }
        ("GET", "content") => {
            if !s.files.contains_key(&path) {
                return error(conn, 404, "itemNotFound");
            }
            let host = if s.evil_download { "http://198.51.100.7".to_string() } else { base };
            reply(conn, 302, &[("Location", format!("{host}/download/{path}"))], b"");
        }
        ("POST", "createUploadSession") => {
            s.next += 1;
            let sid = format!("s{}", s.next);
            s.sessions.insert(sid.clone(), (path, 0, Vec::new()));
            json_reply(conn, 200, json!({"uploadUrl": format!("{base}/upload/{sid}"), "expirationDateTime": "2099-01-01T00:00:00Z"}));
        }
        ("GET", "children") => {
            let prefix = if path.is_empty() { String::new() } else { format!("{path}/") };
            let mut entries: Vec<serde_json::Value> = Vec::new();
            let mut folders = BTreeSet::new();
            for (p, d) in &s.files {
                let Some(rel) = p.strip_prefix(&prefix) else { continue };
                match rel.split_once('/') {
                    Some((dir, _)) => {
                        folders.insert(dir.to_string());
                    }
                    None => entries.push(json!({"name": rel, "size": d.len(), "file": {}})),
                }
            }
            entries.extend(folders.into_iter().map(|f| json!({"name": f, "size": 0, "folder": {"childCount": 1}})));
            let top = req.query.get("$top").and_then(|t| t.parse::<usize>().ok()).unwrap_or(200).min(s.page_size);
            let skip: usize = req.query.get("$skiptoken").map(|t| t.parse().unwrap()).unwrap_or(0);
            let page: Vec<_> = entries.iter().skip(skip).take(top).cloned().collect();
            let mut out = json!({ "value": page });
            if skip + top < entries.len() {
                let host = if s.evil_next_link { "https://198.51.100.7".to_string() } else { base };
                out["@odata.nextLink"] = json!(format!("{host}{}?$top={top}&$skiptoken={}", req.path, skip + top));
            }
            json_reply(conn, 200, out);
        }
        ("DELETE", "") => {
            let before = s.files.len();
            s.files.retain(|p, _| *p != path && !p.starts_with(&format!("{path}/")));
            if s.files.len() < before { reply(conn, 204, &[], b"") } else { error(conn, 404, "itemNotFound") }
        }
        _ => error(conn, 400, "invalidRequest"),
    }
}

fn provider(base: &str) -> OneDriveProvider {
    let config =
        OneDriveConfig { client_id: "cid".into(), refresh_token: REFRESH.into(), api: format!("{base}/v1.0"), token_url: format!("{base}/token") };
    let mut p = OneDriveProvider::new(config, "fake onedrive").unwrap();
    p.retry_base = Duration::from_millis(1);
    p
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

#[test]
fn passes_the_provider_contract_without_leaking_credentials() {
    let (state, base) = start();
    let mut p = provider(&base);
    contract::check_provider(&mut p).unwrap();
    let s = state.lock().unwrap();
    assert_eq!(s.leaked, 0, "download addresses get no Authorization header");
    assert_eq!(s.tokens.len(), 1);
    drop(s);
    assert!(p.refresh_token().starts_with("rotated-"), "a rotated refresh token is kept for the vault");
}

#[test]
fn nested_folders_are_listed_across_pages() {
    let (state, base) = start();
    state.lock().unwrap().page_size = 3;
    let mut p = provider(&base);
    for i in 0..20u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    p.put(&ObjectName::parse("manifest/snapshot-1.json").unwrap(), b"{}").unwrap();
    let listed = p.list().unwrap();
    assert_eq!(listed.len(), 21);
    assert!(listed.iter().all(|l| l.remote.key == l.name.as_str()));
    assert_eq!(p.quota().unwrap(), lightcraft_cloud::Quota { total: Some(5_000_000_000), used: 82 });
}

#[test]
fn large_files_use_an_upload_session_that_survives_server_errors() {
    let (state, base) = start();
    let mut p = provider(&base).with_chunk_size(320 * 1024).unwrap();
    state.lock().unwrap().failing_chunks = 3;
    let data = bytes(SIMPLE_MAX + 777_777, 1);
    let (id, remote) = store_blob(&mut p, &data).unwrap();
    assert_eq!(fetch_blob(&mut p, &remote, &id, data.len() as u64).unwrap(), data);
    let s = state.lock().unwrap();
    assert_eq!((s.failing_chunks, s.status_queries, s.leaked), (0, 3, 0));
    assert!(s.sessions.is_empty());
}

#[test]
fn expired_tokens_and_rate_limits() {
    let (state, base) = start();
    let mut p = provider(&base);
    p.quota().unwrap();
    state.lock().unwrap().tokens.clear();
    p.quota().unwrap();
    state.lock().unwrap().rate_limited = 3;
    p.quota().unwrap();
    state.lock().unwrap().rate_limited = 100;
    assert!(matches!(p.quota(), Err(CloudError::Io(_))));
}

#[test]
fn foreign_addresses_are_refused() {
    let (state, base) = start();
    let mut p = provider(&base);
    let (_, remote) = store_blob(&mut p, b"x").unwrap();
    state.lock().unwrap().evil_download = true;
    assert!(matches!(p.get(&remote, 10), Err(CloudError::Corrupt(m)) if m.contains("https")));

    state.lock().unwrap().page_size = 1;
    store_blob(&mut p, b"y").unwrap();
    state.lock().unwrap().evil_next_link = true;
    assert!(matches!(p.list(), Err(CloudError::Corrupt(_))));
    assert!(matches!(p.get(&RemoteRef { key: "../../me/drive".into() }, 10), Err(CloudError::InvalidName(_))));
}

#[test]
fn a_revoked_refresh_token_asks_to_sign_in_again() {
    let (_state, base) = start();
    let config =
        OneDriveConfig { client_id: "cid".into(), refresh_token: "revoked".into(), api: format!("{base}/v1.0"), token_url: format!("{base}/token") };
    let mut p = OneDriveProvider::new(config, "t").unwrap();
    assert!(matches!(p.list(), Err(CloudError::Denied(m)) if m.starts_with("sign in again")));
}
