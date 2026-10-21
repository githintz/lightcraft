//! WebDAV storage (RFC 4918): Nextcloud, ownCloud, Koofr, a NAS, Apache `mod_dav`, rclone… Objects
//! are files under one collection URL; folders are created as needed.

use std::collections::BTreeSet;

use lightcraft_fetch::http::Url;
use serde::{Deserialize, Serialize};

use crate::blob::MAX_DEPTH;
use crate::net::{MAX_META_BODY, Net, base64, is_local, uri_decode, xml_elements};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// Most files [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// Most collections [`Provider::list`] visits.
pub const MAX_FOLDERS: usize = 100_000;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebDavConfig {
    /// The app folder's collection, ending in `/`, e.g.
    /// `https://cloud.example.com/remote.php/dav/files/me/LightCraft/`.
    pub url: String,
    pub user: String,
    pub password: String,
}

impl std::fmt::Debug for WebDavConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebDavConfig").field("url", &self.url).field("user", &self.user).finish_non_exhaustive()
    }
}

pub struct WebDavProvider {
    base: Url,
    auth: Option<String>,
    label: String,
    pub net: Net,
    /// Collections known to exist (created or seen), relative to the base.
    known_dirs: BTreeSet<String>,
}

const PROPFIND_LIST: &str =
    r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:resourcetype/><D:getcontentlength/></D:prop></D:propfind>"#;
const PROPFIND_QUOTA: &str = r#"<?xml version="1.0" encoding="utf-8"?><D:propfind xmlns:D="DAV:"><D:prop><D:quota-available-bytes/><D:quota-used-bytes/></D:prop></D:propfind>"#;

impl WebDavProvider {
    pub fn new(config: WebDavConfig, label: impl Into<String>) -> Result<WebDavProvider> {
        let base = Url::parse(&config.url).map_err(|e| CloudError::InvalidName(format!("WebDAV URL: {e}")))?;
        if !base.path.ends_with('/') || base.path.contains('?') {
            return Err(CloudError::InvalidName("the WebDAV URL must be a folder ending in / with no query".into()));
        }
        if !base.tls && !config.password.is_empty() && !is_local(&base.host) {
            return Err(CloudError::Denied("refusing to send a password over plain http:// to another computer; use https://".into()));
        }
        let auth = (!config.user.is_empty()).then(|| format!("Basic {}", base64(format!("{}:{}", config.user, config.password).as_bytes())));
        Ok(WebDavProvider { base, auth, label: label.into(), net: Net::default(), known_dirs: BTreeSet::new() })
    }

    /// Create the app folder's collection if it doesn't exist (its parent must).
    pub fn ensure_root(&mut self) -> Result<()> {
        self.mkcol("")
    }

    fn url(&self, rel: &str) -> Url {
        Url { path: format!("{}{rel}", self.base.path), ..self.base.clone() }
    }

