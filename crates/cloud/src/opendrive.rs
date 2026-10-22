//! OpenDrive (REST API v1, `https://dev.opendrive.com/api/v1`): objects are files under one
//! private folder in the account's root (`LightCraft` by default), addressed by path
//! (`LightCraft/blobs/ab/<id>.bin`); a [`RemoteRef`] is the object name.
//!
//! Sign-in is OpenDrive's OAuth 2.0 "password" grant ([`login`]): the username and password are
//! exchanged once for tokens and never kept. The refresh token lasts 30 days and is replaced on
//! every refresh (the old one stops working at once); the provider keeps the newest
//! ([`Provider::refresh_token`]) for the vault. The API takes the access token in the query
//! (`access_token=…`) with `OAUTH` where a call wants a session id.
//!
//! An upload is `create_file` → `open_file_upload` → chunks (`upload_file_chunk2`, in order) →
//! `close_file_upload`, which checks the file against its MD5. `create_file` makes the file entry
//! before any data arrives, so the data goes to a hidden temporary name first
//! (`.lc-upload-<name>`, never a valid [`ObjectName`]) and is renamed into place once complete:
//! an interrupted upload never shows as a short object. Replacing an object removes the old file
//! just before the rename. A chunk whose answer was lost is sent again; OpenDrive then says how
//! much it holds (`Incorrect chunk offset: uploaded=N`) and the upload goes on from there.
//! 429, 423 (a chunk still being written) and 5xx answers are retried with backoff.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use lightcraft_fetch::http::{Response, Url};
use md5::{Digest, Md5};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::browse::{Browse, BrowseDir, BrowseFile};
use crate::net::{MAX_META_BODY, Net, form, hex, is_local, retry_after};
use crate::oauth::{OAuthClient, TokenSource, Tokens};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// The partner id OpenDrive's own accounts sign in with.
pub const CLIENT_ID: &str = "OpenDrive";
/// Default upload chunk.
pub const DEFAULT_CHUNK: usize = 8 * 1024 * 1024;
/// Most files [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// Most folders [`Provider::list`] walks.
const MAX_FOLDERS: usize = 100_000;
/// Retries of a rate-limited or failed request (and of a stalled upload) before giving up.
pub const MAX_RETRIES: u32 = 5;
/// Prefix of the hidden name a file is uploaded under before it is renamed into place.
const UPLOAD_PREFIX: &str = ".lc-upload-";

fn default_api() -> String {
    "https://dev.opendrive.com/api/v1".into()
}
fn default_folder() -> String {
    "LightCraft".into()
}

/// One OpenDrive account, as stored in the vault. `Debug` never prints the refresh token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OpenDriveConfig {
    pub refresh_token: String,
    /// The folder objects live under, from the account's root (`LightCraft`, or `Backups/LightCraft`).
    #[serde(default = "default_folder")]
    pub folder: String,
    /// API root; changed only by tests (a local fake server).
    #[serde(default = "default_api")]
    pub api: String,
}

impl std::fmt::Debug for OpenDriveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenDriveConfig").field("folder", &self.folder).field("api", &self.api).finish_non_exhaustive()
    }
}

impl OpenDriveConfig {
    /// An account signed in with `tokens` (from [`login`]), at the default folder.
    pub fn new(api: &str, refresh_token: impl Into<String>) -> OpenDriveConfig {
        OpenDriveConfig { refresh_token: refresh_token.into(), folder: default_folder(), api: api.trim_end_matches('/').to_string() }
    }
}

/// The token endpoint under the API root `api`.
fn token_url(api: &str) -> String {
    format!("{}/oauth2/grant.json", api.trim_end_matches('/'))
}

fn parse_api(api: &str) -> Result<Url> {
    let url = Url::parse(api.trim_end_matches('/')).map_err(|e| CloudError::InvalidName(format!("OpenDrive API URL: {e}")))?;
    if !url.tls && !is_local(&url.host) {
        return Err(CloudError::Denied("refusing to send OpenDrive credentials over plain http:// to another computer".into()));
    }
    if url.path.contains('?') {
        return Err(CloudError::InvalidName("the OpenDrive API URL can't have a query".into()));
    }
    Ok(url)
}

