//! Scan folders through a [`Browse`]: every photo and video under the roots, with its size, the
//! provider's content hash, a hash of its first 64 KiB, and the metadata at the start of the file
//! (capture time to the millisecond, camera, serial number, pixel size), read with ranged requests.
//!
//! Workers run in parallel, one [`Browse`] each. What a scan learns is appended to a cache file
//! as it goes (JSON lines), so a stopped scan (cancelled, the computer asleep, OpenDrive's daily
//! download limit) goes on where it stopped, and a later scan reads only new or changed files.

use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};

use lightcraft_cloud::{Browse, BrowseFile, CloudError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::Kind;

/// Bytes read from the start of a photo for its metadata.
pub const HEAD_BYTES: u64 = 256 * 1024;
/// Bytes read again when the first read held no capture time (a raw with large maker notes).
pub const MORE_BYTES: u64 = 2 * 1024 * 1024;
/// Bytes at the start of a file whose hash [`Record::head`] is.
pub const HEAD_HASH_BYTES: usize = 64 * 1024;
/// Most files a scan takes (instead of a silently partial scan).
pub const MAX_FILES: usize = 5_000_000;
/// Deepest folder walked.
pub const MAX_DEPTH: usize = 64;

/// One file under a root, and what was learned about it.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Record {
    /// The root folder it was found under, as given (`Photos A`).
    pub root: String,
    /// Its path under the root (`Iceland 2019/Day 1/DSC_0001.NEF`).
    pub path: String,
    /// The provider's id for the file.
    pub id: String,
    pub size: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub modified: Option<String>,
    pub kind: Option<Kind>,
    /// The provider's content hash (OpenDrive: MD5).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hash: Option<String>,
    /// SHA-256 (hex, first 16 bytes) of the first [`HEAD_HASH_BYTES`] of the file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head: Option<String>,
    /// Capture time, ISO 8601 as the camera recorded it (milliseconds and offset when known).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub make: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub serial: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub width: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub height: Option<u32>,
    /// What went wrong reading it (the file is still listed; a later scan tries again).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Record {
    /// The file name.
    pub fn name(&self) -> &str {
        self.path.rsplit('/').next().unwrap_or(&self.path)
    }

    /// The folder it is in, under its root (`""` for the root itself).
    pub fn dir(&self) -> &str {
        self.path.rsplit_once('/').map_or("", |(d, _)| d)
    }
}

/// What a scan reads.
#[derive(Clone, Debug)]
pub struct ScanOptions {
    /// Folder paths from the account's root (`Photos A`, `Archive/Photos B`).
    pub roots: Vec<String>,
    /// The cache file (JSON lines), made when missing.
    pub cache: PathBuf,
}

/// A scan's progress, shared with whoever shows it; `cancel` stops it between files.
#[derive(Debug, Default)]
pub struct Progress {
    /// Files found so far (photos, videos, sidecars).
    pub found: AtomicU64,
    /// Folders listed so far.
    pub folders: AtomicU64,
    /// Files to read (not already in the cache).
    pub to_read: AtomicU64,
    pub read: AtomicU64,
    /// Files whose reading failed (listed with their error).
    pub failed: AtomicU64,
    /// Bytes of file content downloaded.
    pub bytes: AtomicU64,
    /// Set once every folder was listed.
    pub listed: AtomicBool,
    pub cancel: AtomicBool,
}

/// How a scan ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Every file was read (some may have failed: see their `error`).
    Done,
    Cancelled,
    /// Something every request would hit (signed out, the daily download limit): the scan
    /// stopped, and goes on from here next time.
    Halted(String),
}

