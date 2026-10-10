//! An account that is a folder: a second disk, a NAS share, or a folder that a sync client
//! (a cloud drive's desktop app) uploads on its own. Objects are files under the folder, at their
//! [`ObjectName`] path.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::blob::MAX_DEPTH;
use crate::{CloudError, Listed, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result};

/// Most files [`Provider::list`] walks before giving up with an error (instead of a silently
/// incomplete list).
pub const MAX_LISTED: usize = 2_000_000;

static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct FolderProvider {
    root: PathBuf,
    label: String,
}

impl FolderProvider {
    /// An existing folder. A missing one is an error, never created here: an unplugged disk or
    /// unmounted share must not turn into a new empty folder on the startup disk.
    pub fn open(root: impl Into<PathBuf>, label: impl Into<String>) -> Result<FolderProvider> {
        let root = root.into();
        if !fs::metadata(&root).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(CloudError::Io(format!("{} is not an available folder", root.display())));
        }
        Ok(FolderProvider { root, label: label.into() })
    }

    /// Create the folder (and its parents) when the user sets the account up, then open it.
    pub fn create(root: impl Into<PathBuf>, label: impl Into<String>) -> Result<FolderProvider> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        FolderProvider::open(root, label)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, name: &ObjectName) -> PathBuf {
        name.segments().fold(self.root.clone(), |p, s| p.join(s))
    }

    /// The file behind `remote`, which must be a regular file (not a link that could point
    /// outside the folder).
    fn existing_file(&self, remote: &RemoteRef) -> Result<(PathBuf, u64)> {
        let path = self.path(&ObjectName::parse(&remote.key)?);
        let meta = fs::symlink_metadata(&path)?;
        if !meta.file_type().is_file() {
            return Err(CloudError::Corrupt(format!("{} is not a regular file", path.display())));
        }
        Ok((path, meta.len()))
    }

    fn walk(&self, dir: &Path, prefix: &str, depth: usize, out: &mut Vec<Listed>) -> Result<()> {
        if depth > MAX_DEPTH {
            return Ok(());
        }
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let Some(file_name) = entry.file_name().to_str().map(str::to_string) else { continue };
            let name = if prefix.is_empty() { file_name } else { format!("{prefix}/{file_name}") };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                if ObjectName::parse(&name).is_ok() {
                    self.walk(&entry.path(), &name, depth + 1, out)?;
                }
            } else if kind.is_file()
                && let Ok(object) = ObjectName::parse(&name)
            {
                if out.len() >= MAX_LISTED {
                    return Err(CloudError::Io(format!("more than {MAX_LISTED} files in {}", self.root.display())));
                }
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                out.push(Listed { remote: RemoteRef { key: name }, name: object, size });
            }
        }
        Ok(())
    }
}

/// Make a rename durable: fsync the folder that holds it. Not possible on every platform
/// (Windows can't open a folder as a file); the rename itself is still atomic there.
fn sync_dir(dir: &Path) {
    if let Ok(d) = File::open(dir) {
        let _ = d.sync_all();
    }
}

impl Provider for FolderProvider {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: "folder".into(), label: self.label.clone(), max_file_size: None }
    }

    fn quota(&mut self) -> Result<Quota> {
        let used = self.list()?.iter().fold(0u64, |n, l| n.saturating_add(l.size));
        Ok(Quota { total: None, used })
    }

    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        let path = self.path(name);
        let dir = path.parent().ok_or_else(|| CloudError::InvalidName(name.to_string()))?;
        fs::create_dir_all(dir)?;
        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("object");
        let n = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
        // starts with '.', so it is never a valid ObjectName and never listed
        let temp = dir.join(format!(".tmp-{}-{n}-{file_name}", std::process::id()));
        let written = (|| -> std::io::Result<()> {
            let mut f = File::create(&temp)?;
            f.write_all(data)?;
            f.sync_all()?;
            drop(f);
            fs::rename(&temp, &path)
        })();
        if let Err(e) = written {
            let _ = fs::remove_file(&temp);
            return Err(e.into());
        }
        sync_dir(dir);
        Ok(RemoteRef { key: name.as_str().to_string() })
    }

    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        let (path, size) = self.existing_file(remote)?;
        if size > max_len {
            return Err(CloudError::TooLarge { size, max: max_len });
        }
        // the file may grow between the size check and the read: never read past the limit
        let mut data = Vec::with_capacity(usize::try_from(size).unwrap_or(0));
        File::open(&path)?.take(max_len.saturating_add(1)).read_to_end(&mut data)?;
        let read = data.len() as u64;
        if read > max_len {
            return Err(CloudError::TooLarge { size: read, max: max_len });
        }
        Ok(data)
    }

    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        match self.existing_file(remote) {
            Ok((path, _)) => {
                fs::remove_file(&path)?;
                if let Some(dir) = path.parent() {
                    sync_dir(dir);
                }
                Ok(())
            }
            Err(CloudError::NotFound) => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn list(&mut self) -> Result<Vec<Listed>> {
        let mut out = Vec::new();
        self.walk(&self.root, "", 1, &mut out)?;
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }
}