/// Sign in with a username (or email) and password: the OAuth 2.0 password grant, the one way
/// OpenDrive offers. The password is sent once, here, and never kept; the result holds the
/// refresh token. `api` is [`OpenDriveConfig::api`] (`None`: OpenDrive's own).
pub fn login(api: Option<&str>, username: &str, password: &str, net: &Net) -> Result<OpenDriveConfig> {
    let api = api.map(str::to_string).unwrap_or_else(default_api);
    parse_api(&api)?;
    if username.trim().is_empty() || password.is_empty() {
        return Err(CloudError::Denied("give the OpenDrive username (or email) and password".into()));
    }
    let tokens: Tokens =
        crate::oauth::password_grant(&OAuthClient::opendrive(token_url(&api)), net, username.trim(), password).map_err(|e| match e {
            CloudError::Denied(m) => CloudError::Denied(format!("OpenDrive refused the sign-in: {}", m.trim_start_matches("sign in again: "))),
            e => e,
        })?;
    let refresh = tokens.refresh_token.filter(|t| !t.is_empty()).ok_or_else(|| CloudError::Denied("OpenDrive gave no refresh token".into()))?;
    Ok(OpenDriveConfig::new(&api, refresh))
}

/// A folder path from the account's root: `/`-separated names OpenDrive accepts.
fn check_folder(folder: &str) -> Result<()> {
    let ok = !folder.is_empty()
        && folder.len() <= 1024
        && folder.split('/').all(|seg| {
            !seg.is_empty()
                && seg.len() <= 255
                && seg != "."
                && seg != ".."
                && seg.trim() == seg
                && !seg.chars().any(|c| c.is_control() || "\\:*?\"<>|".contains(c) || c.len_utf8() == 4)
        });
    if ok {
        Ok(())
    } else {
        Err(CloudError::InvalidName(format!("OpenDrive folder {:?} (names separated by /)", folder.chars().take(64).collect::<String>())))
    }
}

/// An id OpenDrive gave us, checked before it goes into a URL path.
fn checked_id(v: &Value, what: &str) -> Result<String> {
    let id = match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        _ => String::new(),
    };
    if id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        return Err(CloudError::Corrupt(format!("OpenDrive {what} id")));
    }
    Ok(id)
}

