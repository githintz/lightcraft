//! The Dropbox provider against a fake Dropbox API v2 on `127.0.0.1` (RPC and content
//! endpoints, upload sessions with `incorrect_offset`, cursored listings, the token endpoint),
//! with fault injection. Offline.

#![cfg(not(target_arch = "wasm32"))]

use std::collections::{BTreeMap, HashMap};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod common;
use common::{REFRESH, Request, bearer, json_reply, read_request, reply, token_endpoint};
use lightcraft_cloud::dropbox::{DropboxConfig, DropboxProvider, SIMPLE_MAX};
use lightcraft_cloud::{CloudError, ObjectName, Provider, Quota, contract, fetch_blob, store_blob};
use serde_json::{Value, json};

#[derive(Default)]
struct State {
    /// Lower-cased path → bytes.
    files: BTreeMap<String, Vec<u8>>,
    sessions: HashMap<String, Vec<u8>>,
    next: u64,
    tokens: Vec<String>,
    page_size: usize,
    rate_limited: u32,
    /// The next N appends are stored but their answer is lost (503).
    lost_answers: u32,
    offset_errors: u32,
}

type Shared = Arc<Mutex<State>>;

fn start() -> (Shared, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: Shared = Arc::new(Mutex::new(State { page_size: 2000, ..State::default() }));
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

fn conflict(conn: &mut TcpStream, summary: &str, error: Value) {
    json_reply(conn, 409, json!({"error_summary": summary, "error": error}));
}

fn serve(state: &Shared, conn: &mut TcpStream, req: &Request) {
    let mut s = state.lock().unwrap();
    if req.path == "/oauth2/token" {
        let mut tokens = std::mem::take(&mut s.tokens);
        token_endpoint(conn, req, "cid", &mut tokens, false);
        s.tokens = tokens;
        return;
    }
    if !bearer(req).is_some_and(|t| s.tokens.iter().any(|x| x == t)) {
        return json_reply(conn, 401, json!({"error_summary": "expired_access_token/", "error": {".tag": "expired_access_token"}}));
    }
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        return reply(conn, 429, &[("Retry-After", "0".into())], b"too_many_requests");
    }
    assert_eq!(req.method, "POST", "every Dropbox call is a POST");
    let arg: Value = match req.headers.get("dropbox-api-arg") {
        Some(a) => {
            assert!(a.is_ascii());
            serde_json::from_str(a).unwrap()
        }
        None if req.body == b"null" => Value::Null,
        None => serde_json::from_slice(&req.body).unwrap_or(Value::Null),
    };
    let path = arg["path"].as_str().unwrap_or_default().to_ascii_lowercase();
    match req.path.as_str() {
        "/2/users/get_space_usage" => {
            assert_eq!(req.body, b"null");
            let used: usize = s.files.values().map(Vec::len).sum();
            json_reply(conn, 200, json!({"used": used, "allocation": {".tag": "individual", "allocated": 2_000_000_000u64}}));
        }
        "/2/files/upload" => {
            assert_eq!(arg["mode"], "overwrite");
            s.files.insert(path.clone(), req.body.clone());
            json_reply(conn, 200, json!({"path_lower": path, "size": req.body.len()}));
        }
        "/2/files/download" => match s.files.get(&path).cloned() {
            Some(d) => reply(conn, 200, &[("Dropbox-API-Result", json!({"size": d.len()}).to_string())], &d),
            None => conflict(conn, "path/not_found/..", json!({".tag": "path", "path": {".tag": "not_found"}})),
        },
        "/2/files/delete_v2" => {
            if s.files.remove(&path).is_some() {
                json_reply(conn, 200, json!({"metadata": {"path_lower": path}}));
            } else {
                conflict(conn, "path_lookup/not_found/", json!({".tag": "path_lookup", "path_lookup": {".tag": "not_found"}}));
            }
        }
        "/2/files/upload_session/start" => {
            s.next += 1;
            let id = format!("sess{}", s.next);
            s.sessions.insert(id.clone(), req.body.clone());
            json_reply(conn, 200, json!({ "session_id": id }));
        }
        "/2/files/upload_session/append_v2" | "/2/files/upload_session/finish" => {
            let id = arg["cursor"]["session_id"].as_str().unwrap().to_string();
            let offset = arg["cursor"]["offset"].as_u64().unwrap() as usize;
            let finish = req.path.ends_with("finish");
            let lose = !finish && s.lost_answers > 0;
            let Some(data) = s.sessions.get_mut(&id) else {
                return conflict(conn, "lookup_failed/not_found/", json!({".tag": "lookup_failed", "lookup_failed": {".tag": "not_found"}}));
            };
            if offset != data.len() {
                let held = data.len();
                s.offset_errors += 1;
                let e = json!({".tag": "incorrect_offset", "correct_offset": held});
                return if finish {
                    conflict(conn, "lookup_failed/incorrect_offset/", json!({".tag": "lookup_failed", "lookup_failed": e}))
                } else {
                    conflict(conn, "incorrect_offset/", e)
                };
            }
            data.extend_from_slice(&req.body);
            if lose {
                s.lost_answers -= 1;
                return reply(conn, 503, &[], b"");
            }
            if finish {
                assert_eq!(arg["commit"]["mode"], "overwrite");
                let commit = arg["commit"]["path"].as_str().unwrap().to_ascii_lowercase();
                let data = s.sessions.remove(&id).unwrap();
                let size = data.len();
                s.files.insert(commit.clone(), data);
                return json_reply(conn, 200, json!({"path_lower": commit, "size": size}));
            }
            json_reply(conn, 200, Value::Null);
        }
        "/2/files/list_folder" | "/2/files/list_folder/continue" => {
            let (root, start) = if req.path.ends_with("continue") {
                let c = arg["cursor"].as_str().unwrap();
                let (root, start) = c.split_once('|').unwrap();
                (root.to_string(), start.parse::<usize>().unwrap())
            } else {
                assert_eq!(arg["recursive"], true);
                (path.clone(), 0)
            };
            let prefix = format!("{root}/");
            let entries: Vec<Value> = s
                .files
                .iter()
                .filter(|(p, _)| p.starts_with(&prefix))
                .map(|(p, d)| json!({".tag": "file", "path_lower": p, "path_display": p, "size": d.len()}))
                .collect();
            if entries.is_empty() && !root.is_empty() {
                return conflict(conn, "path/not_found/", json!({".tag": "path", "path": {".tag": "not_found"}}));
            }
            let mut all = vec![json!({".tag": "folder", "path_lower": format!("{root}/blobs")})];
            all.extend(entries);
            let n = s.page_size;
            let page: Vec<Value> = all.iter().skip(start).take(n).cloned().collect();
            let more = start + n < all.len();
            json_reply(conn, 200, json!({"entries": page, "cursor": format!("{root}|{}", start + n), "has_more": more}));
        }
        other => panic!("unexpected call {other}"),
    }
}

