//! S3-compatible storage: Amazon S3, Backblaze B2, Cloudflare R2, Wasabi, MinIO, Garage…
//! Requests are signed with AWS Signature Version 4, written here from the public
//! specification. Objects live under `<prefix>` in one bucket.

use hmac::{Hmac, Mac};
use lightcraft_fetch::http::Url;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::net::{MAX_META_BODY, Net, amz_date, hex, now_unix, uri_encode, xml_elements};
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// Most objects [`Provider::list`] collects before giving up with an error.
pub const MAX_LISTED: usize = 2_000_000;
/// S3's single-PUT limit (5 GiB); larger blobs need multipart upload, not done yet.
pub const MAX_PUT: u64 = 5 * 1024 * 1024 * 1024;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct S3Config {
    /// `https://s3.eu-central-003.backblazeb2.com`, `http://127.0.0.1:9000`…
    pub endpoint: String,
    pub region: String,
    pub bucket: String,
    /// The app folder inside the bucket, e.g. `lightcraft/library-1/` (empty = the whole bucket).
    pub prefix: String,
    pub access_key: String,
    pub secret_key: String,
    /// `https://endpoint/bucket/key` (true) or `https://bucket.endpoint/key` (false).
    #[serde(default = "yes")]
    pub path_style: bool,
}

fn yes() -> bool {
    true
}

impl std::fmt::Debug for S3Config {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("S3Config")
            .field("endpoint", &self.endpoint)
            .field("region", &self.region)
            .field("bucket", &self.bucket)
            .field("prefix", &self.prefix)
            .field("access_key", &self.access_key)
            .finish_non_exhaustive()
    }
}

pub struct S3Provider {
    config: S3Config,
    label: String,
    /// The endpoint with the bucket in the host (virtual-hosted) or not (path style).
    base: Url,
    pub net: Net,
    clock: fn() -> u64,
}

fn check_bucket(b: &str) -> Result<()> {
    if (3..=63).contains(&b.len()) && b.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'.')) {
        Ok(())
    } else {
        Err(CloudError::InvalidName(format!("bucket name {b:?}")))
    }
}

/// An empty prefix, or `/`-terminated segments that are valid object names.
fn check_prefix(p: &str) -> Result<()> {
    if p.is_empty() {
        return Ok(());
    }
    let inner = p.strip_suffix('/').ok_or_else(|| CloudError::InvalidName(format!("prefix {p:?} must end with /")))?;
    if inner.split('/').all(|s| ObjectName::parse(s).is_ok()) && inner.len() <= 512 {
        Ok(())
    } else {
        Err(CloudError::InvalidName(format!("prefix {p:?}")))
    }
}

