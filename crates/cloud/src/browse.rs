//! Reading the user's own folders in a storage account, outside LightCraft's app folder: to scan
//! photos already there (`lightcraft-organize`). A [`Browse`] can only read (list a folder, read
//! a file's content hash and a byte range), so whoever holds one can't change the folders it
//! reads: there is no call to.

use std::fs;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Component, Path, PathBuf};

use crate::{CloudError, Result};

/// A file found by [`Browse::list`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BrowseFile {
    /// The provider's id for it (OpenDrive: a file id; a folder: its path).
    pub id: String,
    pub name: String,
    pub size: u64,
    /// When it was last changed, as the provider says it (for telling a changed file from one
    /// already scanned); `None` when it doesn't.
    pub modified: Option<String>,
}

/// A folder's direct contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BrowseDir {
    /// `(id, name)` of each subfolder.
    pub folders: Vec<(String, String)>,
    pub files: Vec<BrowseFile>,
}

/// Read-only access to an account's folders. Calls block (worker threads only) and every failure
/// is an error, never a panic.
pub trait Browse: Send {
    /// The provider kind (`"opendrive"`, `"folder"`).
    fn kind(&self) -> &str;

    /// The id of the folder at `path` (`/`-separated, from the account's root; `""` is the root).
    /// A missing folder is [`CloudError::NotFound`].
    fn folder(&mut self, path: &str) -> Result<String>;

    fn list(&mut self, folder: &str) -> Result<BrowseDir>;

    /// The provider's hash of the whole file (OpenDrive: MD5), when it keeps one: equal hashes and
    /// sizes mean equal files. `None` when the provider has none.
    fn content_hash(&mut self, file: &str) -> Result<Option<String>>;

    /// Up to `len` bytes from `offset` (fewer at the end of the file).
    fn read_range(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>>;
}

/// Most bytes one [`Browse::read_range`] returns.
pub const MAX_RANGE: u64 = 64 * 1024 * 1024;

/// A folder on this computer (a disk, a mounted share, a sync client's folder) as a [`Browse`].
/// Ids are paths relative to the root; nothing outside it is reachable.
#[derive(Debug)]
pub struct FolderBrowser {
    root: PathBuf,
}

impl FolderBrowser {
    pub fn new(root: impl Into<PathBuf>) -> Result<FolderBrowser> {
        let root = root.into();
        if !fs::metadata(&root).map(|m| m.is_dir()).unwrap_or(false) {
            return Err(CloudError::Io(format!("{} is not an available folder", root.display())));
        }
        Ok(FolderBrowser { root })
    }

    /// The path of the relative id `rel`, refused when it would leave the root.
    fn path(&self, rel: &str) -> Result<PathBuf> {
        let rel = Path::new(rel);
        if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
            return Err(CloudError::InvalidName(format!(
                "{:?} is not inside the folder",
                rel.display().to_string().chars().take(80).collect::<String>()
            )));
        }
        Ok(self.root.join(rel))
    }
}

fn join(dir: &str, name: &str) -> String {
    if dir.is_empty() { name.to_string() } else { format!("{dir}/{name}") }
}

impl Browse for FolderBrowser {
    fn kind(&self) -> &str {
        "folder"
    }

    fn folder(&mut self, path: &str) -> Result<String> {
        let rel = path.trim_matches('/');
        match fs::metadata(self.path(rel)?) {
            Ok(m) if m.is_dir() => Ok(rel.to_string()),
            _ => Err(CloudError::NotFound),
        }
    }

    fn list(&mut self, folder: &str) -> Result<BrowseDir> {
        let mut out = BrowseDir::default();
        let entries = fs::read_dir(self.path(folder)?).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CloudError::NotFound,
            _ => CloudError::Io(e.to_string()),
        })?;
        for entry in entries.flatten() {
            // names that aren't UTF-8 can't be named in a plan; symbolic links could leave the root
            let Ok(name) = entry.file_name().into_string() else { continue };
            let Ok(kind) = entry.file_type() else { continue };
            if kind.is_dir() {
                out.folders.push((join(folder, &name), name));
            } else if kind.is_file() {
                let meta = entry.metadata().ok();
                let modified = meta
                    .as_ref()
                    .and_then(|m| m.modified().ok())
                    .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                    .map(|d| d.as_nanos().to_string());
                out.files.push(BrowseFile { id: join(folder, &name), name, size: meta.map_or(0, |m| m.len()), modified });
            }
        }
        out.folders.sort();
        out.files.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    fn content_hash(&mut self, _file: &str) -> Result<Option<String>> {
        // hashing would read the whole file: callers compare sizes and what they read instead
        Ok(None)
    }

    fn read_range(&mut self, file: &str, offset: u64, len: u64) -> Result<Vec<u8>> {
        let mut f = fs::File::open(self.path(file)?).map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => CloudError::NotFound,
            _ => CloudError::Io(e.to_string()),
        })?;
        f.seek(SeekFrom::Start(offset))?;
        let mut out = Vec::new();
        f.take(len.min(MAX_RANGE)).read_to_end(&mut out)?;
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_lists_and_reads_and_stays_inside_its_root() {
        let dir = std::env::temp_dir().join(format!("lc-browse-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("Trips/Iceland")).unwrap();
        fs::write(dir.join("Trips/Iceland/DSC_0001.NEF"), b"0123456789").unwrap();
        let mut b = FolderBrowser::new(&dir).unwrap();
        let trips = b.folder("Trips").unwrap();
        let listed = b.list(&trips).unwrap();
        assert_eq!(listed.folders, vec![("Trips/Iceland".to_string(), "Iceland".to_string())]);
        let files = b.list("Trips/Iceland").unwrap().files;
        assert_eq!((files[0].name.as_str(), files[0].size), ("DSC_0001.NEF", 10));
        assert_eq!(b.read_range(&files[0].id, 2, 3).unwrap(), b"234");
        assert_eq!(b.read_range(&files[0].id, 8, 100).unwrap(), b"89");
        assert_eq!(b.content_hash(&files[0].id).unwrap(), None);
        assert!(matches!(b.folder("Nope"), Err(CloudError::NotFound)));
        assert!(b.list("../").is_err() && b.read_range("../x", 0, 1).is_err() && b.folder("/etc/..").is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