fn provider(base: &str, root: &str) -> DropboxProvider {
    let config = DropboxConfig {
        client_id: "cid".into(),
        refresh_token: REFRESH.into(),
        root: root.into(),
        api: base.into(),
        content: base.into(),
        token_url: format!("{base}/oauth2/token"),
    };
    let mut p = DropboxProvider::new(config, "fake dropbox").unwrap();
    p.retry_base = Duration::from_millis(1);
    p
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

#[test]
fn passes_the_provider_contract_in_the_app_folder_and_in_a_subfolder() {
    for root in ["", "/LightCraft"] {
        let (state, base) = start();
        let mut p = provider(&base, root);
        contract::check_provider(&mut p).unwrap();
        assert_eq!(state.lock().unwrap().tokens.len(), 1);
    }
}

#[test]
fn objects_live_under_the_root_and_are_listed_across_pages() {
    let (state, base) = start();
    state.lock().unwrap().page_size = 7;
    let mut p = provider(&base, "/LightCraft");
    for i in 0..20u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    {
        let mut s = state.lock().unwrap();
        assert!(s.files.keys().all(|k| k.starts_with("/lightcraft/blobs/")));
        // things the provider must skip: another app's root, a name that isn't ours
        s.files.insert("/other/blobs/x".into(), vec![1]);
        s.files.insert("/lightcraft/.dropbox.cache".into(), vec![1]);
    }
    assert_eq!(p.list().unwrap().len(), 20);
    assert_eq!(p.quota().unwrap(), Quota { total: Some(2_000_000_000), used: 82 });
}

#[test]
fn large_files_use_an_upload_session_that_survives_lost_answers() {
    let (state, base) = start();
    let mut p = provider(&base, "").with_chunk_size(4 * 1024 * 1024).unwrap();
    state.lock().unwrap().lost_answers = 2;
    let data = bytes(SIMPLE_MAX + 5 * 1024 * 1024 + 3, 1);
    let (id, remote) = store_blob(&mut p, &data).unwrap();
    assert_eq!(fetch_blob(&mut p, &remote, &id, data.len() as u64).unwrap(), data);
    let s = state.lock().unwrap();
    assert_eq!((s.lost_answers, s.offset_errors), (0, 2), "each resent chunk was answered with where to go on");
    assert!(s.sessions.is_empty());
    drop(s);
    // replacing it keeps one file
    let name = ObjectName::blob(&id);
    p.put(&name, b"small now").unwrap();
    assert_eq!(p.list().unwrap().len(), 1);
}

#[test]
fn expired_tokens_and_rate_limits() {
    let (state, base) = start();
    let mut p = provider(&base, "");
    p.quota().unwrap();
    state.lock().unwrap().tokens.clear();
    p.quota().unwrap();
    state.lock().unwrap().rate_limited = 4;
    store_blob(&mut p, b"after the limit").unwrap();
    state.lock().unwrap().rate_limited = 100;
    assert!(matches!(p.quota(), Err(CloudError::Io(_))));
}

#[test]
fn a_revoked_refresh_token_asks_to_sign_in_again() {
    let (_state, base) = start();
    let config = DropboxConfig {
        client_id: "cid".into(),
        refresh_token: "revoked".into(),
        root: String::new(),
        api: base.clone(),
        content: base.clone(),
        token_url: format!("{base}/oauth2/token"),
    };
    let mut p = DropboxProvider::new(config, "t").unwrap();
    assert!(matches!(p.list(), Err(CloudError::Denied(m)) if m.starts_with("sign in again")));
}
