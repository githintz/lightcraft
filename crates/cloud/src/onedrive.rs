//! OneDrive (Microsoft Graph v1.0), personal or work/school. Objects are files under the app's
//! own folder (`Apps/<app name>` in the user's OneDrive, Graph's `special/approot`), addressed by
//! path (`blobs/ab/<id>.bin`; folders appear as needed); a [`RemoteRef`] is that path.
//!
//! Access is limited to that folder ([`SCOPES`]: `Files.ReadWrite.AppFolder`): the rest of the
//! user's OneDrive is never visible. Sign-in is [`crate::oauth`] with
//! [`OAuthClient::onedrive`] (a public client, no secret).
//!
//! Uploads up to 4 MiB are one request; larger ones use an upload session sent in chunks, which
//! asks where to go on after a dropped connection or a server error. Downloads follow Graph's
//! redirect to a pre-authenticated address (sent no credentials). Rate limits (429) and 5xx
//! answers are retried, honouring `Retry-After`.

use std::collections::BTreeMap;
use std::time::Duration;

use lightcraft_fetch::http::{Response, Url};
use serde::{Deserialize, Serialize};

use crate::blob::MAX_DEPTH;
use crate::net::{MAX_META_BODY, Net, is_local, retry_after, uri_encode};
use crate::oauth::{OAuthClient, TokenSource};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// The app's own folder, and a refresh token to keep using it.
pub const SCOPES: &[&str] = &["Files.ReadWrite.AppFolder", "offline_access"];
/// OneDrive's per-file limit.
pub const MAX_FILE_SIZE: u64 = 250 * 1024 * 1024 * 1024;
/// Largest upload sent as one request; larger ones use an upload session.
pub const SIMPLE_MAX: usize = 4 * 1024 * 1024;
/// Upload session chunks are multiples of this.
pub const CHUNK_UNIT: usize = 320 * 1024;
/// Default upload session chunk (Graph allows up to 60 MiB).
pub const DEFAULT_CHUNK: usize = 32 * CHUNK_UNIT;
/// Most files [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// Most folders [`Provider::list`] visits.
pub const MAX_FOLDERS: usize = 100_000;
/// Retries of a rate-limited or failed request (and of a stalled upload) before giving up.
pub const MAX_RETRIES: u32 = 5;

fn default_api() -> String {
    "https://graph.microsoft.com/v1.0".into()
}
fn default_token_url() -> String {
    "https://login.microsoftonline.com/common/oauth2/v2.0/token".into()
}

/// One Microsoft account, as stored in the vault. `Debug` never prints the refresh token.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OneDriveConfig {
    pub client_id: String,
    pub refresh_token: String,
    /// Graph's root; changed only by tests (a local fake server).
    #[serde(default = "default_api")]
    pub api: String,
    #[serde(default = "default_token_url")]
    pub token_url: String,
}

impl OneDriveConfig {
    /// An account signed in through [`crate::oauth::LoopbackFlow`] with `client`.
    pub fn new(client: &OAuthClient, refresh_token: impl Into<String>) -> OneDriveConfig {
        OneDriveConfig {
            client_id: client.client_id.clone(),
            refresh_token: refresh_token.into(),
            api: default_api(),
            token_url: client.token_url.clone(),
        }
    }
}

impl std::fmt::Debug for OneDriveConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OneDriveConfig").field("client_id", &self.client_id).field("api", &self.api).finish_non_exhaustive()
    }
}

pub struct OneDriveProvider {
    tokens: TokenSource,
    api: Url,
    label: String,
    pub net: Net,
    /// First wait before a retry; doubles each time.
    pub retry_base: Duration,
    chunk: usize,
}

#[derive(Deserialize)]
struct Children {
    #[serde(default)]
    value: Vec<Item>,
    #[serde(default, rename = "@odata.nextLink")]
    next_link: Option<String>,
}

