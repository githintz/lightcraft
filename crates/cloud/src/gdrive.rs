//! Google Drive (REST API v3). Objects are files in one app folder in My Drive (`LightCraft` by
//! default), each named by its full [`ObjectName`] (`blobs/ab/<id>.bin`: Drive allows `/` in
//! names, so no subfolders are needed); a [`RemoteRef`] is the Drive file id.
//!
//! Access is limited to the files LightCraft created ([`SCOPE`], `drive.file`): the rest of the
//! user's Drive is never visible. Sign-in is [`crate::oauth`] (PKCE + loopback redirect); the
//! refresh token is kept in the vault and an access token is fetched from it when needed.
//!
//! Uploads up to 5 MiB are one multipart request; larger ones use a resumable session sent in
//! chunks, which picks up where Google says it stopped after a dropped connection or a server
//! error. Rate limits (429, 403 `rateLimitExceeded`) and 5xx answers are retried with backoff.
//!
//! Drive doesn't keep names unique: two devices storing the same name at once can leave two
//! files. Both [`Provider::put`] and [`Provider::list`] then use the oldest one, so every device
//! agrees on which one counts (for blobs both hold the same bytes anyway).

use std::collections::BTreeMap;
use std::time::Duration;

use lightcraft_fetch::http::{Response, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::net::{MAX_META_BODY, Net, form, hex, is_local, retry_after};
use crate::oauth::{OAuthClient, TokenSource};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// Access to the files LightCraft creates, and nothing else in the user's Drive.
pub const SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
/// Drive's per-file limit.
pub const MAX_FILE_SIZE: u64 = 5 * 1024 * 1024 * 1024 * 1024;
/// Largest upload sent as one multipart request; larger ones use a resumable session.
pub const MULTIPART_MAX: usize = 5 * 1024 * 1024;
/// Default resumable chunk (Drive wants multiples of 256 KiB).
pub const DEFAULT_CHUNK: usize = 8 * 1024 * 1024;
const CHUNK_UNIT: usize = 256 * 1024;
/// Most files [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// Retries of a rate-limited or failed request (and of a stalled upload) before giving up.
pub const MAX_RETRIES: u32 = 5;

const FOLDER_MIME: &str = "application/vnd.google-apps.folder";
const OCTET: &str = "application/octet-stream";

fn default_folder() -> String {
    "LightCraft".into()
}
fn default_api() -> String {
    "https://www.googleapis.com".into()
}
fn default_token_url() -> String {
    "https://oauth2.googleapis.com/token".into()
}

/// One Google account, as stored in the vault. `Debug` never prints the secrets.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GoogleDriveConfig {
    pub client_id: String,
    /// Google issues one to desktop clients; it isn't secret there (RFC 8252 §8.5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    pub refresh_token: String,
    /// The app folder's name in My Drive.
    #[serde(default = "default_folder")]
    pub folder: String,
    /// API root; changed only by tests (a local fake server).
    #[serde(default = "default_api")]
    pub api: String,
    #[serde(default = "default_token_url")]
    pub token_url: String,
}

impl GoogleDriveConfig {
    /// An account signed in through [`crate::oauth::LoopbackFlow`] with `client`.
    pub fn new(client: &OAuthClient, refresh_token: impl Into<String>) -> GoogleDriveConfig {
        GoogleDriveConfig {
            client_id: client.client_id.clone(),
            client_secret: client.client_secret.clone(),
            refresh_token: refresh_token.into(),
            folder: default_folder(),
            api: default_api(),
            token_url: client.token_url.clone(),
        }
    }
}

impl std::fmt::Debug for GoogleDriveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GoogleDriveConfig")
            .field("client_id", &self.client_id)
            .field("folder", &self.folder)
            .field("api", &self.api)
            .finish_non_exhaustive()
    }
}

pub struct GoogleDriveProvider {
    tokens: TokenSource,
    folder_name: String,
    api: Url,
    label: String,
    pub net: Net,
    /// First wait before a retry; doubles each time.
    pub retry_base: Duration,
    chunk: usize,
    folder_id: Option<String>,
}

#[derive(Deserialize)]
struct FileId {
    id: String,
}

