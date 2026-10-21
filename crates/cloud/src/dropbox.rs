//! Dropbox (API v2), for an app registered with "App folder" access: objects are files under
//! `Apps/<app name>` (the app's root, `""`), addressed by path (`/blobs/ab/<id>.bin`); a
//! [`RemoteRef`] is the object name.
//!
//! Sign-in is [`crate::oauth`] with [`OAuthClient::dropbox`]. Dropbox matches redirect URIs
//! exactly, so the app's registration lists the fixed loopback port it signs in on.
//!
//! Uploads up to 8 MiB are one request; larger ones use an upload session, appended in chunks.
//! A chunk whose answer was lost is sent again; Dropbox then says how much it really holds
//! (`incorrect_offset`) and the upload goes on from there. Rate limits (429) and 5xx answers are
//! retried, honouring `Retry-After`.

use std::collections::BTreeMap;
use std::time::Duration;

use lightcraft_fetch::http::{Response, Url};
use serde::{Deserialize, Serialize};

use crate::net::{MAX_META_BODY, Net, is_local, retry_after};
use crate::oauth::{OAuthClient, TokenSource};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// Read and write the app folder's files, and read the account's space usage.
pub const SCOPES: &[&str] = &["files.content.write", "files.content.read", "files.metadata.read", "account_info.read"];
/// Dropbox's per-file limit for uploads through the API.
pub const MAX_FILE_SIZE: u64 = 350 * 1024 * 1024 * 1024;
/// Largest upload sent as one request; larger ones use an upload session.
pub const SIMPLE_MAX: usize = 8 * 1024 * 1024;
/// Default upload session chunk (a multiple of 4 MiB, as Dropbox recommends).
pub const DEFAULT_CHUNK: usize = 8 * 1024 * 1024;
/// Most files [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// Retries of a rate-limited or failed request (and of a stalled upload) before giving up.
pub const MAX_RETRIES: u32 = 5;
/// The loopback port LightCraft's Dropbox registration lists by default.
pub const DEFAULT_REDIRECT_PORT: u16 = 53682;

fn default_api() -> String {
    "https://api.dropboxapi.com".into()
}
fn default_content() -> String {
    "https://content.dropboxapi.com".into()
}
fn default_token_url() -> String {
    "https://api.dropboxapi.com/oauth2/token".into()
}

/// One Dropbox account, as stored in the vault. `Debug` never prints the refresh token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DropboxConfig {
    pub client_id: String,
    pub refresh_token: String,
    /// The folder objects live under, relative to the app's root: `""` (the app folder itself)
    /// or `/name`.
    #[serde(default)]
    pub root: String,
    /// API roots; changed only by tests (a local fake server).
    #[serde(default = "default_api")]
    pub api: String,
    #[serde(default = "default_content")]
    pub content: String,
    #[serde(default = "default_token_url")]
    pub token_url: String,
}

impl DropboxConfig {
    /// An account signed in through [`crate::oauth::LoopbackFlow`] with `client`.
    pub fn new(client: &OAuthClient, refresh_token: impl Into<String>) -> DropboxConfig {
        DropboxConfig {
            client_id: client.client_id.clone(),
            refresh_token: refresh_token.into(),
            root: String::new(),
            api: default_api(),
            content: default_content(),
            token_url: client.token_url.clone(),
        }
    }
}

impl std::fmt::Debug for DropboxConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DropboxConfig").field("client_id", &self.client_id).field("root", &self.root).field("api", &self.api).finish_non_exhaustive()
    }
}

pub struct DropboxProvider {
    tokens: TokenSource,
    api: Url,
    content: Url,
    root: String,
    label: String,
    pub net: Net,
    /// First wait before a retry; doubles each time.
    pub retry_base: Duration,
    chunk: usize,
}

#[derive(Deserialize)]
struct SpaceUsage {
    #[serde(default)]
    used: u64,
    #[serde(default)]
    allocation: Option<Allocation>,
}

#[derive(Deserialize)]
struct Allocation {
    #[serde(default)]
    allocated: Option<u64>,
}