#[derive(Deserialize)]
struct Item {
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    folder: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct Drive {
    #[serde(default)]
    quota: Option<DriveQuota>,
}

#[derive(Deserialize)]
struct DriveQuota {
    #[serde(default)]
    total: Option<u64>,
    #[serde(default)]
    used: Option<u64>,
}

#[derive(Deserialize)]
struct Session {
    #[serde(rename = "uploadUrl")]
    upload_url: String,
}

#[derive(Deserialize)]
struct SessionStatus {
    #[serde(default, rename = "nextExpectedRanges")]
    next_expected_ranges: Vec<String>,
}

#[derive(Deserialize)]
struct ErrorReply {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    #[serde(default)]
    code: String,
    #[serde(default)]
    message: String,
}

/// A Graph error reply as a [`CloudError`].
fn graph_error(what: &str, status: u16, body: &[u8]) -> CloudError {
    let (code, message) = match serde_json::from_slice::<ErrorReply>(body) {
        Ok(r) => (r.error.code, r.error.message.chars().filter(|c| !c.is_control()).take(200).collect::<String>()),
        Err(_) => (String::new(), String::new()),
    };
    if code == "quotaLimitReached" || status == 507 {
        return CloudError::QuotaExceeded;
    }
    let detail = if message.is_empty() { format!("{what}: HTTP {status}") } else { format!("{what}: {message}") };
    match status {
        404 => CloudError::NotFound,
        401 | 403 => CloudError::Denied(detail),
        413 => CloudError::TooLarge { size: 0, max: 0 },
        _ => CloudError::Io(detail),
    }
}

/// The first byte the server still wants, from `nextExpectedRanges` (`["26-"]`, `["26-99"]`).
fn next_expected(ranges: &[String]) -> Option<u64> {
    ranges.first()?.split('-').next()?.trim().parse().ok()
}

impl OneDriveProvider {
    pub fn new(config: OneDriveConfig, label: impl Into<String>) -> Result<OneDriveProvider> {
        let api = Url::parse(config.api.trim_end_matches('/')).map_err(|e| CloudError::InvalidName(format!("Graph API URL: {e}")))?;
        if !api.tls && !is_local(&api.host) {
            return Err(CloudError::Denied("refusing to send Microsoft credentials over plain http:// to another computer".into()));
        }
        if api.path.contains('?') {
            return Err(CloudError::InvalidName("the Graph API URL can't have a query".into()));
        }
        if config.client_id.is_empty() || config.refresh_token.is_empty() {
            return Err(CloudError::Denied("not signed in to Microsoft (no client id or refresh token)".into()));
        }
        let mut client = OAuthClient::onedrive(config.client_id);
        client.token_url = config.token_url;
        Ok(OneDriveProvider {
            tokens: TokenSource::new(client, config.refresh_token),
            api,
            label: label.into(),
            net: Net::default(),
            retry_base: Duration::from_secs(1),
            chunk: DEFAULT_CHUNK,
        })
    }

    /// Upload sessions in chunks of `bytes` (a positive multiple of 320 KiB, at most 60 MiB).
    pub fn with_chunk_size(mut self, bytes: usize) -> Result<OneDriveProvider> {
        if bytes == 0 || !bytes.is_multiple_of(CHUNK_UNIT) || bytes > 60 * 1024 * 1024 {
            return Err(CloudError::InvalidName(format!(
                "OneDrive upload chunks must be a multiple of {CHUNK_UNIT} bytes up to 60 MiB, not {bytes}"
            )));
        }
        self.chunk = bytes;
        Ok(self)
    }

    /// The refresh token in use (Microsoft rotates it: the vault must store the latest).
    pub fn refresh_token(&self) -> &str {
        self.tokens.refresh_token()
    }

    fn url(&self, path_and_query: &str) -> Url {
        Url { path: format!("{}{path_and_query}", self.api.path.trim_end_matches('/')), ..self.api.clone() }
    }