#[derive(Deserialize)]
struct FileList {
    #[serde(default)]
    files: Vec<DriveFile>,
    #[serde(default, rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct DriveFile {
    id: String,
    #[serde(default)]
    name: String,
    /// int64 as a decimal string
    #[serde(default)]
    size: Option<String>,
}

#[derive(Deserialize)]
struct About {
    #[serde(rename = "storageQuota")]
    storage_quota: StorageQuota,
}

#[derive(Deserialize)]
struct StorageQuota {
    /// Absent for an account without a limit.
    #[serde(default)]
    limit: Option<String>,
    #[serde(default)]
    usage: Option<String>,
}

#[derive(Deserialize)]
struct ErrorReply {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    message: String,
    #[serde(default)]
    errors: Vec<ErrorItem>,
}

#[derive(Deserialize)]
struct ErrorItem {
    #[serde(default)]
    reason: String,
}

/// A file id as Drive issues them; anything else (from the manifest, a listing, a reply) is
/// refused before it goes into a URL.
fn check_id(id: &str) -> Result<&str> {
    if !id.is_empty() && id.len() <= 256 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_') {
        Ok(id)
    } else {
        Err(CloudError::Corrupt(format!("not a Drive file id: {:?}", id.chars().take(64).collect::<String>())))
    }
}

/// A string literal inside a Drive search query.
fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// The last byte Google holds, from a `Range: bytes=0-N` header.
fn range_end(header: &str) -> Option<u64> {
    header.trim().strip_prefix("bytes=0-")?.trim().parse().ok()
}

/// A Drive error reply as a [`CloudError`].
fn drive_error(what: &str, status: u16, body: &[u8]) -> CloudError {
    let (message, reasons) = match serde_json::from_slice::<ErrorReply>(body) {
        Ok(r) => (
            r.error.message.chars().filter(|c| !c.is_control()).take(200).collect::<String>(),
            r.error.errors.into_iter().map(|e| e.reason).collect(),
        ),
        Err(_) => (String::new(), Vec::new()),
    };
    let detail = if message.is_empty() { format!("{what}: HTTP {status}") } else { format!("{what}: {message}") };
    if reasons.iter().any(|r| r == "storageQuotaExceeded") {
        return CloudError::QuotaExceeded;
    }
    match status {
        404 => CloudError::NotFound,
        401 | 403 => CloudError::Denied(detail),
        413 => CloudError::TooLarge { size: 0, max: 0 },
        _ => CloudError::Io(detail),
    }
}

fn is_rate_limit(body: &[u8]) -> bool {
    serde_json::from_slice::<ErrorReply>(body)
        .is_ok_and(|r| r.error.errors.iter().any(|e| e.reason == "rateLimitExceeded" || e.reason == "userRateLimitExceeded"))
}

impl GoogleDriveProvider {
    pub fn new(config: GoogleDriveConfig, label: impl Into<String>) -> Result<GoogleDriveProvider> {
        let api = Url::parse(config.api.trim_end_matches('/')).map_err(|e| CloudError::InvalidName(format!("Drive API URL: {e}")))?;
        if !api.tls && !is_local(&api.host) {
            return Err(CloudError::Denied("refusing to send Google credentials over plain http:// to another computer".into()));
        }
        if api.path.contains('?') {
            return Err(CloudError::InvalidName("the Drive API URL can't have a query".into()));
        }
        if config.client_id.is_empty() || config.refresh_token.is_empty() {
            return Err(CloudError::Denied("not signed in to Google (no client id or refresh token)".into()));
        }
        let folder = config.folder.trim();
        if folder.is_empty() || folder.chars().count() > 255 || folder.chars().any(char::is_control) {
            return Err(CloudError::InvalidName(format!("Drive folder name {:?}", config.folder.chars().take(64).collect::<String>())));
        }
        let mut client = OAuthClient::google_drive(config.client_id, config.client_secret);
        client.token_url = config.token_url;
        Ok(GoogleDriveProvider {
            tokens: TokenSource::new(client, config.refresh_token),
            folder_name: folder.to_string(),
            api,
            label: label.into(),
            net: Net::default(),
            retry_base: Duration::from_secs(1),
            chunk: DEFAULT_CHUNK,
            folder_id: None,
        })
    }