/// A count or size OpenDrive sends as a number or as a string of digits.
fn number(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n.as_u64().or_else(|| n.as_f64().filter(|f| f.is_finite() && *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// A flag sent as `true`, `1` or `"1"`.
fn flag(v: &Value) -> bool {
    matches!(v, Value::Bool(true)) || number(v) == Some(1)
}

/// The message of an OpenDrive error reply (`{"error": {"code", "message"}}`), cleaned for display.
fn message(body: &[u8]) -> String {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    let m = v["error"]["message"].as_str().or(v["error_description"].as_str()).unwrap_or_default();
    m.chars().filter(|c| !c.is_control()).take(200).collect::<String>().trim().to_string()
}

/// An OpenDrive error reply as a [`CloudError`].
fn od_error(what: &str, status: u16, body: &[u8]) -> CloudError {
    let msg = message(body);
    let lower = msg.to_ascii_lowercase();
    if lower.contains("storage") && lower.contains("exceed") {
        return CloudError::QuotaExceeded;
    }
    let detail = if msg.is_empty() { format!("{what}: HTTP {status}") } else { format!("{what}: {msg}") };
    match status {
        404 => CloudError::NotFound,
        403 if lower.contains("already deleted") => CloudError::NotFound,
        401..=403 => CloudError::Denied(detail),
        509 => CloudError::Io(format!("{what}: OpenDrive's bandwidth limit for today was reached")),
        _ => CloudError::Io(detail),
    }
}

/// How much of an upload OpenDrive holds, from `Incorrect chunk offset: uploaded=N, …`.
fn uploaded_offset(body: &[u8]) -> Option<u64> {
    let msg = message(body);
    let rest = msg.split("uploaded=").nth(1)?;
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

pub struct OpenDriveProvider {
    /// Shared by the clones of an [`OpenDriveBrowser`]: OpenDrive replaces the refresh token on
    /// every refresh, so parallel workers must refresh through one source.
    tokens: Arc<Mutex<TokenSource>>,
    /// The refresh token as of the last request (for [`Provider::refresh_token`]).
    refresh: String,
    api: Url,
    folder: String,
    label: String,
    pub net: Net,
    /// First wait before a retry; doubles each time.
    pub retry_base: Duration,
    chunk: usize,
    /// Folder path (relative to `folder`, `""` for it) → folder id.
    folders: HashMap<String, String>,
}

impl OpenDriveProvider {
    pub fn new(config: OpenDriveConfig, label: impl Into<String>) -> Result<OpenDriveProvider> {
        let api = parse_api(&config.api)?;
        check_folder(&config.folder)?;
        if config.refresh_token.is_empty() {
            return Err(CloudError::Denied("not signed in to OpenDrive (no refresh token)".into()));
        }
        let client = OAuthClient::opendrive(token_url(&config.api));
        Ok(OpenDriveProvider {
            refresh: config.refresh_token.clone(),
            tokens: Arc::new(Mutex::new(TokenSource::new(client, config.refresh_token))),
            api,
            folder: config.folder,
            label: label.into(),
            net: Net::default(),
            retry_base: Duration::from_secs(1),
            chunk: DEFAULT_CHUNK,
            folders: HashMap::new(),
        })
    }

    /// Upload in chunks of `bytes` (1 KiB to 256 MiB).
    pub fn with_chunk_size(mut self, bytes: usize) -> Result<OpenDriveProvider> {
        if !(1024..=256 * 1024 * 1024).contains(&bytes) {
            return Err(CloudError::InvalidName(format!("OpenDrive upload chunks must be 1 KiB to 256 MiB, not {bytes}")));
        }
        self.chunk = bytes;
        Ok(self)
    }

    /// The refresh token in use.
    pub fn refresh_token(&self) -> &str {
        &self.refresh
    }

    /// The app folder, made (private) when missing.
    pub fn ensure_root(&mut self) -> Result<()> {
        self.folder_id("", true).map(|_| ())
    }

    /// An authorized request to `route` (under the API root) with `query`: the access token is
    /// added to the query, and a JSON `body` gets `"session_id": "OAUTH"`. A rejected access token
    /// is refreshed once; 423, 429 and 5xx answers are retried with backoff. Other answers are
    /// returned as they are.
    fn call(&mut self, method: &str, route: &str, query: &[(&str, &str)], body: Body) -> Result<Response> {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = {
                let mut t = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
                let token = t.bearer(&self.net)?;
                t.refresh_token().clone_into(&mut self.refresh);
                token
            };
            let mut q: Vec<(&str, &str)> = query.to_vec();
            q.push(("access_token", token.as_str()));
            let url = Url { path: format!("{}{route}?{}", self.api.path.trim_end_matches('/'), form(&q)), ..self.api.clone() };
            let (headers, bytes): (Vec<(&str, String)>, Vec<u8>) = match &body {
                Body::None => (Vec::new(), Vec::new()),
                Body::Json(v) => {
                    let mut v = v.clone();
                    if let Some(o) = v.as_object_mut() {
                        o.insert("session_id".into(), json!("OAUTH"));
                    }
                    (vec![("Content-Type", "application/json".to_string())], v.to_string().into_bytes())
                }
                Body::Multipart(boundary, data) => (vec![("Content-Type", format!("multipart/form-data; boundary={boundary}"))], data.clone()),
            };
            let resp = self.net.send(method, &url, &headers, &bytes)?;
            match resp.status {
                401 if !refreshed => {
                    self.tokens.lock().unwrap_or_else(PoisonError::into_inner).invalidate_if(&token);
                    refreshed = true;
                }
                423 | 429 | 500 | 502 | 503 | 504 if attempt < MAX_RETRIES => {
                    self.net.backoff(self.retry_base, attempt, retry_after(&resp))?;
                    attempt += 1;
                }
                _ => return Ok(resp),
            }
        }
    }

    fn error(&self, what: &str, resp: &mut Response) -> CloudError {
        let body = self.net.body(resp, 64 * 1024).unwrap_or_default();
        od_error(what, resp.status, &body)
    }

    fn json(&self, what: &str, resp: &mut Response) -> Result<Value> {
        if resp.status != 200 {
            return Err(self.error(what, resp));
        }
        let body = self.net.body(resp, MAX_META_BODY)?;
        serde_json::from_slice(&body).map_err(|e| CloudError::Corrupt(format!("{what}: OpenDrive reply: {e}")))
    }

    /// The id of the folder `rel` (`""`: the app folder; else `a/b` inside it), made when
    /// missing and `create`; a missing folder is [`CloudError::NotFound`] otherwise.
    fn folder_id(&mut self, rel: &str, create: bool) -> Result<String> {
        if let Some(id) = self.folders.get(rel) {
            return Ok(id.clone());
        }
        let full = if rel.is_empty() { self.folder.clone() } else { format!("{}/{rel}", self.folder) };
        let id = match self.find_folder(&full)? {
            Some(id) => id,
            None if !create => return Err(CloudError::NotFound),
            // the app folder, and any folders of the user's it is in, from the account's root
            None if rel.is_empty() => {
                let mut parent = "0".to_string();
                let mut path = String::new();
                for name in full.clone().split('/') {
                    path = if path.is_empty() { name.to_string() } else { format!("{path}/{name}") };
                    parent = match self.find_folder(&path)? {
                        Some(id) => id,
                        None => self.make_folder(&parent, name, &path)?,
                    };
                }
                parent
            }
            None => {
                let (up, name) = rel.rsplit_once('/').unwrap_or(("", rel));
                let parent = self.folder_id(up, true)?;
                self.make_folder(&parent, name, &full)?
            }
        };
        self.folders.insert(rel.to_string(), id.clone());
        Ok(id)
    }

    /// The id of the folder at `path` from the account's root, if there is one.
    fn find_folder(&mut self, path: &str) -> Result<Option<String>> {
        let mut resp = self.call("POST", "/folder/idbypath.json", &[], Body::Json(json!({ "path": path })))?;
        match self.json("find folder", &mut resp) {
            Ok(v) => checked_id(&v["FolderId"], "folder").map(Some),
            Err(CloudError::NotFound) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Make the private folder `name` in `parent` (`full` is its path, to find it when another
    /// device made it first).
    fn make_folder(&mut self, parent: &str, name: &str, full: &str) -> Result<String> {
        let mut resp =
            self.call("POST", "/folder.json", &[], Body::Json(json!({ "folder_name": name, "folder_sub_parent": parent, "folder_is_public": 0 })))?;
        if resp.status == 409 {
            return self.find_folder(full)?.ok_or_else(|| CloudError::Io(format!("OpenDrive has a file where the folder {full:?} goes")));
        }
        let v = self.json("create folder", &mut resp)?;
        checked_id(&v["FolderID"], "folder")
    }

    /// The file named `name` in the folder `folder_id`, if there is one.
    fn file_in(&mut self, folder_id: &str, name: &str) -> Result<Option<String>> {
        let route = format!("/folder/itembyname.json/OAUTH/{folder_id}");
        let mut resp = self.call("GET", &route, &[("name", name)], Body::None)?;
        let v = self.json("find file", &mut resp)?;
        match v["Files"].as_array().and_then(|f| f.iter().find(|f| f["Name"].as_str() == Some(name))) {
            Some(f) => checked_id(&f["FileId"], "file").map(Some),
            None => Ok(None),
        }
    }

    /// The id of the object `name`; [`CloudError::NotFound`] when it isn't there.
    fn object_id(&mut self, name: &ObjectName) -> Result<String> {
        let path = format!("{}/{}", self.folder, name.as_str());
        let mut resp = self.call("POST", "/file/idbypath.json", &[], Body::Json(json!({ "path": path })))?;
        let v = self.json("find file", &mut resp)?;
        checked_id(&v["FileId"], "file")
    }

    fn delete_id(&mut self, id: &str) -> Result<()> {
        let mut resp = self.call("DELETE", &format!("/file.json/OAUTH/{id}"), &[], Body::None)?;
        match resp.status {
            200 => Ok(()),
            _ => match self.error("delete", &mut resp) {
                CloudError::NotFound => Ok(()),
                e => Err(e),
            },
        }
    }

    /// Send `data` into the file `file_id` (made by `create_file`) and close it, checked against
    /// `md5`. `hash_only`: OpenDrive already holds this content and needs no data.
    fn upload_into(&mut self, file_id: &str, data: &[u8], md5: &str, mut hash_only: bool) -> Result<()> {
        let size = data.len().to_string();
        let mut temp = String::new();
        if !hash_only {
            let mut resp = self.call(
                "POST",
                "/upload/open_file_upload.json",
                &[],
                Body::Json(json!({ "file_id": file_id, "file_size": data.len(), "file_hash": md5 })),
            )?;
            let v = self.json("start upload", &mut resp)?;
            hash_only = flag(&v["RequireHashOnly"]);
            temp = v["TempLocation"].as_str().unwrap_or_default().to_string();
            if !hash_only && (temp.is_empty() || temp.len() > 4096 || temp.chars().any(char::is_control)) {
                return Err(CloudError::Corrupt("OpenDrive upload location".into()));
            }
        }
        if !hash_only && !data.is_empty() {
            self.send_chunks(file_id, &temp, data)?;
        }
        let mut close = json!({ "file_id": file_id, "file_size": data.len(), "file_hash": md5 });
        if !hash_only {
            close["temp_location"] = json!(temp);
        }
        let mut resp = self.call("POST", "/upload/close_file_upload.json", &[], Body::Json(close))?;
        let v = self.json("finish upload", &mut resp)?;
        // what OpenDrive stored is what was sent
        if let Some(stored) = v.get("Size").and_then(number)
            && stored.to_string() != size
        {
            return Err(CloudError::Corrupt(format!("OpenDrive stored {stored} of {size} bytes")));
        }
        Ok(())
    }

    /// The chunks, in order. A chunk whose answer was lost is resent, and OpenDrive's
    /// "Incorrect chunk offset: uploaded=N" says where to go on.
    fn send_chunks(&mut self, file_id: &str, temp: &str, data: &[u8]) -> Result<()> {
        let total = data.len() as u64;
        let mut offset = 0u64;
        let mut failures = 0u32;
        let max_requests = (total / self.chunk as u64).saturating_add(1).saturating_mul(8).saturating_add(64);
        let mut requests = 0u64;
        let route = format!("/upload/upload_file_chunk2.json/OAUTH/{file_id}");
        while offset < total {
            requests += 1;
            if requests > max_requests {
                return Err(CloudError::Io("the upload stopped making progress".into()));
            }
            let end = offset.saturating_add(self.chunk as u64).min(total);
            let part = usize::try_from(offset).ok().zip(usize::try_from(end).ok()).and_then(|(a, b)| data.get(a..b));
            let part = part.ok_or_else(|| CloudError::Corrupt("upload offset outside the data".into()))?;
            let (off, len) = (offset.to_string(), part.len().to_string());
            let query = [("temp_location", temp), ("chunk_offset", off.as_str()), ("chunk_size", len.as_str())];
            let sent = self.call("POST", &route, &query, multipart(part));
            let mut resp = match sent {
                Ok(r) => r,
                Err(CloudError::Io(e)) => {
                    // the chunk may or may not have arrived: send it again and let OpenDrive say
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(CloudError::Io(e));
                    }
                    self.net.backoff(self.retry_base, failures - 1, None)?;
                    continue;
                }
                Err(e) => return Err(e),
            };
            match resp.status {
                200 => {
                    let v = self.json("upload", &mut resp)?;
                    if v.get("TotalWritten").and_then(number).is_some_and(|n| n != part.len() as u64) {
                        return Err(CloudError::Corrupt("OpenDrive wrote part of a chunk".into()));
                    }
                    offset = end;
                    failures = 0;
                }
                400 => {
                    let body = self.net.body(&mut resp, 64 * 1024).unwrap_or_default();
                    let Some(held) = uploaded_offset(&body) else { return Err(od_error("upload", 400, &body)) };
                    if held > total {
                        return Err(CloudError::Corrupt(format!("OpenDrive holds {held} bytes of a {total}-byte upload")));
                    }
                    if held > offset {
                        failures = 0;
                    } else {
                        failures += 1;
                        if failures > MAX_RETRIES {
                            return Err(CloudError::Io("the upload stopped making progress".into()));
                        }
                    }
                    offset = held;
                }
                _ => return Err(self.error("upload", &mut resp)),
            }
        }
        Ok(())
    }

    /// Give the uploaded file `file_id` its final name in `folder_id`, replacing a file there.
    fn rename_into_place(&mut self, folder_id: &str, file_id: &str, name: &str) -> Result<()> {
        for replaced in [false, true] {
            let mut resp = self.call("POST", "/file/rename.json", &[], Body::Json(json!({ "file_id": file_id, "new_file_name": name })))?;
            match resp.status {
                200 => return Ok(()),
                409 if !replaced => {
                    if let Some(old) = self.file_in(folder_id, name)? {
                        self.delete_id(&old)?;
                    }
                }
                _ => return Err(self.error("rename", &mut resp)),
            }
        }
        Err(CloudError::Io("OpenDrive kept another file under the name".into()))
    }

    /// The files under `folder_id` (whose path in the app folder is `prefix`), into `out`.
    fn walk(&mut self, folder_id: &str, prefix: &str, depth: usize, out: &mut BTreeMap<ObjectName, Listed>, folders: &mut usize) -> Result<()> {
        *folders += 1;
        if *folders > MAX_FOLDERS {
            return Err(CloudError::Io(format!("more than {MAX_FOLDERS} folders in the OpenDrive app folder")));
        }
        let mut resp = self.call("GET", &format!("/folder/list.json/OAUTH/{folder_id}"), &[], Body::None)?;
        let v = self.json("list", &mut resp)?;
        for f in v["Files"].as_array().map(Vec::as_slice).unwrap_or_default() {
            let Some(file) = f["Name"].as_str() else { continue };
            let Ok(name) = ObjectName::parse(&format!("{prefix}{file}")) else { continue };
            if out.len() >= MAX_LISTED {
                return Err(CloudError::Io(format!("more than {MAX_LISTED} files in the OpenDrive app folder")));
            }
            let remote = RemoteRef { key: name.as_str().to_string() };
            out.insert(name.clone(), Listed { name, remote, size: number(&f["Size"]).unwrap_or(0) });
        }
        // names are at most MAX_DEPTH segments deep: deeper folders hold none of ours
        if depth + 1 >= crate::blob::MAX_DEPTH {
            return Ok(());
        }
        let subs: Vec<(String, String)> = v["Folders"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
            .iter()
            .filter_map(|f| Some((f["Name"].as_str()?.to_string(), checked_id(&f["FolderID"], "folder").ok()?)))
            .collect();
        for (name, id) in subs {
            // folders a name can't go through aren't ours
            if ObjectName::parse(&format!("{prefix}{name}/x")).is_err() {
                continue;
            }
            self.walk(&id, &format!("{prefix}{name}/"), depth + 1, out, folders)?;
        }
        Ok(())
    }
}

/// Read-only access to the account's own folders (the photos already there), from a signed-in
/// [`OpenDriveProvider`]: only the reading calls are reachable through it.
pub struct OpenDriveBrowser(OpenDriveProvider);

impl OpenDriveProvider {
    /// This account as a [`Browse`]: list folders, read hashes and byte ranges, nothing else.
    pub fn into_browser(self) -> OpenDriveBrowser {
        OpenDriveBrowser(self)
    }
}

impl OpenDriveBrowser {
    /// The refresh token in use (OpenDrive replaces it on every refresh: the vault must keep it).
    pub fn refresh_token(&self) -> String {
        self.0.tokens.lock().unwrap_or_else(PoisonError::into_inner).refresh_token().to_string()
    }

    /// Another browser on the same account for a parallel worker: its own connection settings,
    /// the same tokens.
    pub fn worker(&self) -> OpenDriveBrowser {
        let p = &self.0;
        OpenDriveBrowser(OpenDriveProvider {
            tokens: p.tokens.clone(),
            refresh: p.refresh.clone(),
            api: p.api.clone(),
            folder: p.folder.clone(),
            label: p.label.clone(),
            net: Net { cancel: p.net.cancel.clone(), ..Net::default() },
            retry_base: p.retry_base,
            chunk: p.chunk,
            folders: HashMap::new(),
        })
    }

    pub fn net(&self) -> &Net {
        &self.0.net
    }

    pub fn set_net(&mut self, net: Net) {
        self.0.net = net;
    }
}

/// A file or folder name from a listing, kept only when it can be part of a path.
fn plain_name(v: &Value) -> Option<String> {
    let n = v.as_str()?;
    (!n.is_empty() && n.len() <= 1024 && !n.contains('/') && n != "." && n != ".." && !n.chars().any(char::is_control)).then(|| n.to_string())
}

impl Browse for OpenDriveBrowser {
    fn kind(&self) -> &str {
        "opendrive"
    }

    fn folder(&mut self, path: &str) -> Result<String> {
        let path = path.trim_matches('/');
        if path.is_empty() {
            return Ok("0".into());
        }
        self.0.find_folder(path)?.ok_or(CloudError::NotFound)
    }

    fn list(&mut self, folder: &str) -> Result<BrowseDir> {
        let id = checked_id(&json!(folder), "folder")?;
        let mut resp = self.0.call("GET", &format!("/folder/list.json/OAUTH/{id}"), &[], Body::None)?;
        let v = self.0.json("list", &mut resp)?;
        let mut out = BrowseDir::default();
        for f in v["Folders"].as_array().map(Vec::as_slice).unwrap_or_default() {
            if let (Ok(id), Some(name)) = (checked_id(&f["FolderID"], "folder"), plain_name(&f["Name"])) {
                out.folders.push((id, name));
            }
        }
        for f in v["Files"].as_array().map(Vec::as_slice).unwrap_or_default() {
            if let (Ok(id), Some(name)) = (checked_id(&f["FileId"], "file"), plain_name(&f["Name"])) {
                let modified = f["DateModified"].as_str().map(str::to_string).or_else(|| number(&f["DateModified"]).map(|n| n.to_string()));
                out.files.push(BrowseFile { id, name, size: number(&f["Size"]).unwrap_or(0), modified });
            }
        }
        Ok(out)
    }

    fn content_hash(&mut self, file: &str) -> Result<Option<String>> {
        let id = checked_id(&json!(file), "file")?;
        let mut resp = self.0.call("GET", &format!("/file/info.json/{id}"), &[("session_id", "OAUTH")], Body::None)?;
        let v = self.0.json("file info", &mut resp)?;
        Ok(v["FileHash"]
            .as_str()
            .map(str::to_ascii_lowercase)
            .filter(|h| !h.is_empty() && h.len() <= 128 && h.bytes().all(|b| b.is_ascii_hexdigit())))
    }

    fn read_range(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let id = checked_id(&json!(file), "file")?;
        let off = offset.to_string();
        let query: Vec<(&str, &str)> = if offset > 0 { vec![("session_id", "OAUTH"), ("offset", &off)] } else { vec![("session_id", "OAUTH")] };
        let mut resp = self.0.call("GET", &format!("/download/file.json/{id}"), &query, Body::None)?;
        match resp.status {
            200 | 206 => {
                // a server that ignored the offset would send the start of the file instead
                if let Some(range) = resp.header("Content-Range")
                    && !range.trim_start_matches("bytes ").starts_with(&format!("{offset}-"))
                {
                    return Err(CloudError::Corrupt(format!("OpenDrive sent the wrong part of the file ({range})")));
                }
                self.0.net.body_prefix(&mut resp, len.min(crate::browse::MAX_RANGE))
            }
            416 => Ok(Vec::new()),
            _ => Err(self.0.error("download", &mut resp)),
        }
    }
}

/// A request body.
enum Body {
    None,
    Json(Value),
    /// Boundary and the whole `multipart/form-data` body.
    Multipart(String, Vec<u8>),
}

/// `data` as the `file_data` field of a `multipart/form-data` body.
fn multipart(data: &[u8]) -> Body {
    // a boundary from the data's own hash can't appear in it by accident
    let boundary = format!("lightcraft-{}", hex(&Md5::digest(data)));
    let mut body = Vec::with_capacity(data.len() + 256);
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(b"Content-Disposition: form-data; name=\"file_data\"; filename=\"chunk\"\r\n");
    body.extend_from_slice(b"Content-Type: application/octet-stream\r\n\r\n");
    body.extend_from_slice(data);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Body::Multipart(boundary, body)
}

impl Provider for OpenDriveProvider {
    fn refresh_token(&self) -> Option<&str> {
        Some(&self.refresh)
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "opendrive".into(), label: self.label.clone(), max_file_size: None }
    }

    /// The account's plan size (`MaxStorage`, in MB) and what it uses (`StorageUsed`, bytes).
    fn quota(&mut self) -> Result<Quota> {
        let mut resp = self.call("GET", "/users/info.json/OAUTH", &[], Body::None)?;
        let v = self.json("storage quota", &mut resp)?;
        let total = number(&v["MaxStorage"]).filter(|mb| *mb > 0).map(|mb| mb.saturating_mul(1024 * 1024));
        Ok(Quota { total, used: number(&v["StorageUsed"]).unwrap_or(0) })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        let (dir, file) = name.as_str().rsplit_once('/').unwrap_or(("", name.as_str()));
        let folder = self.folder_id(dir, true)?;
        let md5 = hex(&Md5::digest(data));
        let temp = format!("{UPLOAD_PREFIX}{file}");
        let create = json!({ "folder_id": folder, "file_name": temp, "file_size": data.len(), "file_hash": md5, "open_if_exists": 0 });
        let mut resp = self.call("POST", "/upload/create_file.json", &[], Body::Json(create.clone()))?;
        if resp.status == 409 {
            // what an interrupted upload left: start again
            if let Some(old) = self.file_in(&folder, &temp)? {
                self.delete_id(&old)?;
            }
            resp = self.call("POST", "/upload/create_file.json", &[], Body::Json(create))?;
        }
        let v = self.json("create file", &mut resp)?;
        let file_id = checked_id(&v["FileId"], "file")?;
        let done = self.upload_into(&file_id, data, &md5, flag(&v["RequireHashOnly"])).and_then(|()| self.rename_into_place(&folder, &file_id, file));
        if let Err(e) = done {
            let _ = self.delete_id(&file_id);
            return Err(e);
        }
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let name = ObjectName::parse(&remote.key)?;
        let id = self.object_id(&name)?;
        let mut resp = self.call("GET", &format!("/download/file.json/{id}"), &[("session_id", "OAUTH")], Body::None)?;
        if resp.status != 200 {
            return Err(self.error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let name = ObjectName::parse(&remote.key)?;
        match self.object_id(&name) {
            Ok(id) => self.delete_id(&id),
            Err(CloudError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let root = match self.folder_id("", false) {
            Ok(id) => id,
            // an app folder nothing was stored in yet
            Err(CloudError::NotFound) => return Ok(Vec::new()),
            Err(e) => return Err(e),
        };
        let mut out = BTreeMap::new();
        let mut folders = 0usize;
        self.walk(&root, "", 0, &mut out, &mut folders)?;
        Ok(out.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OpenDriveConfig {
        OpenDriveConfig::new("https://dev.opendrive.com/api/v1", "refresh-me")
    }

    #[test]
    fn config_is_checked_and_secrets_are_not_printed() {
        let c = config();
        assert!(!format!("{c:?}").contains("refresh-me"));
        assert!(OpenDriveProvider::new(c.clone(), "t").is_ok());
        assert!(OpenDriveProvider::new(OpenDriveConfig { folder: "Backups/LightCraft".into(), ..c.clone() }, "t").is_ok());
        for bad in ["", "/x", "x/", "a//b", "a/../b", "a:b", "a\nb", " a", "😀"] {
            assert!(OpenDriveProvider::new(OpenDriveConfig { folder: bad.into(), ..c.clone() }, "t").is_err(), "{bad:?}");
        }
        assert!(matches!(OpenDriveProvider::new(OpenDriveConfig { api: "http://od.example".into(), ..c.clone() }, "t"), Err(CloudError::Denied(_))));
        assert!(OpenDriveProvider::new(OpenDriveConfig { refresh_token: String::new(), ..c.clone() }, "t").is_err());
        assert!(OpenDriveProvider::new(c, "t").unwrap().with_chunk_size(10).is_err());
        let c: OpenDriveConfig = serde_json::from_str(r#"{"refresh_token":"r"}"#).unwrap();
        assert_eq!((c.folder.as_str(), c.api.as_str()), ("LightCraft", "https://dev.opendrive.com/api/v1"));
    }

    #[test]
    fn replies_are_read_leniently_and_checked() {
        assert_eq!(number(&json!("123")), Some(123));
        assert_eq!(number(&json!(5)), Some(5));
        assert_eq!(number(&json!("-1")), None);
        assert!(flag(&json!(true)) && flag(&json!(1)) && flag(&json!("1")) && !flag(&json!(0)) && !flag(&Value::Null));
        assert_eq!(checked_id(&json!("MV8xMjM0NQ"), "file").unwrap(), "MV8xMjM0NQ");
        assert_eq!(checked_id(&json!(42), "file").unwrap(), "42");
        for bad in [json!(""), json!("../x"), json!("a/b"), json!("a?b"), Value::Null] {
            assert!(checked_id(&bad, "file").is_err(), "{bad}");
        }
    }

    #[test]
    fn error_replies() {
        let e = |code: u16, m: &str| od_error("x", code, json!({"error": {"code": code, "message": m}}).to_string().as_bytes());
        assert_eq!(e(400, "Storage exceeded the account owner values."), CloudError::QuotaExceeded);
        assert_eq!(e(404, "File not found."), CloudError::NotFound);
        assert_eq!(e(403, "File is already deleted."), CloudError::NotFound);
        assert!(matches!(e(402, "Account is suspended, please contact support."), CloudError::Denied(m) if m.contains("suspended")));
        assert!(matches!(e(509, "Bandwidth limit has exceeded."), CloudError::Io(m) if m.contains("bandwidth")));
        assert!(matches!(od_error("x", 500, b"<html>"), CloudError::Io(m) if m == "x: HTTP 500"));
        let offset = json!({"error": {"code": 400, "message": "Incorrect chunk offset: uploaded=8388608, chunk_offset=0"}}).to_string();
        assert_eq!(uploaded_offset(offset.as_bytes()), Some(8_388_608));
        assert_eq!(uploaded_offset(b"junk"), None);
    }

    #[test]
    fn chunks_go_as_the_file_data_field() {
        let Body::Multipart(boundary, body) = multipart(b"abc") else { panic!() };
        let text = String::from_utf8(body).unwrap();
        assert!(text.starts_with(&format!("--{boundary}\r\n")));
        assert!(text.contains("name=\"file_data\""));
        assert!(text.ends_with(&format!("\r\n\r\nabc\r\n--{boundary}--\r\n")));
    }
}