    /// `/me/drive/special/approot:/<path>:<suffix>` (or the folder itself for an empty path).
    fn item_url(&self, path: &str, suffix: &str) -> Url {
        if path.is_empty() {
            return self.url(&format!("/me/drive/special/approot{suffix}"));
        }
        let path = uri_encode(path, true);
        if suffix.is_empty() {
            // the item itself: no closing colon
            return self.url(&format!("/me/drive/special/approot:/{path}"));
        }
        self.url(&format!("/me/drive/special/approot:/{path}:{suffix}"))
    }

    /// An address the API handed out (a next page, an upload session, a download): https, or
    /// this computer when the API itself is local (tests).
    fn check_foreign(&self, what: &str, s: &str) -> Result<Url> {
        let url = Url::parse(s).map_err(|e| CloudError::Corrupt(format!("{what} address: {e}")))?;
        if url.tls || (is_local(&url.host) && !self.api.tls) {
            Ok(url)
        } else {
            Err(CloudError::Corrupt(format!("{what} address isn't https: {}", url.display())))
        }
    }

    /// An authorized API request: a rejected access token is refreshed once; 429 and 5xx answers
    /// are retried with backoff. Other answers are returned as they are.
    fn send(&mut self, method: &str, url: &Url, headers: &[(&'static str, String)], body: &[u8]) -> Result<Response> {
        let mut refreshed = false;
        let mut attempt = 0u32;
        loop {
            let token = self.tokens.bearer(&self.net)?;
            let mut h = headers.to_vec();
            h.push(("Authorization", format!("Bearer {token}")));
            let resp = self.net.send(method, url, &h, body)?;
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

    fn error(&self, what: &str, resp: &mut Response) -> CloudError {
        let body = self.net.body(resp, 64 * 1024).unwrap_or_default();
        graph_error(what, resp.status, &body)
    }

    fn json<T: for<'de> Deserialize<'de>>(&self, what: &str, resp: &mut Response) -> Result<T> {
        if !(200..300).contains(&resp.status) {
            return Err(self.error(what, resp));
        }
        let body = self.net.body(resp, MAX_META_BODY)?;
        serde_json::from_slice(&body).map_err(|e| CloudError::Corrupt(format!("{what}: Graph reply: {e}")))
    }

    /// An upload session, sent in chunks; after a failure, asks the session what it still needs
    /// and goes on from there.
    fn upload_session(&mut self, name: &ObjectName, data: &[u8]) -> Result<()> {
        let total = data.len() as u64;
        let url = self.item_url(name.as_str(), "/createUploadSession");
        let body = serde_json::json!({ "item": { "@microsoft.graph.conflictBehavior": "replace" } }).to_string();
        let mut resp = self.send("POST", &url, &[("Content-Type", "application/json".to_string())], body.as_bytes())?;
        let session: Session = self.json("start upload", &mut resp)?;
        let session = self.check_foreign("upload session", &session.upload_url)?;

        let mut offset = 0u64;
        let mut failures = 0u32;
        let mut ask = false;
        let max_requests = (total / self.chunk as u64).saturating_add(1).saturating_mul(8).saturating_add(64);
        let mut requests = 0u64;
        loop {
            requests += 1;
            if requests > max_requests {
                return Err(CloudError::Io("the upload stopped making progress".into()));
            }
            // the session address carries the right to write: it is sent no credentials
            let sent = if ask || offset >= total {
                self.net.send("GET", &session, &[], &[])
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
                    self.net.backoff(self.retry_base, failures - 1, None)?;
                    ask = true;
                    continue;
                }
            };
            match resp.status {
                200 | 201 if !was_ask => return Ok(()),
                // 202 after a chunk; 200 with the ranges after a status request
                200 | 202 => {
                    let status: SessionStatus = self.json("upload", &mut resp)?;
                    let Some(next) = next_expected(&status.next_expected_ranges) else {
                        // a status reply without ranges: everything arrived and the file is made
                        return Ok(());
                    };
                    if next > total {
                        return Err(CloudError::Corrupt(format!("OneDrive expects byte {next} of a {total}-byte upload")));
                    }
                    if next > offset {
                        failures = 0;
                    } else if !was_ask || next >= total {
                        failures += 1;
                        if failures > MAX_RETRIES {
                            return Err(CloudError::Io("the upload stopped making progress".into()));
                        }
                        self.net.backoff(self.retry_base, failures - 1, None)?;
                    }
                    offset = next;
                }
                201 => return Ok(()),
                404 | 410 => return Err(CloudError::Io("the upload session expired; try again".into())),
                // the server holds other bytes than we thought: ask
                416 => ask = true,
                429 | 500 | 502 | 503 | 504 => {
                    failures += 1;
                    if failures > MAX_RETRIES {
                        return Err(self.error("upload", &mut resp));
                    }
                    self.net.backoff(self.retry_base, failures - 1, retry_after(&resp))?;
                    ask = true;
                }
                _ => return Err(self.error("upload", &mut resp)),
            }
        }
    }
}

impl Provider for OneDriveProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "onedrive".into(), label: self.label.clone(), max_file_size: Some(MAX_FILE_SIZE) }
    }