    /// Resumable uploads in chunks of `bytes` (a positive multiple of 256 KiB).
    pub fn with_chunk_size(mut self, bytes: usize) -> Result<GoogleDriveProvider> {
        if bytes == 0 || !bytes.is_multiple_of(CHUNK_UNIT) {
            return Err(CloudError::InvalidName(format!("Drive upload chunks must be a multiple of {CHUNK_UNIT} bytes, not {bytes}")));
        }
        self.chunk = bytes;
        Ok(self)
    }

    /// The refresh token in use: providers that rotate it hand out a new one on refresh, which
    /// the vault must then store.
    pub fn refresh_token(&self) -> &str {
        self.tokens.refresh_token()
    }

    /// The app folder's id, finding or creating the folder on first use.
    pub fn ensure_root(&mut self) -> Result<String> {
        if let Some(id) = &self.folder_id {
            return Ok(id.clone());
        }
        let q = format!("name = {} and mimeType = {} and 'root' in parents and trashed = false", quote(&self.folder_name), quote(FOLDER_MIME));
        let id = match self.first_match(&q)? {
            Some(id) => id,
            None => {
                let meta = serde_json::json!({ "name": self.folder_name, "mimeType": FOLDER_MIME, "parents": ["root"] });
                let url = self.url("/drive/v3/files?fields=id");
                let headers = [("Content-Type", "application/json; charset=UTF-8".to_string())];
                let mut resp = self.send("create folder", "POST", &url, &headers, meta.to_string().as_bytes())?;
                self.file_id_reply("create folder", &mut resp)?
            }
        };
        self.folder_id = Some(id.clone());
        Ok(id)
    }

    fn url(&self, path_and_query: &str) -> Url {
        Url { path: format!("{}{path_and_query}", self.api.path.trim_end_matches('/')), ..self.api.clone() }
    }

    fn backoff(&self, attempt: u32, resp: Option<&Response>) -> Result<()> {
        self.net.backoff(self.retry_base, attempt, resp.and_then(retry_after))
    }

