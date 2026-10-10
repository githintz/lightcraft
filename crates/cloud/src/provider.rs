//! The interface every storage backend implements.

use serde::{Deserialize, Serialize};

use crate::{ObjectName, Result};

/// What an account can do, for placement and the UI.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderInfo {
    /// Provider kind (`"memory"`, `"folder"`, later `"s3"`, `"webdav"`…).
    pub provider: String,
    /// Human-readable name of this account ("Backups on NAS").
    pub label: String,
    /// The provider's per-file limit, if it has one.
    pub max_file_size: Option<u64>,
}

/// An account's real storage, as the provider reports it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quota {
    /// `None` when the provider doesn't say (a folder, some WebDAV servers).
    pub total: Option<u64>,
    /// Everything stored on the account, not only this library's blobs.
    pub used: u64,
}

/// Where an object lives on one account: the provider's own id for it (a path, an object key,
/// a file id). Stored in the manifest next to the account id.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct RemoteRef {
    pub key: String,
}

/// One object found by [`Provider::list`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Listed {
    pub name: ObjectName,
    pub remote: RemoteRef,
    pub size: u64,
}

/// A storage account. Everything inside the account's app folder is addressed by
/// [`ObjectName`]; nothing outside it is read or written.
///
/// Calls block until done: run them on a worker thread, never on the UI thread. Every failure is
/// an error (never a panic): a dropped connection, a full account, a missing object.
pub trait Provider: Send {
    fn info(&self) -> ProviderInfo;

    fn quota(&mut self) -> Result<Quota>;

    /// Store `data` under `name`, replacing an object of the same name. Durable when it returns:
    /// a crash or dropped connection leaves the old object or none, never part of the new one.
    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef>;

    /// The whole object, refused with [`crate::CloudError::TooLarge`] (before reading it) when it
    /// is bigger than `max_len`. A missing object is [`crate::CloudError::NotFound`].
    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>>;

    /// Remove an object; one that is already gone is not an error.
    fn delete(&mut self, remote: &RemoteRef) -> Result<()>;

    /// Every object in the app folder (index rebuild, audits). Entries whose names aren't valid
    /// [`ObjectName`]s are skipped, never returned.
    fn list(&mut self) -> Result<Vec<Listed>>;
}
