//! Opening the accounts a vault lists: one [`Provider`] per kind, from the account's stored
//! configuration. Native only (the network providers are).

use serde::Deserialize;

use crate::dropbox::{DropboxConfig, DropboxProvider};
use crate::gdrive::{GoogleDriveConfig, GoogleDriveProvider};
use crate::onedrive::{OneDriveConfig, OneDriveProvider};
use crate::opendrive::{OpenDriveConfig, OpenDriveProvider};
use crate::s3::{S3Config, S3Provider};
use crate::webdav::{WebDavConfig, WebDavProvider};
use crate::{AccountSecret, CloudError, FolderProvider, Net, Provider, Result};

/// Every provider kind an account can be.
pub const KINDS: &[&str] = &["folder", "s3", "webdav", "gdrive", "onedrive", "dropbox", "opendrive"];

/// A folder account's configuration.
#[derive(Deserialize)]
struct FolderConfig {
    path: String,
}

fn config<T: for<'de> Deserialize<'de>>(secret: &AccountSecret) -> Result<T> {
    serde_json::from_value(secret.config.clone()).map_err(|e| CloudError::InvalidName(format!("{} account settings: {e}", secret.provider)))
}

/// The provider for `secret`. With `create`, the account's app folder (folder, bucket, WebDAV
/// collection, Drive folder) is made when missing, which may use the network; without it,
/// nothing is sent until the first call, and a missing folder stays an error (an unplugged
/// disk must not become a new empty folder).
pub fn open(secret: &AccountSecret, create: bool) -> Result<Box<dyn Provider>> {
    let label = secret.label.clone();
    Ok(match secret.provider.as_str() {
        "folder" => {
            let c: FolderConfig = config(secret)?;
            if c.path.is_empty() {
                return Err(CloudError::InvalidName("a folder account needs a path".into()));
            }
            Box::new(if create { FolderProvider::create(c.path, label)? } else { FolderProvider::open(c.path, label)? })
        }
        "s3" => {
            let p = S3Provider::new(config::<S3Config>(secret)?, label)?;
            if create {
                p.ensure_bucket()?;
            }
            Box::new(p)
        }
        "webdav" => {
            let mut p = WebDavProvider::new(config::<WebDavConfig>(secret)?, label)?;
            if create {
                p.ensure_root()?;
            }
            Box::new(p)
        }
        "gdrive" => {
            let mut p = GoogleDriveProvider::new(config::<GoogleDriveConfig>(secret)?, label)?;
            if create {
                p.ensure_root()?;
            }
            Box::new(p)
        }
        "onedrive" => Box::new(OneDriveProvider::new(config::<OneDriveConfig>(secret)?, label)?),
        "dropbox" => Box::new(DropboxProvider::new(config::<DropboxConfig>(secret)?, label)?),
        "opendrive" => {
            let mut p = OpenDriveProvider::new(config::<OpenDriveConfig>(secret)?, label)?;
            if create {
                p.ensure_root()?;
            }
            Box::new(p)
        }
        other => return Err(CloudError::Unsupported(format!("storage provider {:?}", other.chars().take(32).collect::<String>()))),
    })
}

/// An OpenDrive account's sign-in, as given when it is added or restored from.
#[derive(Deserialize)]
struct OpenDriveLogin {
    username: String,
    password: String,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    api: Option<String>,
}

/// `secret` ready to [`open`]: an OpenDrive account given by its username and password is
/// signed in (uses the network), and only the refresh token is kept; the password is dropped.
/// Any other account is returned as it is.
pub fn sign_in(mut secret: AccountSecret, net: &Net) -> Result<AccountSecret> {
    if secret.provider != "opendrive" || secret.config.get("refresh_token").is_some() {
        return Ok(secret);
    }
    let login: OpenDriveLogin = config(&secret)?;
    let mut c = crate::opendrive::login(login.api.as_deref(), &login.username, &login.password, net)?;
    if let Some(folder) = login.folder.filter(|f| !f.is_empty()) {
        c.folder = folder;
    }
    secret.config = serde_json::to_value(c).map_err(|e| CloudError::Corrupt(e.to_string()))?;
    Ok(secret)
}