    fn headers(&self, extra: &[(&'static str, String)]) -> Vec<(&'static str, String)> {
        let mut h: Vec<(&'static str, String)> = extra.to_vec();
        if let Some(a) = &self.auth {
            h.push(("Authorization", a.clone()));
        }
        h
    }

    /// Create collection `rel` (`""` = the base, else `a/b/`); one that exists is fine.
    fn mkcol(&mut self, rel: &str) -> Result<()> {
        if self.known_dirs.contains(rel) {
            return Ok(());
        }
        let headers = self.headers(&[]);
        let mut resp = self.net.send("MKCOL", &self.url(rel), &headers, &[])?;
        match resp.status {
            // 405: already exists; some servers answer 301 for an existing collection
            201 | 200 | 204 | 301 | 405 => {
                self.known_dirs.insert(rel.to_string());
                Ok(())
            }
            409 => Err(CloudError::NotFound),
            _ => Err(self.net.status_error("create folder", &mut resp)),
        }
    }

    /// The object name relative to the base for an `href` from a PROPFIND reply, if it is inside
    /// the base (hrefs may be paths or full URLs, percent-encoded).
    fn relative(&self, href: &str) -> Option<String> {
        let path = match href.find("://") {
            Some(i) => {
                let after = href.get(i + 3..)?;
                after.get(after.find('/')?..)?
            }
            None => href,
        };
        let path = uri_decode(path)?;
        let base = uri_decode(&self.base.path)?;
        path.strip_prefix(&base).map(str::to_string)
    }

    fn propfind(&self, rel: &str, depth: &str, body: &str) -> Result<Vec<u8>> {
        let headers = self.headers(&[("Depth", depth.to_string()), ("Content-Type", "application/xml; charset=utf-8".to_string())]);
        let mut resp = self.net.send("PROPFIND", &self.url(rel), &headers, body.as_bytes())?;
        if resp.status != 207 {
            return Err(self.net.status_error("list", &mut resp));
        }
        self.net.body(&mut resp, MAX_META_BODY)
    }
}

/// Split a multistatus reply into its `<response>` elements' raw XML.
fn responses(xml: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut rest = xml;
    while let Some(start) = find_tag(rest, b"response", false) {
        let Some(after) = rest.get(start..) else { break };
        let Some(end) = find_tag(after, b"response", true) else { break };
        out.push(after.get(..end).unwrap_or_default());
        rest = after.get(end..).unwrap_or_default();
    }
    out
}

/// Index of the next `<x:name` (or `</x:name` when `close`), with any namespace prefix.
fn find_tag(xml: &[u8], name: &[u8], close: bool) -> Option<usize> {
    let mut i = 0;
    while i < xml.len() {
        let lt = xml.get(i..)?.iter().position(|b| *b == b'<')? + i;
        let mut j = lt + 1;
        if close {
            if xml.get(j) != Some(&b'/') {
                i = lt + 1;
                continue;
            }
            j += 1;
        } else if xml.get(j) == Some(&b'/') {
            i = lt + 1;
            continue;
        }
        let end = xml.get(j..)?.iter().position(|b| matches!(b, b'>' | b' ' | b'/' | b'\t' | b'\r' | b'\n')).map(|p| p + j)?;
        let tag = xml.get(j..end)?;
        let local = tag.rsplit(|b| *b == b':').next().unwrap_or(tag);
        if local == name {
            return Some(lt);
        }
        i = lt + 1;
    }
    None
}

impl Provider for WebDavProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "webdav".into(), label: self.label.clone(), max_file_size: None }
    }