/// The records of every file under the roots (cached ones as they were), and how it ended.
#[derive(Debug)]
pub struct Scanned {
    pub records: Vec<Record>,
    pub stop: Stop,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A key a cached record matches by: root and path.
type Key = (String, String);

/// The cache: the last record of each file (later lines win; lines that don't parse are skipped).
pub fn load_cache(path: &Path) -> HashMap<Key, Record> {
    let mut out = HashMap::new();
    let Ok(f) = fs::File::open(path) else { return out };
    for line in BufReader::new(f).lines().map_while(std::result::Result::ok) {
        if let Ok(r) = serde_json::from_str::<Record>(&line) {
            out.insert((r.root.clone(), r.path.clone()), r);
        }
    }
    out
}

/// Every photo, video and sidecar under `root`, as `(path under the root, file)`.
fn walk(b: &mut dyn Browse, root: &str, progress: &Progress, out: &mut Vec<(String, String, BrowseFile)>) -> Result<(), CloudError> {
    let top = b.folder(root).map_err(|e| match e {
        CloudError::NotFound => CloudError::Io(format!("there is no folder {root:?} in the account")),
        e => e,
    })?;
    let mut queue: VecDeque<(String, String, usize)> = VecDeque::from([(top, String::new(), 0)]);
    while let Some((id, rel, depth)) = queue.pop_front() {
        if progress.cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let dir = b.list(&id)?;
        progress.folders.fetch_add(1, Ordering::Relaxed);
        for f in dir.files {
            if Kind::of(&f.name).is_none() || f.name.starts_with('.') {
                continue;
            }
            if out.len() >= MAX_FILES {
                return Err(CloudError::Io(format!("more than {MAX_FILES} files under the folders")));
            }
            let path = if rel.is_empty() { f.name.clone() } else { format!("{rel}/{}", f.name) };
            out.push((root.to_string(), path, f));
            progress.found.fetch_add(1, Ordering::Relaxed);
        }
        if depth < MAX_DEPTH {
            for (fid, name) in dir.folders {
                // hidden folders (.thumbnails, .trash, @eaDir) are a system's, not trips
                if name.starts_with('.') || name.starts_with('@') {
                    continue;
                }
                let path = if rel.is_empty() { name } else { format!("{rel}/{name}") };
                queue.push_back((fid, path, depth + 1));
            }
        }
    }
    Ok(())
}

/// Furthest a TIFF's tag directory may be for [`read_one`] to fetch it (the sparse copy is this big).
const MAX_SPARSE: u64 = 256 * 1024 * 1024;

/// Where a TIFF's first tag directory is (classic TIFF, either byte order), from its header.
fn tiff_ifd0(head: &[u8]) -> Option<u64> {
    let h: [u8; 8] = head.get(..8)?.try_into().ok()?;
    match (&h[..2], h[2], h[3]) {
        (b"II", 42, 0) => Some(u32::from_le_bytes([h[4], h[5], h[6], h[7]]) as u64),
        (b"MM", 0, 42) => Some(u32::from_be_bytes([h[4], h[5], h[6], h[7]]) as u64),
        _ => None,
    }
}

fn short_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes).iter().take(16).map(|b| format!("{b:02x}")).collect()
}

/// An error every later request would hit too: stop the scan instead of failing every file.
fn halts(e: &CloudError) -> bool {
    match e {
        CloudError::Denied(_) | CloudError::QuotaExceeded => true,
        CloudError::Io(m) => m.contains("bandwidth") || m == "cancelled",
        _ => false,
    }
}

/// Fill `r` (whose `id`, `size` and `kind` are set) from the file.
fn read_one(b: &mut dyn Browse, r: &mut Record, progress: &Progress) -> Result<(), CloudError> {
    let Some(kind) = r.kind else { return Ok(()) };
    if kind == Kind::Sidecar {
        return Ok(());
    }
    r.hash = b.content_hash(&r.id)?;
    let id = r.id.clone();
    let fetch = |b: &mut dyn Browse, off: u64, len: u64| -> Result<Vec<u8>, CloudError> {
        let bytes = b.read_range(&id, off, len)?;
        progress.bytes.fetch_add(bytes.len() as u64, Ordering::Relaxed);
        Ok(bytes)
    };
    if kind.is_photo() {
        let mut head = fetch(b, 0, HEAD_BYTES)?;
        r.head = Some(short_hash(head.get(..HEAD_HASH_BYTES.min(head.len())).unwrap_or_default()));
        let mut meta = lightcraft_meta::extract(&head);
        if meta.capture_time.is_none() && r.size > head.len() as u64 {
            match tiff_ifd0(&head).filter(|o| *o >= head.len() as u64 && *o < r.size && *o <= MAX_SPARSE) {
                // a TIFF whose tag directory comes after its data (some converters write them so):
                // read it where it is and parse it at its real offset
                Some(at) => {
                    let window = fetch(b, at, HEAD_BYTES)?;
                    let mut sparse = vec![0u8; usize::try_from(at).unwrap_or(0)];
                    sparse.get_mut(..head.len()).unwrap_or_default().copy_from_slice(&head);
                    sparse.extend_from_slice(&window);
                    meta = lightcraft_meta::extract(&sparse);
                }
                // a raw with large maker notes before its Exif data
                None if kind.is_raw() => {
                    head = fetch(b, 0, MORE_BYTES)?;
                    meta = lightcraft_meta::extract(&head);
                }
                None => {}
            }
        }
        r.taken = meta.capture_time.map(|t| t.to_iso());
        (r.make, r.model, r.serial) = (meta.make, meta.model, meta.serial_number);
        (r.width, r.height) = (meta.width, meta.height);
    } else {
        let head = fetch(b, 0, HEAD_HASH_BYTES as u64)?;
        r.head = Some(short_hash(&head));
        if kind == Kind::Video {
            let size = r.size;
            let mut read = |off: u64, len: u64| -> Result<Vec<u8>, CloudError> {
                // the first header is already here
                if off.saturating_add(len) <= head.len() as u64 {
                    return Ok(head.get(off as usize..(off + len) as usize).unwrap_or_default().to_vec());
                }
                fetch(b, off, len)
            };
            r.taken = crate::video::creation_time(size, &mut read)?.map(crate::video::iso);
        }
    }
    Ok(())
}

