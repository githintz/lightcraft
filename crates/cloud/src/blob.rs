//! Content-addressed blobs and the names objects get inside an account's app folder.

use std::fmt;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{CloudError, Provider, RemoteRef, Result};

/// The SHA-256 of a blob's content (before encryption, once that lands), as 64 lower-case hex
/// digits. Names the blob on every account, so a download can be checked and the same bytes are
/// stored once.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ContentId(String);

impl ContentId {
    pub fn of(data: &[u8]) -> ContentId {
        let digest = Sha256::digest(data);
        let mut hex = String::with_capacity(64);
        for b in digest {
            hex.push(char::from(HEX[usize::from(b >> 4)]));
            hex.push(char::from(HEX[usize::from(b & 15)]));
        }
        ContentId(hex)
    }

    pub fn parse(s: &str) -> Result<ContentId> {
        if s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
            Ok(ContentId(s.to_string()))
        } else {
            Err(CloudError::InvalidName(format!("not a content id: {:?}", s.chars().take(80).collect::<String>())))
        }
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const HEX: &[u8; 16] = b"0123456789abcdef";

impl TryFrom<String> for ContentId {
    type Error = CloudError;
    fn try_from(s: String) -> Result<ContentId> {
        ContentId::parse(&s)
    }
}

impl From<ContentId> for String {
    fn from(id: ContentId) -> String {
        id.0
    }
}

impl fmt::Display for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ContentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ContentId({})", self.0)
    }
}

/// Longest object name, in bytes.
pub const MAX_NAME_LEN: usize = 255;
/// Most `/`-separated segments in an object name (`blobs/ab/<id>.bin` has 3).
pub const MAX_DEPTH: usize = 4;

/// A relative name inside an account's app folder: 1 to [`MAX_DEPTH`] segments joined by `/`,
/// each made of `a-z 0-9 . _ -` and not starting with `.` (names starting with `.` are left to
/// providers for temporary files). Validated on construction, so a name read back from an
/// account can never point outside the app folder.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ObjectName(String);

impl ObjectName {
    pub fn parse(s: &str) -> Result<ObjectName> {
        let bad = |why: &str| Err(CloudError::InvalidName(format!("{why}: {:?}", s.chars().take(80).collect::<String>())));
        if s.is_empty() || s.len() > MAX_NAME_LEN {
            return bad("empty or too long");
        }
        let mut depth = 0;
        for seg in s.split('/') {
            depth += 1;
            if depth > MAX_DEPTH {
                return bad("too deep");
            }
            if seg.is_empty() || seg.starts_with('.') {
                return bad("empty or hidden segment");
            }
            if !seg.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')) {
                return bad("unsupported character");
            }
        }
        Ok(ObjectName(s.to_string()))
    }

    /// Where a blob is stored: `blobs/<first two hex digits>/<id>.bin`, so no folder holds more
    /// than 1/256 of the blobs.
    pub fn blob(id: &ContentId) -> ObjectName {
        let s = id.as_str();
        ObjectName(format!("blobs/{}/{s}.bin", s.get(..2).unwrap_or("00")))
    }

    /// The content id of a [`ObjectName::blob`] name, `None` for other objects.
    pub fn blob_id(&self) -> Option<ContentId> {
        let mut parts = self.0.split('/');
        let (Some("blobs"), Some(fan), Some(file), None) = (parts.next(), parts.next(), parts.next(), parts.next()) else {
            return None;
        };
        let id = ContentId::parse(file.strip_suffix(".bin")?).ok()?;
        (id.as_str().get(..2) == Some(fan)).then_some(id)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/')
    }
}

impl TryFrom<String> for ObjectName {
    type Error = CloudError;
    fn try_from(s: String) -> Result<ObjectName> {
        ObjectName::parse(&s)
    }
}

impl From<ObjectName> for String {
    fn from(n: ObjectName) -> String {
        n.0
    }
}

impl fmt::Display for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl fmt::Debug for ObjectName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectName({})", self.0)
    }
}

/// Store `data` as a content-addressed blob on `provider`.
pub fn store_blob(provider: &mut dyn Provider, data: &[u8]) -> Result<(ContentId, RemoteRef)> {
    let id = ContentId::of(data);
    let remote = provider.put(&ObjectName::blob(&id), data)?;
    Ok((id, remote))
}

/// Read a blob back and check it is the content `id` names: a provider that returns other bytes
/// (damage, a truncated upload, a server bug) gives [`CloudError::Corrupt`], never wrong data.
pub fn fetch_blob(provider: &mut dyn Provider, remote: &RemoteRef, id: &ContentId, max_len: u64) -> Result<Vec<u8>> {
    let data = provider.get(remote, max_len)?;
    let got = ContentId::of(&data);
    if &got != id {
        return Err(CloudError::Corrupt(format!("blob {id} came back as {} bytes with id {got}", data.len())));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_id_is_sha256_hex() {
        assert_eq!(ContentId::of(b"").as_str(), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert!(ContentId::parse("E3B0C44298FC1C149AFBF4C8996FB92427AE41E4649B934CA495991B7852B855").is_err());
        assert!(ContentId::parse("abc").is_err());
        assert!(ContentId::parse(&"é".repeat(32)).is_err());
    }

    #[test]
    fn blob_names_round_trip() {
        let id = ContentId::of(b"photo");
        let name = ObjectName::blob(&id);
        assert_eq!(name.as_str(), format!("blobs/{}/{id}.bin", &id.as_str()[..2]));
        assert_eq!(ObjectName::parse(name.as_str()).ok(), Some(name.clone()));
        assert_eq!(name.blob_id(), Some(id));
        assert_eq!(ObjectName::parse("manifest/snapshot-1.json").ok().and_then(|n| n.blob_id()), None);
        // fan-out folder that doesn't match the id
        let wrong = format!("blobs/zz/{}.bin", ContentId::of(b"x"));
        assert_eq!(ObjectName::parse(&wrong).ok().and_then(|n| n.blob_id()), None);
    }

    #[test]
    fn names_cannot_escape_the_app_folder() {
        for bad in ["", "/abs", "a//b", "../x", "a/../b", "a/./b", ".hidden", "a/.tmp", "A", "a b", "a\\b", "a/b/c/d/e", "é", "a\0b"] {
            assert!(ObjectName::parse(bad).is_err(), "{bad:?} should be refused");
        }
        assert!(ObjectName::parse(&"a".repeat(MAX_NAME_LEN + 1)).is_err());
        assert!(ObjectName::parse("manifest/ops-1-2.json").is_ok());
    }

    #[test]
    fn serde_validates() {
        assert!(serde_json::from_str::<ObjectName>("\"../etc/passwd\"").is_err());
        assert!(serde_json::from_str::<ContentId>("\"nope\"").is_err());
        let id = ContentId::of(b"x");
        let json = serde_json::to_string(&id).unwrap_or_default();
        assert_eq!(serde_json::from_str::<ContentId>(&json).ok(), Some(id));
    }
}
