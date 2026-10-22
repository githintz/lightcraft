//! A fake OpenDrive API v1 on `127.0.0.1`: the password and refresh grants with rotating
//! refresh tokens, folders by id and by path, create / open / chunk / close uploads checked by
//! MD5, rename, delete, listings and user info, with fault injection. Shared by the provider's
//! tests and the engine's (`crates/engine/src/tests_cloud.rs`).

use std::collections::BTreeMap;
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use md5::{Digest, Md5};
use serde_json::{Value, json};

use super::{Request, json_reply, params, read_request, reply};

pub const USER: &str = "alice@example.com";
pub const PASS: &str = "hunter2";

pub struct File {
    pub folder: String,
    pub name: String,
    pub data: Vec<u8>,
}

#[derive(Default)]
pub struct State {
    /// Folder id → (parent id, name); "0" is the account's root.
    pub folders: BTreeMap<String, (String, String)>,
    pub files: BTreeMap<String, File>,
    /// Upload temp location → bytes so far.
    pub uploads: BTreeMap<String, Vec<u8>>,
    pub next: u64,
    pub access: Vec<String>,
    /// The one refresh token that works (each refresh replaces it).
    pub refresh: String,
    pub refreshes: u32,
    pub rate_limited: u32,
    /// The next N chunks are stored but their answer is lost (503).
    pub lost_answers: u32,
    pub offset_errors: u32,
    /// The next N chunks are refused with 423 (another chunk being written).
    pub locked: u32,
    /// The next N chunks fail (503) and are not stored.
    pub broken_chunks: u32,
    pub chunks: u32,
    /// `folder_is_public` of every folder made.
    pub made_public: Vec<i64>,
    /// Content (MD5) OpenDrive already holds: `create_file` answers RequireHashOnly.
    pub dedup: bool,
}

pub type Shared = Arc<Mutex<State>>;

pub fn start() -> (Shared, String) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let state: Shared = Arc::new(Mutex::new(State::default()));
    let s = state.clone();
    std::thread::spawn(move || {
        for mut conn in listener.incoming().flatten() {
            if let Some(req) = read_request(&mut conn) {
                serve(&s, &mut conn, &req);
            }
        }
    });
    (state, format!("http://127.0.0.1:{port}/api/v1"))
}

fn error(conn: &mut TcpStream, code: u16, message: &str) {
    json_reply(conn, code, json!({"error": {"code": code, "message": message}}));
}

fn md5(data: &[u8]) -> String {
    Md5::digest(data).iter().map(|b| format!("{b:02x}")).collect()
}

impl State {
    fn id(&mut self) -> String {
        self.next += 1;
        format!("Id{}", self.next)
    }

    fn tokens(&mut self) -> Value {
        let access = format!("at-{}", self.access.len() + 1);
        self.access.push(access.clone());
        self.refreshes += 1;
        self.refresh = format!("rt-{}", self.refreshes);
        json!({"access_token": access, "expires_in": 86400, "token_type": "bearer", "scope": null, "refresh_token": self.refresh})
    }

    pub fn folder_by_path(&self, path: &str) -> Option<String> {
        let mut id = "0".to_string();
        for name in path.trim_start_matches('/').split('/') {
            id = self.folders.iter().find(|(_, (p, n))| *p == id && n == name).map(|(k, _)| k.clone())?;
        }
        Some(id)
    }

    pub fn file_in(&self, folder: &str, name: &str) -> Option<String> {
        self.files.iter().find(|(_, f)| f.folder == folder && f.name == name).map(|(k, _)| k.clone())
    }

    fn file_json(id: &str, f: &File) -> Value {
        json!({"FileId": id, "Name": f.name, "Size": f.data.len().to_string(), "Access": "0"})
    }
}

/// The `file_data` part of a multipart body.
fn file_data(req: &Request) -> Vec<u8> {
    let boundary = req.headers["content-type"].split("boundary=").nth(1).unwrap().to_string();
    let body = &req.body;
    let start = body.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
    let head = String::from_utf8_lossy(&body[..start]);
    assert!(head.contains("name=\"file_data\""), "{head}");
    let end_marker = format!("\r\n--{boundary}--\r\n");
    assert!(body.ends_with(end_marker.as_bytes()));
    body[start..body.len() - end_marker.len()].to_vec()
}