impl S3Provider {
    pub fn new(config: S3Config, label: impl Into<String>) -> Result<S3Provider> {
        check_bucket(&config.bucket)?;
        check_prefix(&config.prefix)?;
        if config.region.is_empty() || !config.region.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-') {
            return Err(CloudError::InvalidName(format!("region {:?}", config.region)));
        }
        if config.access_key.is_empty() || config.secret_key.is_empty() {
            return Err(CloudError::Denied("missing access key or secret".into()));
        }
        let endpoint = Url::parse(&config.endpoint).map_err(|e| CloudError::InvalidName(format!("endpoint: {e}")))?;
        if endpoint.path != "/" {
            return Err(CloudError::InvalidName("the endpoint must not have a path".into()));
        }
        let base = if config.path_style { endpoint } else { Url { host: format!("{}.{}", config.bucket, endpoint.host), ..endpoint } };
        Ok(S3Provider { config, label: label.into(), base, net: Net::default(), clock: now_unix })
    }

    /// Use a fixed clock (tests).
    pub fn with_clock(mut self, clock: fn() -> u64) -> S3Provider {
        self.clock = clock;
        self
    }

    /// Create the bucket if it doesn't exist yet (account setup; most providers also let the user
    /// create it in their console).
    pub fn ensure_bucket(&self) -> Result<()> {
        let body = if self.config.region == "us-east-1" {
            String::new()
        } else {
            format!(
                "<CreateBucketConfiguration xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\"><LocationConstraint>{}</LocationConstraint></CreateBucketConfiguration>",
                self.config.region
            )
        };
        let mut resp = self.call("PUT", &self.bucket_path(), &[], body.as_bytes())?;
        match resp.status {
            200 => Ok(()),
            409 => {
                let msg = self.net.status_error("create bucket", &mut resp);
                if msg.to_string().contains("BucketAlreadyOwnedByYou") { Ok(()) } else { Err(msg) }
            }
            _ => Err(self.net.status_error("create bucket", &mut resp)),
        }
    }

    fn key(&self, name: &str) -> String {
        format!("{}{name}", self.config.prefix)
    }

    /// Path (without query) of `key` in the bucket.
    fn object_path(&self, key: &str) -> String {
        let key = uri_encode(key, true);
        if self.config.path_style { format!("/{}/{key}", self.config.bucket) } else { format!("/{key}") }
    }

    fn bucket_path(&self) -> String {
        if self.config.path_style { format!("/{}", self.config.bucket) } else { "/".into() }
    }

    /// Send a signed request. `query` pairs are unencoded.
    fn call(&self, method: &str, path: &str, query: &[(&str, &str)], body: &[u8]) -> Result<lightcraft_fetch::http::Response> {
        let mut q: Vec<(String, String)> = query.iter().map(|(k, v)| (uri_encode(k, false), uri_encode(v, false))).collect();
        q.sort();
        let query_string = q.iter().map(|(k, v)| format!("{k}={v}")).collect::<Vec<_>>().join("&");
        let url = Url { path: if query_string.is_empty() { path.to_string() } else { format!("{path}?{query_string}") }, ..self.base.clone() };
        let payload_hash = hex(&Sha256::digest(body));
        let (amz, _) = amz_date((self.clock)());
        let host = host_header(&url);
        let auth = sign(&SignInput {
            method,
            path,
            query: &query_string,
            headers: &[("host", &host), ("x-amz-content-sha256", &payload_hash), ("x-amz-date", &amz)],
            payload_hash: &payload_hash,
            amz_date: &amz,
            region: &self.config.region,
            service: "s3",
            access_key: &self.config.access_key,
            secret_key: &self.config.secret_key,
        })?;
        let headers = [("x-amz-content-sha256", payload_hash.clone()), ("x-amz-date", amz), ("Authorization", auth)];
        self.net.send(method, &url, &headers, body)
    }
}

fn host_header(url: &Url) -> String {
    let default = if url.tls { 443 } else { 80 };
    let host = if url.host.contains(':') { format!("[{}]", url.host) } else { url.host.clone() };
    if url.port == default { host } else { format!("{host}:{}", url.port) }
}

/// What a Signature Version 4 signature covers. `path` is already URI-encoded, `query` already
/// canonical (encoded, sorted), header names lower case and sorted.
pub struct SignInput<'a> {
    pub method: &'a str,
    pub path: &'a str,
    pub query: &'a str,
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_hash: &'a str,
    pub amz_date: &'a str,
    pub region: &'a str,
    pub service: &'a str,
    pub access_key: &'a str,
    pub secret_key: &'a str,
}

fn hmac(key: &[u8], data: &[u8]) -> Result<Vec<u8>> {
    let mut m = Hmac::<Sha256>::new_from_slice(key).map_err(|e| CloudError::Io(format!("HMAC: {e}")))?;
    m.update(data);
    Ok(m.finalize().into_bytes().to_vec())
}

/// The signing key for a day, region and service.
pub fn signing_key(secret: &str, date: &str, region: &str, service: &str) -> Result<Vec<u8>> {
    let k = hmac(format!("AWS4{secret}").as_bytes(), date.as_bytes())?;
    let k = hmac(&k, region.as_bytes())?;
    let k = hmac(&k, service.as_bytes())?;
    hmac(&k, b"aws4_request")
}