    /// The server's quota (RFC 4331) when it reports one; else unknown, with `used` counting this
    /// app folder's files.
    fn quota(&mut self) -> Result<Quota> {
        let xml = self.propfind("", "0", PROPFIND_QUOTA)?;
        let num = |name: &str| -> Result<Option<u64>> { Ok(xml_elements(&xml, name)?.first().and_then(|v| v.trim().parse::<u64>().ok())) };
        match (num("quota-available-bytes")?, num("quota-used-bytes")?) {
            (Some(avail), Some(used)) => Ok(Quota { total: Some(avail.saturating_add(used)), used }),
            _ => {
                let used = self.list()?.iter().fold(0u64, |n, l| n.saturating_add(l.size));
                Ok(Quota { total: None, used })
            }
        }
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        let segments: Vec<&str> = name.segments().collect();
        let mut dir = String::new();
        for seg in segments.iter().take(segments.len().saturating_sub(1)) {
            dir.push_str(seg);
            dir.push('/');
            let d = dir.clone();
            self.mkcol(&d)?;
        }
        let headers = self.headers(&[("Content-Type", "application/octet-stream".to_string())]);
        let mut resp = self.net.send("PUT", &self.url(name.as_str()), &headers, data)?;
        match resp.status {
            200 | 201 | 204 => Ok(RemoteRef { key: name.as_str().to_string() }),
            _ => Err(self.net.status_error("upload", &mut resp)),
        }
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let name = ObjectName::parse(&remote.key)?;
        let headers = self.headers(&[]);
        let mut resp = self.net.send("GET", &self.url(name.as_str()), &headers, &[])?;
        if resp.status != 200 {
            return Err(self.net.status_error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let name = ObjectName::parse(&remote.key)?;
        let headers = self.headers(&[]);
        let mut resp = self.net.send("DELETE", &self.url(name.as_str()), &headers, &[])?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.net.status_error("delete", &mut resp)),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let mut out = Vec::new();
        let mut queue: Vec<(String, usize)> = vec![(String::new(), 0)];
        let mut visited = BTreeSet::new();
        while let Some((dir, depth)) = queue.pop() {
            if !visited.insert(dir.clone()) || depth >= MAX_DEPTH {
                continue;
            }
            if visited.len() > MAX_FOLDERS {
                return Err(CloudError::Io(format!("more than {MAX_FOLDERS} folders under {}", self.base.display())));
            }
            let xml = self.propfind(&dir, "1", PROPFIND_LIST)?;
            for resp in responses(&xml) {
                let Some(href) = xml_elements(resp, "href")?.into_iter().next() else { continue };
                let Some(rel) = self.relative(href.trim()) else { continue };
                let is_dir = find_tag(resp, b"collection", false).is_some();
                let rel = rel.trim_end_matches('/').to_string();
                if rel.is_empty() || rel == dir.trim_end_matches('/') {
                    continue; // the folder itself
                }
                let Ok(name) = ObjectName::parse(&rel) else { continue };
                if is_dir {
                    self.known_dirs.insert(format!("{rel}/"));
                    queue.push((format!("{rel}/"), depth + 1));
                } else {
                    if out.len() >= MAX_LISTED {
                        return Err(CloudError::Io(format!("more than {MAX_LISTED} files under {}", self.base.display())));
                    }
                    let size = xml_elements(resp, "getcontentlength")?.first().and_then(|v| v.trim().parse().ok()).unwrap_or(0);
                    out.push(Listed { remote: RemoteRef { key: rel }, name, size });
                }
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(url: &str) -> WebDavProvider {
        WebDavProvider::new(WebDavConfig { url: url.into(), user: "me".into(), password: "pw".into() }, "t").unwrap()
    }

    #[test]
    fn hrefs_map_to_names_inside_the_base_only() {
        let p = provider("https://cloud.example/remote.php/dav/files/me/Light%20Craft/");
        assert_eq!(p.relative("/remote.php/dav/files/me/Light%20Craft/blobs/ab/x.bin").as_deref(), Some("blobs/ab/x.bin"));
        assert_eq!(p.relative("https://cloud.example/remote.php/dav/files/me/Light Craft/blobs/").as_deref(), Some("blobs/"));
        assert_eq!(p.relative("/remote.php/dav/files/me/Other/x"), None);
        assert_eq!(p.relative("/remote.php/dav/files/me/Light%2"), None);
    }

    #[test]
    fn multistatus_parsing() {
        let xml = br#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:">
<d:response><d:href>/dav/lc/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/dav/lc/blobs/</d:href><d:propstat><d:prop><d:resourcetype><d:collection /></d:resourcetype></d:prop></d:propstat></d:response>
<d:response><d:href>/dav/lc/vault.enc</d:href><d:propstat><d:prop><d:resourcetype/><d:getcontentlength>42</d:getcontentlength></d:prop></d:propstat></d:response>
</d:multistatus>"#;
        let rs = responses(xml);
        assert_eq!(rs.len(), 3);
        assert!(find_tag(rs[0], b"collection", false).is_some());
        assert!(find_tag(rs[1], b"collection", false).is_some());
        assert!(find_tag(rs[2], b"collection", false).is_none());
        assert_eq!(xml_elements(rs[2], "getcontentlength").unwrap(), ["42"]);
        assert!(responses(b"<d:response><d:href>/x").is_empty(), "an unterminated response is skipped");
    }

    #[test]
    fn config_is_checked_and_secrets_are_not_printed() {
        assert!(WebDavProvider::new(WebDavConfig { url: "https://h/dav".into(), user: String::new(), password: String::new() }, "t").is_err());
        assert!(matches!(
            WebDavProvider::new(WebDavConfig { url: "http://nas.example/dav/".into(), user: "me".into(), password: "pw".into() }, "t"),
            Err(CloudError::Denied(_))
        ));
        assert!(WebDavProvider::new(WebDavConfig { url: "http://127.0.0.1:8081/".into(), user: "me".into(), password: "pw".into() }, "t").is_ok());
        let c = WebDavConfig { url: "https://h/".into(), user: "me".into(), password: "hunter2".into() };
        assert!(!format!("{c:?}").contains("hunter2"));
        assert_eq!(provider("https://h/").auth.as_deref(), Some("Basic bWU6cHc="));
    }
}