    /// An authorized API request. A rejected access token is refreshed once; rate limits and
    /// 5xx answers are retried with backoff. Other answers are returned as they are, except a
    /// 403, which is read and returned as an error.
    fn send(&mut self, what: &str, method: &str, url: &Url, headers: &[(&'static str, String)], body: &[u8]) -> Result<Response> {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.bearer(&self.net)?;
            let mut h = headers.to_vec();
            h.push(("Authorization", format!("Bearer {token}")));
            let mut resp = self.net.send(method, url, &h, body)?;
            match resp.status {
                401 if !refreshed => {
                    self.tokens.invalidate();
                    refreshed = true;
                }
                429 | 500 | 502 | 503 | 504 if attempt < MAX_RETRIES => {
                    self.backoff(attempt, Some(&resp))?;
                    attempt += 1;
                }
                403 => {
                    let reply = self.net.body(&mut resp, 64 * 1024).unwrap_or_default();
                    if !(is_rate_limit(&reply) && attempt < MAX_RETRIES) {
                        return Err(drive_error(what, 403, &reply));
                    }
                    self.backoff(attempt, None)?;
                    attempt += 1;
                }
                _ => return Ok(resp),
            }
        }
    }

    fn error(&self, what: &str, resp: &mut Response) -> CloudError {
        let body = self.net.body(resp, 64 * 1024).unwrap_or_default();
        drive_error(what, resp.status, &body)
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, what: &str, resp: &mut Response) -> Result<T> {
        if resp.status != 200 {
            return Err(self.error(what, resp));
        }
        let body = self.net.body(resp, MAX_META_BODY)?;
        serde_json::from_slice(&body).map_err(|e| CloudError::Corrupt(format!("{what}: Drive reply: {e}")))
    }

    fn file_id_reply(&self, what: &str, resp: &mut Response) -> Result<String> {
        let f: FileId = self.json(what, resp)?;
        check_id(&f.id).map(str::to_string)
    }

    /// The oldest file matching a search query, if any.
    fn first_match(&mut self, q: &str) -> Result<Option<String>> {
        let query = form(&[("q", q), ("fields", "files(id)"), ("orderBy", "createdTime"), ("pageSize", "1"), ("spaces", "drive")]);
        let url = self.url(&format!("/drive/v3/files?{query}"));
        let mut resp = self.send("search", "GET", &url, &[], &[])?;
        let list: FileList = self.json("search", &mut resp)?;
        list.files.first().map(|f| check_id(&f.id).map(str::to_string)).transpose()
    }

    /// One request with the metadata and the bytes; `None` if the bytes happen to contain the
    /// part boundary (then the caller uses a resumable upload instead).
    fn upload_multipart(&mut self, method: &str, path: &str, meta: &serde_json::Value, data: &[u8]) -> Result<Option<String>> {
        let boundary = format!("lightcraft-{}", hex(Sha256::digest(data).get(..16).unwrap_or_default()));
        if data.windows(boundary.len()).any(|w| w == boundary.as_bytes()) {
            return Ok(None);
        }
        let mut body = Vec::with_capacity(data.len() + 512);
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: application/json; charset=UTF-8\r\n\r\n{meta}\r\n").as_bytes());
        body.extend_from_slice(format!("--{boundary}\r\nContent-Type: {OCTET}\r\n\r\n").as_bytes());
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        let url = self.url(&format!("{path}?uploadType=multipart&fields=id"));
        let headers = [("Content-Type", format!("multipart/related; boundary={boundary}"))];
        let mut resp = self.send("upload", method, &url, &headers, &body)?;
        self.file_id_reply("upload", &mut resp).map(Some)
    }

    /// A resumable upload session, sent in chunks; after a failure, asks Google how much it
    /// holds and goes on from there.
    fn upload_resumable(&mut self, method: &str, path: &str, meta: &serde_json::Value, data: &[u8]) -> Result<String> {
        let total = data.len() as u64;
        let url = self.url(&format!("{path}?uploadType=resumable&fields=id"));
        let headers = [
            ("Content-Type", "application/json; charset=UTF-8".to_string()),
            ("X-Upload-Content-Type", OCTET.to_string()),
            ("X-Upload-Content-Length", total.to_string()),
        ];
        let mut resp = self.send("start upload", method, &url, &headers, meta.to_string().as_bytes())?;
        if resp.status != 200 {
            return Err(self.error("start upload", &mut resp));
        }
        let location = resp.header("Location").ok_or_else(|| CloudError::Corrupt("upload session without an address".into()))?.to_string();
        drop(resp);
        let session = Url::parse(&location).map_err(|e| CloudError::Corrupt(format!("upload session address: {e}")))?;
        // the session address carries the right to write; it must not point anywhere else
        if session.tls != self.api.tls || session.host != self.api.host || session.port != self.api.port {
            return Err(CloudError::Corrupt(format!("upload session on another server: {}", session.display())));
        }

        let mut offset = 0u64;
        let mut failures = 0u32;
        let mut ask = false;
        // never more than a few requests per chunk, whatever the server answers
        let max_requests = (total / self.chunk as u64).saturating_add(1).saturating_mul(8).saturating_add(64);
        let mut requests = 0u64;
        loop {
            requests += 1;
            if requests > max_requests {
                return Err(CloudError::Io("the upload stopped making progress".into()));
            }
            let sent = if ask || offset >= total {
                self.net.send("PUT", &session, &[("Content-Range", format!("bytes */{total}"))], &[])
            } else {
                let end = offset.saturating_add(self.chunk as u64).min(total);
                let part = usize::try_from(offset).ok().zip(usize::try_from(end).ok()).and_then(|(a, b)| data.get(a..b));
                let part = part.ok_or_else(|| CloudError::Corrupt("upload offset outside the data".into()))?;
                self.net.send("PUT", &session, &[("Content-Range", format!("bytes {offset}-{}/{total}", end - 1))], part)
            };
            let was_ask = ask || offset >= total;
            ask = false;
            let mut resp = match sent {
                Ok(r) => r,
                Err(e) => {
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(e);
                    }
                    self.backoff(failures - 1, None)?;
                    ask = true;
                    continue;
                }
            };
            match resp.status {
                200 | 201 => return self.file_id_reply("upload", &mut resp),
                308 => {
                    let next = match resp.header("Range") {
                        Some(r) => {
                            range_end(r).and_then(|n| n.checked_add(1)).ok_or_else(|| CloudError::Corrupt(format!("upload Range header {r:?}")))?
                        }
                        None => 0,
                    };
                    if next > total {
                        return Err(CloudError::Corrupt(format!("Google holds {next} bytes of a {total}-byte upload")));
                    }
                    if next > offset {
                        failures = 0;
                    } else if !was_ask || next >= total {
                        // a chunk that moved nothing, or everything sent and still not finished
                        failures += 1;
                        if failures > MAX_RETRIES {
                            return Err(CloudError::Io("the upload stopped making progress".into()));
                        }
                        self.backoff(failures - 1, None)?;
                    }
                    offset = next;
                }
                404 | 410 => return Err(CloudError::Io("the upload session expired; try again".into())),
                429 | 500 | 502 | 503 | 504 => {
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(self.error("upload", &mut resp));
                    }
                    self.backoff(failures - 1, Some(&resp))?;
                    ask = true;
                }
                _ => return Err(self.error("upload", &mut resp)),
            }
        }
    }
}