#[derive(Deserialize)]
struct ListFolder {
    #[serde(default)]
    entries: Vec<Entry>,
    #[serde(default)]
    cursor: String,
    #[serde(default)]
    has_more: bool,
}

#[derive(Deserialize)]
struct Entry {
    #[serde(rename = ".tag")]
    tag: String,
    #[serde(default)]
    path_lower: Option<String>,
    #[serde(default)]
    size: Option<u64>,
}

#[derive(Deserialize)]
struct SessionStart {
    session_id: String,
}

#[derive(Deserialize)]
struct ErrorReply {
    #[serde(default)]
    error_summary: String,
    #[serde(default)]
    error: Option<serde_json::Value>,
}

/// A Dropbox error reply as a [`CloudError`].
fn dropbox_error(what: &str, status: u16, body: &[u8]) -> CloudError {
    let summary = serde_json::from_slice::<ErrorReply>(body).map(|r| r.error_summary).unwrap_or_default();
    let summary: String = summary.chars().filter(|c| !c.is_control()).take(200).collect();
    if summary.contains("insufficient_space") || status == 507 {
        return CloudError::QuotaExceeded;
    }
    if summary.contains("not_found") {
        return CloudError::NotFound;
    }
    let detail = if summary.is_empty() { format!("{what}: HTTP {status}") } else { format!("{what}: {summary}") };
    match status {
        404 => CloudError::NotFound,
        401 | 403 => CloudError::Denied(detail),
        _ if summary.contains("too_large") => CloudError::TooLarge { size: 0, max: 0 },
        _ => CloudError::Io(detail),
    }
}

/// The offset Dropbox holds, from an `incorrect_offset` error on an upload session.
fn correct_offset(body: &[u8]) -> Option<u64> {
    let reply: ErrorReply = serde_json::from_slice(body).ok()?;
    let error = reply.error?;
    // append_v2 and finish both report it, the latter nested under "lookup_failed"
    let e = if error.get(".tag")?.as_str()? == "lookup_failed" { error.get("lookup_failed")? } else { &error };
    if e.get(".tag")?.as_str()? != "incorrect_offset" {
        return None;
    }
    e.get("correct_offset")?.as_u64()
}

fn check_root(root: &str) -> Result<()> {
    let ok = root.is_empty()
        || (root.starts_with('/')
            && !root.ends_with('/')
            && !root.contains("//")
            && root.len() <= 255
            && root.chars().all(|c| c.is_ascii_graphic() || c == ' ')
            && !root.split('/').any(|seg| seg == "." || seg == ".."));
    if ok {
        Ok(())
    } else {
        Err(CloudError::InvalidName(format!("Dropbox folder {:?} (use \"\" or /name)", root.chars().take(64).collect::<String>())))
    }
}

impl DropboxProvider {
    pub fn new(config: DropboxConfig, label: impl Into<String>) -> Result<DropboxProvider> {
        let parse = |s: &str, what: &str| -> Result<Url> {
            let url = Url::parse(s.trim_end_matches('/')).map_err(|e| CloudError::InvalidName(format!("Dropbox {what} URL: {e}")))?;
            if !url.tls && !is_local(&url.host) {
                return Err(CloudError::Denied("refusing to send Dropbox credentials over plain http:// to another computer".into()));
            }
            if url.path.contains('?') {
                return Err(CloudError::InvalidName(format!("the Dropbox {what} URL can't have a query")));
            }
            Ok(url)
        };
        let api = parse(&config.api, "API")?;
        let content = parse(&config.content, "content")?;
        check_root(&config.root)?;
        if config.client_id.is_empty() || config.refresh_token.is_empty() {
            return Err(CloudError::Denied("not signed in to Dropbox (no client id or refresh token)".into()));
        }
        let mut client = OAuthClient::dropbox(config.client_id, DEFAULT_REDIRECT_PORT);
        client.token_url = config.token_url;
        Ok(DropboxProvider {
            tokens: TokenSource::new(client, config.refresh_token),
            api,
            content,
            root: config.root,
            label: label.into(),
            net: Net::default(),
            retry_base: Duration::from_secs(1),
            chunk: DEFAULT_CHUNK,
        })
    }