/// Scan `opts.roots` with one worker per browser (the first also lists the folders). Records come
/// back sorted by root and path.
pub fn scan(mut browsers: Vec<Box<dyn Browse>>, opts: &ScanOptions, progress: &Progress) -> Result<Scanned, String> {
    if browsers.is_empty() {
        return Err("no account to scan".into());
    }
    if opts.roots.is_empty() {
        return Err("give at least one folder to scan".into());
    }
    let cache = load_cache(&opts.cache);
    let mut files = Vec::new();
    for root in &opts.roots {
        let root = root.trim_matches('/');
        if let Some(b) = browsers.first_mut() {
            walk(b.as_mut(), root, progress, &mut files).map_err(|e| format!("listing {root:?}: {e}"))?;
        }
    }
    progress.listed.store(true, Ordering::Relaxed);
    if progress.cancel.load(Ordering::Relaxed) {
        return Ok(Scanned { records: Vec::new(), stop: Stop::Cancelled });
    }

    // what the cache already knows (same file, unchanged) isn't read again
    let mut done: Vec<Record> = Vec::new();
    let mut todo: VecDeque<Record> = VecDeque::new();
    for (root, path, f) in files {
        let key = (root.clone(), path.clone());
        match cache.get(&key) {
            Some(c) if c.id == f.id && c.size == f.size && c.modified == f.modified && c.error.is_none() => done.push(c.clone()),
            _ => todo.push_back(Record {
                root,
                path: path.clone(),
                id: f.id,
                size: f.size,
                modified: f.modified,
                kind: Kind::of(&path),
                ..Record::default()
            }),
        }
    }
    progress.to_read.store(todo.len() as u64, Ordering::Relaxed);

    if let Some(dir) = opts.cache.parent().filter(|d| !d.as_os_str().is_empty()) {
        fs::create_dir_all(dir).map_err(|e| format!("can't make {}: {e}", dir.display()))?;
    }
    let mut out =
        fs::OpenOptions::new().create(true).append(true).open(&opts.cache).map_err(|e| format!("can't write {}: {e}", opts.cache.display()))?;
    let queue = Arc::new(Mutex::new(todo));
    let halted: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::channel::<Record>();
    std::thread::scope(|scope| {
        for b in browsers.iter_mut() {
            let (queue, halted, tx) = (queue.clone(), halted.clone(), tx.clone());
            scope.spawn(move || {
                loop {
                    if progress.cancel.load(Ordering::Relaxed) || lock(&halted).is_some() {
                        return;
                    }
                    let Some(mut r) = lock(&queue).pop_front() else { return };
                    match read_one(b.as_mut(), &mut r, progress) {
                        Ok(()) => {}
                        Err(e) if halts(&e) => {
                            lock(&halted).get_or_insert_with(|| e.to_string());
                            return;
                        }
                        Err(e) => {
                            r.error = Some(e.to_string());
                            progress.failed.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                    progress.read.fetch_add(1, Ordering::Relaxed);
                    if tx.send(r).is_err() {
                        return;
                    }
                }
            });
        }
        drop(tx);
        // one writer: each record as it comes, so a stopped scan keeps what it learned
        for r in rx {
            if let Ok(line) = serde_json::to_string(&r) {
                let _ = writeln!(out, "{line}");
            }
            done.push(r);
        }
    });
    let _ = out.flush();
    let stop = match lock(&halted).take() {
        Some(e) => Stop::Halted(e),
        None if progress.cancel.load(Ordering::Relaxed) => Stop::Cancelled,
        None => Stop::Done,
    };
    if stop == Stop::Done {
        compact(&opts.cache, &done).map_err(|e| format!("can't rewrite {}: {e}", opts.cache.display()))?;
    }
    done.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
    Ok(Scanned { records: done, stop })
}

/// Rewrite the cache with only `records` (files that went away leave it).
fn compact(path: &Path, records: &[Record]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut f = fs::File::create(&tmp)?;
    for r in records {
        if let Ok(line) = serde_json::to_string(r) {
            writeln!(f, "{line}")?;
        }
    }
    f.sync_all()?;
    fs::rename(&tmp, path)
}