impl Provider for GoogleDriveProvider {
    fn refresh_token(&self) -> Option<String> {
        Some(self.tokens.refresh_token().to_string())
    }

    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "gdrive".into(), label: self.label.clone(), max_file_size: Some(MAX_FILE_SIZE) }
    }

    /// The whole account's storage (Drive, Gmail and Photos share it); no limit for an account
    /// that has none.
    fn quota(&mut self) -> Result<Quota> {
        let url = self.url("/drive/v3/about?fields=storageQuota");
        let mut resp = self.send("storage quota", "GET", &url, &[], &[])?;
        let about: About = self.json("storage quota", &mut resp)?;
        let num = |v: Option<String>| v.and_then(|s| s.trim().parse::<u64>().ok());
        Ok(Quota { total: num(about.storage_quota.limit), used: num(about.storage_quota.usage).unwrap_or(0) })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        if data.len() as u64 > MAX_FILE_SIZE {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: MAX_FILE_SIZE });
        }
        let folder = self.ensure_root()?;
        let existing = self.first_match(&format!("name = {} and {} in parents and trashed = false", quote(name.as_str()), quote(&folder)))?;
        let (method, path, meta) = match &existing {
            // new content for the oldest file of that name: it keeps its id
            Some(id) => ("PATCH", format!("/upload/drive/v3/files/{id}"), serde_json::json!({})),
            None => {
                ("POST", "/upload/drive/v3/files".to_string(), serde_json::json!({ "name": name.as_str(), "parents": [folder], "mimeType": OCTET }))
            }
        };
        let small = if data.len() <= MULTIPART_MAX { self.upload_multipart(method, &path, &meta, data)? } else { None };
        let id = match small {
            Some(id) => id,
            None => self.upload_resumable(method, &path, &meta, data)?,
        };
        Ok(RemoteRef { key: check_id(&id)?.to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let id = check_id(&remote.key)?;
        let url = self.url(&format!("/drive/v3/files/{id}?alt=media"));
        let mut resp = self.send("download", "GET", &url, &[], &[])?;
        if resp.status != 200 {
            return Err(self.error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let id = check_id(&remote.key)?;
        let url = self.url(&format!("/drive/v3/files/{id}"));
        let mut resp = self.send("delete", "DELETE", &url, &[], &[])?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.error("delete", &mut resp)),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let folder = self.ensure_root()?;
        let q = format!("{} in parents and trashed = false", quote(&folder));
        // oldest first, so the first file seen for a name is the one put() also updates
        let mut out: BTreeMap<ObjectName, Listed> = BTreeMap::new();
        let mut page: Option<String> = None;
        let mut pages = 0usize;
        loop {
            pages += 1;
            if pages > MAX_LISTED / 100 {
                return Err(CloudError::Io("too many listing pages from Google Drive".into()));
            }
            let mut params = vec![
                ("q", q.as_str()),
                ("fields", "nextPageToken,files(id,name,size)"),
                ("orderBy", "createdTime"),
                ("pageSize", "1000"),
                ("spaces", "drive"),
            ];
            if let Some(t) = &page {
                params.push(("pageToken", t));
            }
            let url = self.url(&format!("/drive/v3/files?{}", form(&params)));
            let mut resp = self.send("list", "GET", &url, &[], &[])?;
            let list: FileList = self.json("list", &mut resp)?;
            for f in list.files {
                let (Ok(name), Ok(id)) = (ObjectName::parse(&f.name), check_id(&f.id)) else { continue };
                let size = f.size.and_then(|s| s.trim().parse().ok()).unwrap_or(0);
                let remote = RemoteRef { key: id.to_string() };
                out.entry(name.clone()).or_insert(Listed { name, remote, size });
                if out.len() > MAX_LISTED {
                    return Err(CloudError::Io(format!("more than {MAX_LISTED} files in the Drive folder {}", self.folder_name)));
                }
            }
            match list.next_page_token {
                Some(t) if !t.is_empty() && t.len() <= 4096 => page = Some(t),
                _ => return Ok(out.into_values().collect()),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> GoogleDriveConfig {
        let client = OAuthClient::google_drive("id", Some("secret".into()));
        GoogleDriveConfig::new(&client, "refresh-me")
    }

    #[test]
    fn config_is_checked_and_secrets_are_not_printed() {
        let c = config();
        let shown = format!("{c:?}");
        assert!(!shown.contains("secret") && !shown.contains("refresh-me"), "{shown}");
        assert!(GoogleDriveProvider::new(c.clone(), "t").is_ok());
        assert!(matches!(
            GoogleDriveProvider::new(GoogleDriveConfig { api: "http://drive.example".into(), ..c.clone() }, "t"),
            Err(CloudError::Denied(_))
        ));
        assert!(GoogleDriveProvider::new(GoogleDriveConfig { api: "http://127.0.0.1:9".into(), ..c.clone() }, "t").is_ok());
        assert!(GoogleDriveProvider::new(GoogleDriveConfig { refresh_token: String::new(), ..c.clone() }, "t").is_err());
        assert!(GoogleDriveProvider::new(GoogleDriveConfig { folder: " ".into(), ..c.clone() }, "t").is_err());
        assert!(GoogleDriveProvider::new(GoogleDriveConfig { folder: "a\nb".into(), ..c.clone() }, "t").is_err());
        let p = GoogleDriveProvider::new(c, "t").unwrap();
        assert!(p.with_chunk_size(1000).is_err());
        // the vault's JSON form round-trips, and old entries get the defaults
        let json = r#"{"client_id":"id","refresh_token":"r"}"#;
        let c: GoogleDriveConfig = serde_json::from_str(json).unwrap();
        assert_eq!((c.folder.as_str(), c.api.as_str()), ("LightCraft", "https://www.googleapis.com"));
    }

    #[test]
    fn ids_queries_and_ranges() {
        assert!(check_id("1AbC_d-9").is_ok());
        for bad in ["", "a/b", "a?b", "a b", "../x", &"x".repeat(257)] {
            assert!(check_id(bad).is_err(), "{bad:?}");
        }
        assert_eq!(quote(r"it's a \ test"), r"'it\'s a \\ test'");
        assert_eq!(range_end("bytes=0-524287"), Some(524_287));
        assert_eq!(range_end("bytes=5-9"), None);
        assert_eq!(range_end("junk"), None);
    }

    #[test]
    fn error_replies() {
        let quota =
            br#"{"error":{"code":403,"message":"The user's Drive storage quota has been exceeded.","errors":[{"reason":"storageQuotaExceeded"}]}}"#;
        assert_eq!(drive_error("upload", 403, quota), CloudError::QuotaExceeded);
        let rate = br#"{"error":{"code":403,"message":"Rate Limit Exceeded","errors":[{"reason":"userRateLimitExceeded"}]}}"#;
        assert!(is_rate_limit(rate));
        assert!(!is_rate_limit(quota));
        assert!(matches!(drive_error("x", 403, b"<html>"), CloudError::Denied(m) if m == "x: HTTP 403"));
        assert_eq!(drive_error("x", 404, b""), CloudError::NotFound);
        assert!(matches!(drive_error("x", 400, br#"{"error":{"message":"Invalid query"}}"#), CloudError::Io(m) if m == "x: Invalid query"));
    }
}