    /// Upload sessions in chunks of `bytes` (a positive multiple of 4 MiB, at most 128 MiB).
    pub fn with_chunk_size(mut self, bytes: usize) -> Result<DropboxProvider> {
        const UNIT: usize = 4 * 1024 * 1024;
        if bytes == 0 || !bytes.is_multiple_of(UNIT) || bytes > 128 * 1024 * 1024 {
            return Err(CloudError::InvalidName(format!("Dropbox upload chunks must be a multiple of 4 MiB up to 128 MiB, not {bytes}")));
        }
        self.chunk = bytes;
        Ok(self)
    }

    /// The refresh token in use.
    pub fn refresh_token(&self) -> &str {
        self.tokens.refresh_token()
    }

    fn path(&self, name: &str) -> String {
        format!("{}/{name}", self.root)
    }

    fn endpoint(base: &Url, route: &str) -> Url {
        Url { path: format!("{}/2/{route}", base.path.trim_end_matches('/')), ..base.clone() }
    }

    /// An authorized request: a rejected access token is refreshed once; 429 and 5xx answers
    /// are retried with backoff. Other answers are returned as they are.
    fn send(&mut self, url: &Url, headers: &[(&'static str, String)], body: &[u8]) -> Result<Response> {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.bearer(&self.net)?;
            let mut h = headers.to_vec();
            h.push(("Authorization", format!("Bearer {token}")));
            let resp = self.net.send("POST", url, &h, body)?;
            match resp.status {
                401 if !refreshed => {
                    self.tokens.invalidate();
                    refreshed = true;
                }
                429 | 500 | 502 | 503 | 504 if attempt < MAX_RETRIES => {
                    self.net.backoff(self.retry_base, attempt, retry_after(&resp))?;
                    attempt += 1;
                }
                _ => return Ok(resp),
            }
        }
    }

    /// A call to an RPC endpoint on the API host: JSON in, JSON out.
    fn rpc(&mut self, route: &str, arg: &serde_json::Value) -> Result<Response> {
        let url = Self::endpoint(&self.api, route);
        self.send(&url, &[("Content-Type", "application/json".to_string())], arg.to_string().as_bytes())
    }

    /// A call to a content endpoint: the argument in a header, bytes in the body.
    fn content_call(&mut self, route: &str, arg: &serde_json::Value, body: &[u8]) -> Result<Response> {
        let url = Self::endpoint(&self.content, route);
        // serde_json writes non-ASCII as is; HTTP headers must be ASCII
        let arg = arg.to_string();
        if !arg.is_ascii() {
            return Err(CloudError::InvalidName("a Dropbox path must be ASCII".into()));
        }
        self.send(&url, &[("Dropbox-API-Arg", arg), ("Content-Type", "application/octet-stream".to_string())], body)
    }

    fn error(&self, what: &str, resp: &mut Response) -> CloudError {
        let body = self.net.body(resp, 64 * 1024).unwrap_or_default();
        dropbox_error(what, resp.status, &body)
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, what: &str, resp: &mut Response) -> Result<T> {
        if resp.status != 200 {
            return Err(self.error(what, resp));
        }
        let body = self.net.body(resp, MAX_META_BODY)?;
        serde_json::from_slice(&body).map_err(|e| CloudError::Corrupt(format!("{what}: Dropbox reply: {e}")))
    }

    /// An upload session: start, append each chunk, finish with the commit. A chunk whose answer
    /// was lost is resent, and Dropbox's `incorrect_offset` says where to go on.
    fn upload_session(&mut self, path: &str, data: &[u8]) -> Result<()> {
        let total = data.len() as u64;
        let mut resp = self.content_call("files/upload_session/start", &serde_json::json!({ "close": false }), &[])?;
        let start: SessionStart = self.json("start upload", &mut resp)?;
        if start.session_id.is_empty() || start.session_id.len() > 1024 {
            return Err(CloudError::Corrupt("Dropbox upload session id".into()));
        }
        let id = start.session_id;
        let mut offset = 0u64;
        let mut failures = 0u32;
        let max_requests = (total / self.chunk as u64).saturating_add(1).saturating_mul(8).saturating_add(64);
        let mut requests = 0u64;
        loop {
            requests += 1;
            if requests > max_requests {
                return Err(CloudError::Io("the upload stopped making progress".into()));
            }
            let finishing = offset >= total;
            let end = offset.saturating_add(self.chunk as u64).min(total);
            let part = usize::try_from(offset).ok().zip(usize::try_from(end).ok()).and_then(|(a, b)| data.get(a..b));
            let part = part.ok_or_else(|| CloudError::Corrupt("upload offset outside the data".into()))?;
            let cursor = serde_json::json!({ "session_id": id, "offset": offset });
            let sent = if finishing {
                let commit = serde_json::json!({ "path": path, "mode": "overwrite", "mute": true, "strict_conflict": false });
                self.content_call("files/upload_session/finish", &serde_json::json!({ "cursor": cursor, "commit": commit }), &[])
            } else {
                self.content_call("files/upload_session/append_v2", &serde_json::json!({ "cursor": cursor, "close": false }), part)
            };
            let mut resp = match sent {
                Ok(r) => r,
                Err(CloudError::Io(e)) => {
                    // the chunk may or may not have arrived: send it again and let Dropbox say
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
                200 if finishing => return Ok(()),
                200 => {
                    offset = end;
                    failures = 0;
                }
                409 => {
                    let body = self.net.body(&mut resp, 64 * 1024).unwrap_or_default();
                    let Some(held) = correct_offset(&body) else { return Err(dropbox_error("upload", 409, &body)) };
                    if held > total {
                        return Err(CloudError::Corrupt(format!("Dropbox holds {held} bytes of a {total}-byte upload")));
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
    }
}

impl Provider for DropboxProvider {
    fn refresh_token(&self) -> Option<&str> {
        Some(self.tokens.refresh_token())
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "dropbox".into(), label: self.label.clone(), max_file_size: Some(MAX_FILE_SIZE) }
    }

    /// The account's space (a team's shared space for team accounts).
    fn quota(&mut self) -> Result<Quota> {
        let url = Self::endpoint(&self.api, "users/get_space_usage");
        let mut resp = self.send(&url, &[("Content-Type", "application/json".to_string())], b"null")?;
        let usage: SpaceUsage = self.json("storage quota", &mut resp)?;
        Ok(Quota { total: usage.allocation.and_then(|a| a.allocated).filter(|t| *t > 0), used: usage.used })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        if data.len() as u64 > MAX_FILE_SIZE {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: MAX_FILE_SIZE });
        }
        let path = self.path(name.as_str());
        if data.len() <= SIMPLE_MAX {
            let arg = serde_json::json!({ "path": path, "mode": "overwrite", "mute": true, "strict_conflict": false });
            let mut resp = self.content_call("files/upload", &arg, data)?;
            if resp.status != 200 {
                return Err(self.error("upload", &mut resp));
            }
        } else {
            self.upload_session(&path, data)?;
        }
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let name = ObjectName::parse(&remote.key)?;
        let path = self.path(name.as_str());
        let mut resp = self.content_call("files/download", &serde_json::json!({ "path": path }), &[])?;
        if resp.status != 200 {
            return Err(self.error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let name = ObjectName::parse(&remote.key)?;
        let path = self.path(name.as_str());
        let mut resp = self.rpc("files/delete_v2", &serde_json::json!({ "path": path }))?;
        match resp.status {
            200 => Ok(()),
            _ => match self.error("delete", &mut resp) {
                CloudError::NotFound => Ok(()),
                e => Err(e),
            },
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let mut out: BTreeMap<ObjectName, Listed> = BTreeMap::new();
        let prefix = format!("{}/", self.root.to_ascii_lowercase());
        let mut resp = self.rpc("files/list_folder", &serde_json::json!({ "path": self.root, "recursive": true, "limit": 2000 }))?;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_LISTED / 100 {
                return Err(CloudError::Io("too many listing pages from Dropbox".into()));
            }
            let page: ListFolder = match self.json("list", &mut resp) {
                Ok(p) => p,
                // an app folder nothing was stored in yet doesn't exist
                Err(CloudError::NotFound) if pages == 1 => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            for entry in page.entries {
                if entry.tag != "file" {
                    continue;
                }
                let Some(rel) = entry.path_lower.as_deref().and_then(|p| p.strip_prefix(&prefix)) else { continue };
                let Ok(name) = ObjectName::parse(rel) else { continue };
                if out.len() >= MAX_LISTED {
                    return Err(CloudError::Io(format!("more than {MAX_LISTED} files in the Dropbox app folder")));
                }
                let remote = RemoteRef { key: name.as_str().to_string() };
                out.insert(name.clone(), Listed { name, remote, size: entry.size.unwrap_or(0) });
            }
            if !page.has_more {
                return Ok(out.into_values().collect());
            }
            if page.cursor.is_empty() || page.cursor.len() > 64 * 1024 {
                return Err(CloudError::Corrupt("Dropbox listing cursor".into()));
            }
            resp = self.rpc("files/list_folder/continue", &serde_json::json!({ "cursor": page.cursor }))?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> DropboxConfig {
        DropboxConfig::new(&OAuthClient::dropbox("cid", DEFAULT_REDIRECT_PORT), "refresh-me")
    }

    #[test]
    fn config_is_checked_and_secrets_are_not_printed() {
        let c = config();
        assert!(!format!("{c:?}").contains("refresh-me"));
        assert!(DropboxProvider::new(c.clone(), "t").is_ok());
        assert!(DropboxProvider::new(DropboxConfig { root: "/LightCraft".into(), ..c.clone() }, "t").is_ok());
        for bad in ["LightCraft", "/x/", "//x", "/a/../b", "/é", "/a\nb"] {
            assert!(DropboxProvider::new(DropboxConfig { root: bad.into(), ..c.clone() }, "t").is_err(), "{bad:?}");
        }
        assert!(matches!(
            DropboxProvider::new(DropboxConfig { content: "http://dropbox.example".into(), ..c.clone() }, "t"),
            Err(CloudError::Denied(_))
        ));
        assert!(DropboxProvider::new(c, "t").unwrap().with_chunk_size(1024).is_err());
        let c: DropboxConfig = serde_json::from_str(r#"{"client_id":"id","refresh_token":"r"}"#).unwrap();
        assert_eq!((c.root.as_str(), c.content.as_str()), ("", "https://content.dropboxapi.com"));
    }

    #[test]
    fn error_replies() {
        assert_eq!(dropbox_error("x", 409, br#"{"error_summary":"path/not_found/..","error":{".tag":"path"}}"#), CloudError::NotFound);
        assert_eq!(dropbox_error("x", 409, br#"{"error_summary":"path/insufficient_space/...","error":{}}"#), CloudError::QuotaExceeded);
        assert!(matches!(dropbox_error("x", 401, br#"{"error_summary":"invalid_access_token/"}"#), CloudError::Denied(_)));
        assert!(matches!(dropbox_error("x", 400, b"Error in call to API function"), CloudError::Io(m) if m == "x: HTTP 400"));
        let append = br#"{"error_summary":"incorrect_offset/..","error":{".tag":"incorrect_offset","correct_offset":8388608}}"#;
        assert_eq!(correct_offset(append), Some(8_388_608));
        let finish = br#"{"error":{".tag":"lookup_failed","lookup_failed":{".tag":"incorrect_offset","correct_offset":5}}}"#;
        assert_eq!(correct_offset(finish), Some(5));
        assert_eq!(correct_offset(br#"{"error":{".tag":"closed"}}"#), None);
        assert_eq!(correct_offset(b"junk"), None);
    }
}