fn serve(state: &Shared, conn: &mut TcpStream, req: &Request) {
    let mut s = state.lock().unwrap();
    let path = req.path.strip_prefix("/api/v1").expect("every call is under the API root").to_string();
    if path == "/oauth2/grant.json" {
        let form = params(std::str::from_utf8(&req.body).unwrap_or_default());
        let get = |k: &str| form.get(k).map(String::as_str).unwrap_or_default();
        assert_eq!(get("client_id"), "OpenDrive");
        let ok = match get("grant_type") {
            "password" => get("username") == USER && get("password") == PASS,
            "refresh_token" => !s.refresh.is_empty() && get("refresh_token") == s.refresh,
            _ => false,
        };
        if !ok {
            let status = if get("grant_type") == "password" { 401 } else { 400 };
            return json_reply(conn, status, json!({"error": "invalid_grant", "error_description": "Invalid refresh token"}));
        }
        let t = s.tokens();
        return json_reply(conn, 200, t);
    }
    let token = req.query.get("access_token").cloned().unwrap_or_default();
    if !s.access.contains(&token) {
        return error(conn, 401, "Session does not exist, please re-login.");
    }
    if s.rate_limited > 0 {
        s.rate_limited -= 1;
        return reply(conn, 429, &[("Retry-After", "0".into())], b"slow down");
    }
    let body: Value = if req.headers.get("content-type").is_some_and(|c| c.starts_with("application/json")) {
        let v: Value = serde_json::from_slice(&req.body).unwrap();
        assert_eq!(v["session_id"], "OAUTH", "{path}");
        v
    } else {
        Value::Null
    };
    let str_of = |v: &Value| v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string());
    let seg: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    match (req.method.as_str(), seg.as_slice()) {
        ("GET", ["users", "info.json", "OAUTH"]) => {
            let used: usize = s.files.values().map(|f| f.data.len()).sum();
            json_reply(conn, 200, json!({"UserName": USER, "MaxStorage": "1024", "StorageUsed": used.to_string()}))
        }
        ("POST", ["folder", "idbypath.json"]) => match s.folder_by_path(body["path"].as_str().unwrap()) {
            Some(id) => json_reply(conn, 200, json!({"FolderId": id})),
            None => error(conn, 404, "Folder not found."),
        },
        ("POST", ["folder.json"]) => {
            let (parent, name) = (str_of(&body["folder_sub_parent"]), body["folder_name"].as_str().unwrap().to_string());
            assert!(parent == "0" || s.folders.contains_key(&parent));
            assert!(!name.contains('/'));
            if s.folders.values().any(|(p, n)| *p == parent && *n == name) || s.file_in(&parent, &name).is_some() {
                return error(conn, 409, "File or folder with such name already exists.");
            }
            s.made_public.push(body["folder_is_public"].as_i64().unwrap_or(2));
            let id = s.id();
            s.folders.insert(id.clone(), (parent, name.clone()));
            json_reply(conn, 200, json!({"FolderID": id, "Name": name}))
        }
        ("GET", ["folder", "itembyname.json", "OAUTH", folder]) => {
            let name = req.query.get("name").unwrap();
            let files: Vec<Value> =
                s.files.iter().filter(|(_, f)| f.folder == *folder && &f.name == name).map(|(k, f)| State::file_json(k, f)).collect();
            let mut v = json!({"DirUpdateTime": "1"});
            if !files.is_empty() {
                v["Files"] = json!(files);
            }
            json_reply(conn, 200, v)
        }
        ("GET", ["folder", "list.json", "OAUTH", folder]) => {
            if *folder != "0" && !s.folders.contains_key(*folder) {
                return error(conn, 404, "Directory does not exist.");
            }
            let folders: Vec<Value> =
                s.folders.iter().filter(|(_, (p, _))| p == folder).map(|(k, (_, n))| json!({"FolderID": k, "Name": n})).collect();
            let files: Vec<Value> = s.files.iter().filter(|(_, f)| f.folder == *folder).map(|(k, f)| State::file_json(k, f)).collect();
            json_reply(conn, 200, json!({"Folders": folders, "Files": files}))
        }
        ("POST", ["file", "idbypath.json"]) => {
            let p = body["path"].as_str().unwrap();
            assert!(!p.starts_with('/'));
            let (dir, name) = p.rsplit_once('/').unwrap();
            match s.folder_by_path(dir).and_then(|f| s.file_in(&f, name)) {
                Some(id) => json_reply(conn, 200, json!({"FileId": id})),
                None => error(conn, 404, "File not found."),
            }
        }
        ("POST", ["upload", "create_file.json"]) => {
            let (folder, name) = (str_of(&body["folder_id"]), body["file_name"].as_str().unwrap().to_string());
            assert!(s.folders.contains_key(&folder));
            assert_eq!(body["open_if_exists"], 0);
            if s.file_in(&folder, &name).is_some() {
                return error(conn, 409, "File with such a name already exists.");
            }
            let id = s.id();
            // the entry exists, empty, before any data arrives
            s.files.insert(id.clone(), File { folder, name: name.clone(), data: Vec::new() });
            let hash_only = s.dedup;
            json_reply(conn, 200, json!({"FileId": id, "Name": name, "RequireHashOnly": if hash_only { 1 } else { 0 }}))
        }
        ("POST", ["upload", "open_file_upload.json"]) => {
            let id = str_of(&body["file_id"]);
            assert!(s.files.contains_key(&id));
            let temp = format!("/tmp/up-{id}");
            s.uploads.insert(temp.clone(), Vec::new());
            json_reply(conn, 200, json!({"TempLocation": temp, "RequireHashOnly": false, "RequireCompression": false}))
        }
        ("POST", ["upload", "upload_file_chunk2.json", "OAUTH", id]) => {
            if s.broken_chunks > 0 {
                s.broken_chunks -= 1;
                return reply(conn, 503, &[], b"down");
            }
            if s.locked > 0 {
                s.locked -= 1;
                return error(conn, 423, "Another chunk of this file is being uploaded.");
            }
            assert!(s.files.contains_key(*id));
            let temp = req.query.get("temp_location").unwrap().clone();
            let offset: usize = req.query["chunk_offset"].parse().unwrap();
            let data = file_data(req);
            assert_eq!(req.query["chunk_size"].parse::<usize>().unwrap(), data.len());
            let held = s.uploads.get(&temp).expect("an open upload").len();
            if offset != held {
                s.offset_errors += 1;
                return error(conn, 400, &format!("Incorrect chunk offset: uploaded={held}, chunk_offset={offset}"));
            }
            s.uploads.get_mut(&temp).unwrap().extend_from_slice(&data);
            s.chunks += 1;
            if s.lost_answers > 0 {
                s.lost_answers -= 1;
                return reply(conn, 503, &[], b"lost");
            }
            json_reply(conn, 200, json!({"TotalWritten": data.len()}))
        }
        ("POST", ["upload", "close_file_upload.json"]) => {
            let id = str_of(&body["file_id"]);
            let size = body["file_size"].as_u64().unwrap() as usize;
            let hash = body["file_hash"].as_str().expect("the MD5 goes with every upload").to_string();
            let data = match body["temp_location"].as_str() {
                Some(t) => s.uploads.remove(t).expect("an open upload"),
                None => {
                    assert!(s.dedup, "no data only when OpenDrive asked for the hash only");
                    // the fake's dedup: the content is "already held" when it's the empty file or the test says so
                    vec![b'x'; size]
                }
            };
            if body["temp_location"].is_string() && (data.len() != size || md5(&data) != hash) {
                return error(conn, 400, "File is corrupted (invalid checksum). Please re-upload the file.");
            }
            let f = s.files.get_mut(&id).unwrap();
            f.data = data;
            let v = State::file_json(&id, f);
            json_reply(conn, 200, v)
        }
        ("POST", ["file", "rename.json"]) => {
            let (id, name) = (str_of(&body["file_id"]), body["new_file_name"].as_str().unwrap().to_string());
            let folder = s.files[&id].folder.clone();
            if s.file_in(&folder, &name).is_some() {
                return error(conn, 409, "File or folder with such name already exists.");
            }
            s.files.get_mut(&id).unwrap().name = name;
            let v = State::file_json(&id, &s.files[&id]);
            json_reply(conn, 200, v)
        }
        ("DELETE", ["file.json", "OAUTH", id]) => match s.files.remove(*id) {
            Some(_) => json_reply(conn, 200, json!({"DirUpdateTime": 1})),
            None => error(conn, 404, "File doesn't exist."),
        },
        ("GET", ["download", "file.json", id]) => {
            assert_eq!(req.query.get("session_id").map(String::as_str), Some("OAUTH"));
            match s.files.get(*id) {
                Some(f) => reply(conn, 200, &[("Content-Type", "application/octet-stream".into())], &f.data.clone()),
                None => error(conn, 404, "File does not exist."),
            }
        }
        other => panic!("unexpected call {other:?}"),
    }
}