/// The `Authorization` header value for a request.
pub fn sign(i: &SignInput) -> Result<String> {
    let date = i.amz_date.get(..8).ok_or_else(|| CloudError::Io("bad date".into()))?;
    let canonical_headers: String = i.headers.iter().map(|(k, v)| format!("{k}:{}\n", v.trim())).collect();
    let signed: Vec<&str> = i.headers.iter().map(|(k, _)| *k).collect();
    let signed = signed.join(";");
    let canonical = format!("{}\n{}\n{}\n{canonical_headers}\n{signed}\n{}", i.method, i.path, i.query, i.payload_hash);
    let scope = format!("{date}/{}/{}/aws4_request", i.region, i.service);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", i.amz_date, hex(&Sha256::digest(canonical.as_bytes())));
    let signature = hex(&hmac(&signing_key(i.secret_key, date, i.region, i.service)?, to_sign.as_bytes())?);
    Ok(format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", i.access_key))
}

impl Provider for S3Provider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "s3".into(), label: self.label.clone(), max_file_size: Some(MAX_PUT) }
    }

    /// S3 has no quota: `total` is unknown and `used` counts this app folder's objects.
    fn quota(&mut self) -> Result<Quota> {
        let used = self.list()?.iter().fold(0u64, |n, l| n.saturating_add(l.size));
        Ok(Quota { total: None, used })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        if data.len() as u64 > MAX_PUT {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: MAX_PUT });
        }
        let mut resp = self.call("PUT", &self.object_path(&self.key(name.as_str())), &[], data)?;
        if resp.status != 200 {
            return Err(self.net.status_error("upload", &mut resp));
        }
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let name = ObjectName::parse(&remote.key)?;
        let mut resp = self.call("GET", &self.object_path(&self.key(name.as_str())), &[], &[])?;
        if resp.status != 200 {
            return Err(self.net.status_error("download", &mut resp));
        }
        self.net.body(&mut resp, max_len)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        let name = ObjectName::parse(&remote.key)?;
        let mut resp = self.call("DELETE", &self.object_path(&self.key(name.as_str())), &[], &[])?;
        match resp.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(self.net.status_error("delete", &mut resp)),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        let prefix = self.config.prefix.clone();
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix.as_str())];
            if let Some(t) = &token {
                query.push(("continuation-token", t.as_str()));
            }
            let mut resp = self.call("GET", &self.bucket_path(), &query, &[])?;
            if resp.status != 200 {
                return Err(self.net.status_error("list", &mut resp));
            }
            let xml = self.net.body(&mut resp, MAX_META_BODY)?;
            let keys = xml_elements(&xml, "Key")?;
            let sizes = xml_elements(&xml, "Size")?;
            if keys.len() != sizes.len() {
                return Err(CloudError::Corrupt("listing: keys and sizes don't match".into()));
            }
            for (key, size) in keys.into_iter().zip(sizes) {
                // keys outside the prefix or not valid names (other tools' files) are skipped
                let Some(rel) = key.strip_prefix(prefix.as_str()) else { continue };
                let Ok(name) = ObjectName::parse(rel) else { continue };
                if out.len() >= MAX_LISTED {
                    return Err(CloudError::Io(format!("more than {MAX_LISTED} objects under {prefix:?}")));
                }
                out.push(Listed { remote: RemoteRef { key: name.as_str().to_string() }, name, size: size.trim().parse().unwrap_or(0) });
            }
            let truncated = xml_elements(&xml, "IsTruncated")?.first().is_some_and(|t| t.trim() == "true");
            let next = xml_elements(&xml, "NextContinuationToken")?.into_iter().next();
            match (truncated, next) {
                (true, Some(t)) if Some(&t) != token.as_ref() && !t.is_empty() => token = Some(t),
                (true, _) => return Err(CloudError::Corrupt("listing: truncated without a new continuation token".into())),
                (false, _) => break,
            }
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "wJalrXUtnFEMI/K7MDENG+bPxRfiCYEXAMPLEKEY";

    /// The signing-key derivation example from AWS's Signature Version 4 documentation.
    #[test]
    fn aws_documented_signing_key() {
        assert_eq!(
            hex(&signing_key(SECRET, "20120215", "us-east-1", "iam").unwrap()),
            "f4780e2d9f65fa895f9c67b32ce1baf0b0d8a43505a000a1a9e090d414db404d"
        );
    }

    /// The `GET ?Action=ListUsers` example from AWS's Signature Version 4 documentation.
    #[test]
    fn aws_documented_iam_example() {
        let empty = hex(&Sha256::digest(b""));
        let auth = sign(&SignInput {
            method: "GET",
            path: "/",
            query: "Action=ListUsers&Version=2010-05-08",
            headers: &[
                ("content-type", "application/x-www-form-urlencoded; charset=utf-8"),
                ("host", "iam.amazonaws.com"),
                ("x-amz-date", "20150830T123600Z"),
            ],
            payload_hash: &empty,
            amz_date: "20150830T123600Z",
            region: "us-east-1",
            service: "iam",
            access_key: "AKIDEXAMPLE",
            secret_key: SECRET,
        })
        .unwrap();
        assert!(auth.ends_with("Signature=5d672d79c15b13162d9279b0855cfba6789a8edb4c82c400e06b5924a6f2b5d7"), "{auth}");
    }

    fn provider() -> S3Provider {
        S3Provider::new(
            S3Config {
                endpoint: "http://127.0.0.1:9000".into(),
                region: "us-east-1".into(),
                bucket: "photos".into(),
                prefix: "lc/".into(),
                access_key: "AKIDEXAMPLE".into(),
                secret_key: SECRET.into(),
                path_style: true,
            },
            "test",
        )
        .unwrap()
        .with_clock(|| 1_791_635_696)
    }

    /// Signatures computed independently by botocore (the AWS SDK for Python) for the same
    /// requests: an object PUT and a listing page with a token that needs encoding.
    #[test]
    fn matches_botocore_for_s3_requests() {
        let p = provider();
        let body = b"hello lightcraft";
        let hash = hex(&Sha256::digest(body));
        let (amz, _) = amz_date((p.clock)());
        assert_eq!(amz, "20261010T123456Z");
        let put = sign(&SignInput {
            method: "PUT",
            path: &p.object_path("lc/blobs/ab/x.bin"),
            query: "",
            headers: &[("host", "127.0.0.1:9000"), ("x-amz-content-sha256", &hash), ("x-amz-date", &amz)],
            payload_hash: &hash,
            amz_date: &amz,
            region: "us-east-1",
            service: "s3",
            access_key: "AKIDEXAMPLE",
            secret_key: SECRET,
        })
        .unwrap();
        assert!(put.ends_with("Signature=ab0c9000c77e25687fa1dae9fb31d7273fc5214264edf5c8c445265f2d16934f"), "{put}");
        let empty = hex(&Sha256::digest(b""));
        let list = sign(&SignInput {
            method: "GET",
            path: &p.bucket_path(),
            query: "continuation-token=a%2Bb%3D%2F&list-type=2&prefix=lc%2F",
            headers: &[("host", "127.0.0.1:9000"), ("x-amz-content-sha256", &empty), ("x-amz-date", &amz)],
            payload_hash: &empty,
            amz_date: &amz,
            region: "us-east-1",
            service: "s3",
            access_key: "AKIDEXAMPLE",
            secret_key: SECRET,
        })
        .unwrap();
        assert!(list.ends_with("Signature=038eaa1e6739684b8cb47a5b3f898224b3625a4993116c0ddbdba7fca8afd651"), "{list}");
    }

    #[test]
    fn config_is_validated_and_secrets_are_not_printed() {
        let good = provider().config;
        assert!(!format!("{good:?}").contains(SECRET));
        for bad in [
            S3Config { bucket: "No".into(), ..good.clone() },
            S3Config { prefix: "lc".into(), ..good.clone() },
            S3Config { prefix: "../".into(), ..good.clone() },
            S3Config { endpoint: "http://h/path".into(), ..good.clone() },
            S3Config { region: "us east".into(), ..good.clone() },
            S3Config { secret_key: String::new(), ..good.clone() },
        ] {
            assert!(S3Provider::new(bad.clone(), "x").is_err(), "{bad:?}");
        }
        let hosted = S3Provider::new(S3Config { path_style: false, ..good }, "x").unwrap();
        assert_eq!(hosted.base.host, "photos.127.0.0.1");
        assert_eq!(hosted.object_path("lc/a"), "/lc/a");
    }
}