    /// The whole OneDrive's storage (shared with Outlook attachments on some plans).
    fn quota(&mut self) -> Result<Quota> {
        let url = self.url("/me/drive?$select=quota");
        let mut resp = self.send("GET", &url, &[], &[])?;
        let drive: Drive = self.json("storage quota", &mut resp)?;
        let q = drive.quota.unwrap_or(DriveQuota { total: None, used: None });
        Ok(Quota { total: q.total.filter(|t| *t > 0), used: q.used.unwrap_or(0) })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        if data.len() as u64 > MAX_FILE_SIZE {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: MAX_FILE_SIZE });
        }
        if data.len() <= SIMPLE_MAX {
            let url = self.item_url(name.as_str(), "/content?@microsoft.graph.conflictBehavior=replace");
            let mut resp = self.send("PUT", &url, &[("Content-Type", "application/octet-stream".to_string())], data)?;
            if !matches!(resp.status, 200 | 201) {
                return Err(self.error("upload", &mut resp));
            }
        } else {
            self.upload_session(name, data)?;
        }
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let name = ObjectName::parse(&remote.key)?;
        let url = self.item_url(name.as_str(), "/content");
        let mut resp = self.send("GET", &url, &[], &[])?;
        if matches!(resp.status, 301 | 302 | 303 | 307 | 308) {
            let location = resp.header("Location").ok_or_else(|| CloudError::Corrupt("download redirect without an address".into()))?.to_string();
            let target = self.check_foreign("download", &location)?;
            // a pre-authenticated address: no credentials go with it
            resp = self.net.send("GET", &target, &[], &[])?;
        }
        if resp.status != 200 {
            return Err(self.error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let name = ObjectName::parse(&remote.key)?;
        let url = self.item_url(name.as_str(), "");
        let mut resp = self.send("DELETE", &url, &[], &[])?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.error("delete", &mut resp)),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let mut out: BTreeMap<ObjectName, Listed> = BTreeMap::new();
        let mut queue: Vec<(String, usize)> = vec![(String::new(), 0)];
        let mut folders = 0usize;
        while let Some((dir, depth)) = queue.pop() {
            folders += 1;
            if folders > MAX_FOLDERS {
                return Err(CloudError::Io(format!("more than {MAX_FOLDERS} folders in the OneDrive app folder")));
            }
            let mut next = Some(self.item_url(&dir, "/children?$select=name,size,folder&$top=1000"));
            let mut pages = 0usize;
            while let Some(url) = next.take() {
                pages += 1;
                if pages > MAX_LISTED / 100 {
                    return Err(CloudError::Io("too many listing pages from OneDrive".into()));
                }
                let mut resp = self.send("GET", &url, &[], &[])?;
                let page: Children = self.json("list", &mut resp)?;
                for item in page.value {
                    let path = if dir.is_empty() { item.name.clone() } else { format!("{dir}/{}", item.name) };
                    if item.name.contains('/') {
                        continue;
                    }
                    if item.folder.is_some() {
                        // a folder deeper than any object name can be is not ours
                        if depth + 1 < MAX_DEPTH && ObjectName::parse(&path).is_ok() {
                            queue.push((path, depth + 1));
                        }
                        continue;
                    }
                    let Ok(name) = ObjectName::parse(&path) else { continue };
                    if out.len() >= MAX_LISTED {
                        return Err(CloudError::Io(format!("more than {MAX_LISTED} files in the OneDrive app folder")));
                    }
                    let remote = RemoteRef { key: path };
                    out.insert(name.clone(), Listed { name, remote, size: item.size.unwrap_or(0) });
                }
                if let Some(link) = page.next_link {
                    let link = self.check_foreign("next page", &link)?;
                    // a next page comes from the API itself, never another server
                    if link.host != self.api.host || link.port != self.api.port || link.tls != self.api.tls {
                        return Err(CloudError::Corrupt(format!("next page on another server: {}", link.display())));
                    }
                    next = Some(link);
                }
            }
        }
        Ok(out.into_values().collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> OneDriveConfig {
        OneDriveConfig::new(&OAuthClient::onedrive("cid"), "refresh-me")
    }

    #[test]
    fn config_is_checked_and_secrets_are_not_printed() {
        let c = config();
        assert!(!format!("{c:?}").contains("refresh-me"));
        assert!(OneDriveProvider::new(c.clone(), "t").is_ok());
        assert!(matches!(
            OneDriveProvider::new(OneDriveConfig { api: "http://graph.example/v1.0".into(), ..c.clone() }, "t"),
            Err(CloudError::Denied(_))
        ));
        assert!(OneDriveProvider::new(OneDriveConfig { refresh_token: String::new(), ..c.clone() }, "t").is_err());
        let p = OneDriveProvider::new(c, "t").unwrap();
        assert!(p.with_chunk_size(1024 * 1024).is_err());
        let c: OneDriveConfig = serde_json::from_str(r#"{"client_id":"id","refresh_token":"r"}"#).unwrap();
        assert_eq!(c.api, "https://graph.microsoft.com/v1.0");
    }

    #[test]
    fn addresses() {
        let p = OneDriveProvider::new(config(), "t").unwrap();
        assert_eq!(p.item_url("blobs/ab/x.bin", "/content").path, "/v1.0/me/drive/special/approot:/blobs/ab/x.bin:/content");
        assert_eq!(p.item_url("", "/children").path, "/v1.0/me/drive/special/approot/children");
        assert_eq!(p.item_url("manifest/a", "").path, "/v1.0/me/drive/special/approot:/manifest/a");
        assert!(p.check_foreign("x", "https://my.sharepoint.com/up?x=1").is_ok());
        assert!(p.check_foreign("x", "http://127.0.0.1:9/up").is_err(), "plain http only when the API itself is local");
        assert_eq!(next_expected(&["26-".into()]), Some(26));
        assert_eq!(next_expected(&["5-9".into(), "20-".into()]), Some(5));
        assert_eq!(next_expected(&[]), None);
        assert_eq!(next_expected(&["junk".into()]), None);
    }

    #[test]
    fn error_replies() {
        assert_eq!(graph_error("upload", 507, b""), CloudError::QuotaExceeded);
        assert_eq!(graph_error("upload", 403, br#"{"error":{"code":"quotaLimitReached","message":"full"}}"#), CloudError::QuotaExceeded);
        assert_eq!(graph_error("x", 404, br#"{"error":{"code":"itemNotFound","message":"gone"}}"#), CloudError::NotFound);
        assert!(matches!(graph_error("x", 403, br#"{"error":{"code":"accessDenied","message":"no"}}"#), CloudError::Denied(m) if m == "x: no"));
    }
}