/// `secret` with the refresh token `provider` now uses (providers that rotate it), or `None`
/// when it is unchanged.
pub fn updated_secret(secret: &AccountSecret, provider: &dyn Provider) -> Option<AccountSecret> {
    let token = provider.refresh_token()?;
    let stored = secret.config.get("refresh_token").and_then(|v| v.as_str())?;
    if stored == token {
        return None;
    }
    let mut s = secret.clone();
    if let Some(obj) = s.config.as_object_mut() {
        obj.insert("refresh_token".into(), serde_json::Value::String(token.to_string()));
    }
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn secret(provider: &str, config: serde_json::Value) -> AccountSecret {
        AccountSecret { id: "a1".into(), provider: provider.into(), label: "test".into(), config }
    }

    #[test]
    fn every_kind_opens_from_its_config_without_the_network() {
        let dir = std::env::temp_dir().join(format!("lc-accounts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(open(&secret("folder", serde_json::json!({"path": dir.to_string_lossy()})), false).is_err(), "a missing folder isn't made");
        assert!(open(&secret("folder", serde_json::json!({"path": dir.to_string_lossy()})), true).is_ok());
        let s3 = serde_json::json!({"endpoint": "https://s3.example", "region": "us-east-1", "bucket": "lightcraft-test", "prefix": "lc/", "access_key": "k", "secret_key": "s", "path_style": true});
        assert_eq!(open(&secret("s3", s3), false).unwrap().info().provider, "s3");
        let dav = serde_json::json!({"url": "https://dav.example/lc/", "user": "u", "password": "p"});
        assert_eq!(open(&secret("webdav", dav), false).unwrap().info().provider, "webdav");
        let p = open(&secret("opendrive", serde_json::json!({"refresh_token": "r"})), false).unwrap();
        assert_eq!((p.info().provider.as_str(), p.refresh_token()), ("opendrive", Some("r")));
        for kind in ["gdrive", "onedrive", "dropbox"] {
            let p = open(&secret(kind, serde_json::json!({"client_id": "c", "refresh_token": "r"})), false).unwrap();
            assert_eq!(p.info().provider, kind);
            assert_eq!(p.refresh_token(), Some("r"));
        }
        assert!(matches!(open(&secret("terabox", serde_json::json!({})), false), Err(CloudError::Unsupported(_))));
        assert!(open(&secret("gdrive", serde_json::json!({"client_id": 5})), false).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn only_an_opendrive_login_is_signed_in() {
        let net = Net::default();
        let folder = secret("folder", serde_json::json!({"path": "/x"}));
        assert_eq!(sign_in(folder.clone(), &net).unwrap(), folder);
        let signed = secret("opendrive", serde_json::json!({"refresh_token": "r"}));
        assert_eq!(sign_in(signed.clone(), &net).unwrap(), signed);
        // a login with no password fails before any request
        let r = sign_in(secret("opendrive", serde_json::json!({"username": "u", "password": ""})), &net);
        assert!(matches!(r, Err(CloudError::Denied(_))));
        assert!(sign_in(secret("opendrive", serde_json::json!({"user": "u"})), &net).is_err());
    }

    #[test]
    fn rotated_refresh_tokens_update_the_secret() {
        let s = secret("onedrive", serde_json::json!({"client_id": "c", "refresh_token": "old"}));
        let p = open(&s, false).unwrap();
        assert!(updated_secret(&s, p.as_ref()).is_none());
        let s2 = secret("onedrive", serde_json::json!({"client_id": "c", "refresh_token": "older"}));
        let u = updated_secret(&s2, p.as_ref()).unwrap();
        assert_eq!(u.config["refresh_token"], "old");
    }
}
