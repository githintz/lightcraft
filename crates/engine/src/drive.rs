//! The drive (opt-in, on top of cloud backup; `docs/cloud-drive.md`): files and folders the user
//! stores directly in the library's storage accounts, next to the library's own backup. They use
//! the same vault, placement, encryption and manifest as the backup, under entry ids starting
//! with [`PREFIX`]; the backup's entries ([`is_library_entry`]) are shown read-only. The commands
//! are in `cmd/drive.rs`; `lightcraft-cli drive` serves a web dashboard over them.
//!
//! ```text
//! LightCraft Library/cloud/
//!   manifest.enc   the manifest (shared with the backup), saved after every change
//!   drive.json     dates and stars of drive items (also stored on every account as `meta.drive`)
//!   inbox/         files handed over for upload (`lightcraft-cli drive`), removed once stored
//!   outbox/        downloaded files, decrypted, for an hour or until the transfer is cleared
//! ```
//!
//! The session thread keeps the manifest ([`DriveState::manifest`]) and changes names, folders
//! and the trash in it at once; one worker thread ([`Transfers`]) uploads, downloads, deletes
//! stored copies and saves the manifest to the accounts, merging its changes back
//! ([`Manifest::merge`]) so neither side loses the other's.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use lightcraft_cloud::backup::{Account, AccountSettings, Fleet, Item, Outcome};
use lightcraft_cloud::crypto::{self, OsRandom};
use lightcraft_cloud::{AccountSecret, Availability, ContentId, Entry, Manifest, Quota, Vault, accounts};
use serde::{Deserialize, Serialize};

use crate::cloud::{self, CloudSettings, write_atomic};

/// Entry ids of drive items (files and folders the user stored).
pub const PREFIX: &str = "drive.";
/// The manifest entry holding [`DriveMeta`] on the accounts.
pub const META_ID: &str = "meta.drive";
pub const META_FILE: &str = "drive.json";
pub const INBOX: &str = "inbox";
pub const OUTBOX: &str = "outbox";
/// Longest item name in bytes.
pub const MAX_NAME: usize = 255;
/// Finished transfers listed (older ones are forgotten).
const KEEP_TASKS: usize = 500;
/// How long a downloaded file stays in the outbox.
pub const OUTBOX_AGE: Duration = Duration::from_secs(3600);
/// Largest `drive.json` read back.
const MAX_META: u64 = 64 * 1024 * 1024;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

pub fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis().min(u128::from(u64::MAX)) as u64).unwrap_or(0)
}

/// Entries the library backup owns (and trashes when their file leaves the library).
pub fn is_library_entry(id: &str) -> bool {
    ["catalog.", "original.", "smart."].iter().any(|p| id.starts_with(p)) || id == "library.info" || id == "cloud.settings"
}

pub fn is_drive_entry(id: &str) -> bool {
    id.starts_with(PREFIX)
}

/// What the manifest doesn't record about a drive item.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ItemMeta {
    /// Milliseconds since 1970.
    pub added: u64,
    pub modified: u64,
    pub starred: bool,
    pub trashed_at: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct DriveMeta {
    pub items: BTreeMap<String, ItemMeta>,
}

impl DriveMeta {
    /// Take every item the other copy changed later (another device, or the accounts' copy).
    pub fn merge(&mut self, other: &DriveMeta) {
        for (id, m) in &other.items {
            match self.items.get(id) {
                Some(mine) if mine.modified >= m.modified && mine.trashed_at.unwrap_or(0) >= m.trashed_at.unwrap_or(0) => {}
                _ => {
                    self.items.insert(id.clone(), m.clone());
                }
            }
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TaskState {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

/// One transfer, as listed by `drive.transfers`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Task {
    pub id: String,
    /// "upload", "download", "delete" or "sync".
    pub kind: &'static str,
    pub name: String,
    pub entry: Option<String>,
    pub size: u64,
    pub state: TaskState,
    pub error: Option<String>,
    /// Copies stored (uploads).
    pub copies: usize,
    /// When it was queued, then when it finished (ms).
    pub at: u64,
    /// A finished download's file in the outbox.
    #[serde(skip)]
    pub file: Option<PathBuf>,
    /// The folder an upload goes into (names there are kept unique).
    #[serde(skip)]
    pub parent: Option<String>,
}

/// Work for the drive's worker thread.
pub enum Work {
    Upload {
        entry: Entry,
        copies: u8,
        source: PathBuf,
        staged: bool,
    },
    Download {
        id: String,
        dest: PathBuf,
    },
    /// Delete stored copies (account, provider key, content) of deleted items.
    Purge {
        gone: Vec<(String, String, ContentId)>,
    },
    /// Read the accounts' manifest and fold it in (another device, a restore).
    Pull,
}

/// What the worker needs from the session (taken each time it starts).
pub struct WorkerContext {
    pub vault: Vault,
    pub settings: CloudSettings,
    pub library: PathBuf,
}

/// The worker's queue and results, shared with the session.
#[derive(Default)]
pub struct Transfers {
    queue: Mutex<VecDeque<(String, Work)>>,
    pub tasks: Mutex<Vec<Task>>,
    busy: AtomicBool,
    /// The manifest clock last saved to the accounts.
    pub synced: AtomicU64,
    /// The drive metadata changed since it was last saved to the accounts.
    pub meta_dirty: AtomicBool,
    /// Why the last save to the accounts failed (cleared when one works).
    pub sync_error: Mutex<Option<String>>,
    pub quotas: Mutex<BTreeMap<String, std::result::Result<Quota, String>>>,
    /// Accounts whose credentials changed (rotated refresh tokens), for the vault.
    pub updated: Mutex<Vec<AccountSecret>>,
}

impl Transfers {
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::Relaxed)
    }

    fn unsynced(&self, manifest: &Mutex<Manifest>) -> bool {
        self.meta_dirty.load(Ordering::Relaxed) || lock(manifest).clock() > self.synced.load(Ordering::Relaxed)
    }

    pub fn list(&self) -> Vec<Task> {
        lock(&self.tasks).clone()
    }

    fn set(&self, id: &str, f: impl FnOnce(&mut Task)) {
        if let Some(t) = lock(&self.tasks).iter_mut().find(|t| t.id == id) {
            f(t);
        }
    }

    fn state(&self, id: &str) -> Option<TaskState> {
        lock(&self.tasks).iter().find(|t| t.id == id).map(|t| t.state)
    }

    /// Queue `work`, listed as `task`.
    pub fn push(&self, task: Task, work: Work) {
        let id = task.id.clone();
        {
            let mut tasks = lock(&self.tasks);
            tasks.push(task);
            // forget the oldest finished ones
            let finished = tasks.iter().filter(|t| !matches!(t.state, TaskState::Queued | TaskState::Running)).count();
            if finished > KEEP_TASKS {
                let mut drop = finished - KEEP_TASKS;
                tasks.retain(|t| {
                    if drop > 0 && !matches!(t.state, TaskState::Queued | TaskState::Running) {
                        drop -= 1;
                        if let Some(f) = &t.file {
                            let _ = std::fs::remove_file(f);
                        }
                        return false;
                    }
                    true
                });
            }
        }
        lock(&self.queue).push_back((id, work));
    }

    /// Cancel a queued transfer (one already running finishes).
    pub fn cancel(&self, id: &str) -> bool {
        let mut q = lock(&self.queue);
        let before = q.len();
        q.retain(|(t, _)| t != id);
        let removed = q.len() != before;
        drop(q);
        if removed {
            self.set(id, |t| {
                t.state = TaskState::Cancelled;
                t.at = now_ms();
            });
        }
        removed
    }

    /// Forget finished transfers (and their downloaded files).
    pub fn clear_finished(&self) {
        lock(&self.tasks).retain(|t| {
            let keep = matches!(t.state, TaskState::Queued | TaskState::Running);
            if !keep && let Some(f) = &t.file {
                let _ = std::fs::remove_file(f);
            }
            keep
        });
    }

    fn next(&self) -> Option<(String, Work)> {
        let mut q = lock(&self.queue);
        let n = q.pop_front();
        if n.is_none() {
            // under the queue lock: a push after this sees `busy` false and starts a worker
            self.busy.store(false, Ordering::Relaxed);
        }
        n
    }
}

/// The drive's state in this session (reset when the vault locks or the library changes).
#[derive(Default)]
pub struct DriveState {
    pub loaded: bool,
    /// The manifest: drive items and the backup's entries.
    pub manifest: Arc<Mutex<Manifest>>,
    pub meta: Arc<Mutex<DriveMeta>>,
    pub transfers: Arc<Transfers>,
}

impl std::fmt::Debug for DriveState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriveState").field("loaded", &self.loaded).finish_non_exhaustive()
    }
}

pub fn inbox(library: &Path) -> PathBuf {
    cloud::dir(library).join(INBOX)
}

pub fn outbox(library: &Path) -> PathBuf {
    cloud::dir(library).join(OUTBOX)
}

/// Remove files older than `age` from `dir` (leftovers of an earlier run).
pub fn clean_dir(dir: &Path, age: Duration) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten().take(100_000) {
        let old = e.metadata().ok().and_then(|m| m.modified().ok()).and_then(|t| t.elapsed().ok()).is_none_or(|el| el >= age);
        if old && e.file_type().is_ok_and(|t| t.is_file()) {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

impl DriveState {
    /// Read this device's manifest and drive metadata (once per unlock).
    pub fn load(&mut self, library: &Path, vault: &Vault) {
        if self.loaded {
            return;
        }
        let m = cloud::read_local_manifest(library, vault).unwrap_or_default();
        let meta = std::fs::read(cloud::dir(library).join(META_FILE))
            .ok()
            .filter(|b| b.len() as u64 <= MAX_META)
            .and_then(|b| serde_json::from_slice::<DriveMeta>(&b).ok())
            .unwrap_or_default();
        {
            let mut shared = lock(&self.manifest);
            if let Err(e) = shared.merge(&m) {
                log::warn!("drive: {e}");
            }
        }
        lock(&self.meta).merge(&meta);
        self.transfers.synced.store(m.clock(), Ordering::Relaxed);
        clean_dir(&outbox(library), Duration::ZERO);
        clean_dir(&inbox(library), Duration::from_secs(24 * 3600));
        self.loaded = true;
    }

    /// Save the manifest and metadata in the library (after a change on the session thread).
    pub fn save_local(&self, library: &Path, vault: &Vault) -> Result<(), String> {
        let plain = lock(&self.manifest).to_snapshot().map_err(|e| e.to_string())?;
        let sealed = crypto::encrypt(&vault.data_key, &plain, &mut OsRandom).map_err(|e| e.to_string())?;
        write_atomic(&cloud::dir(library).join(cloud::MANIFEST_FILE), &sealed).map_err(|e| format!("can't save the manifest: {e}"))?;
        let meta = serde_json::to_vec(&*lock(&self.meta)).map_err(|e| e.to_string())?;
        write_atomic(&cloud::dir(library).join(META_FILE), &meta).map_err(|e| format!("can't save {META_FILE}: {e}"))
    }

    /// Changes not yet saved to the accounts.
    pub fn unsynced(&self) -> bool {
        self.transfers.unsynced(&self.manifest)
    }

    /// Start the worker if it isn't running: it does the queued work, then saves the manifest to
    /// the accounts if it changed.
    pub fn kick(&self, ctx: WorkerContext) -> std::io::Result<()> {
        if self.transfers.busy.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let (manifest, meta, transfers) = (self.manifest.clone(), self.meta.clone(), self.transfers.clone());
        let t = transfers.clone();
        let r = std::thread::Builder::new().name("lc-drive".into()).spawn(move || {
            let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| Worker::new(ctx, manifest, meta, t.clone()).run()));
            if r.is_err() {
                *lock(&t.sync_error) = Some("the drive's transfers stopped unexpectedly".into());
                // whatever was running or queued is reported, and the next change starts a new worker
                let mut q = lock(&t.queue);
                q.clear();
                for task in lock(&t.tasks).iter_mut().filter(|x| matches!(x.state, TaskState::Queued | TaskState::Running)) {
                    task.state = TaskState::Failed;
                    task.error = Some("stopped unexpectedly".into());
                }
                t.busy.store(false, Ordering::Relaxed);
            }
        });
        if r.is_err() {
            transfers.busy.store(false, Ordering::Relaxed);
        }
        r.map(|_| ())
    }
}

struct Worker {
    ctx: WorkerContext,
    manifest: Arc<Mutex<Manifest>>,
    meta: Arc<Mutex<DriveMeta>>,
    transfers: Arc<Transfers>,
    fleet: Option<Fleet>,
    quotas_read: bool,
}

impl Worker {
    fn new(ctx: WorkerContext, manifest: Arc<Mutex<Manifest>>, meta: Arc<Mutex<DriveMeta>>, transfers: Arc<Transfers>) -> Worker {
        Worker { ctx, manifest, meta, transfers, fleet: None, quotas_read: false }
    }

    /// The accounts, opened once per run (with the manifest as it is now).
    fn fleet(&mut self) -> Result<&mut Fleet, String> {
        let current = lock(&self.manifest).clone();
        if self.fleet.is_none() {
            let mut list = Vec::new();
            for secret in &self.ctx.vault.accounts {
                match accounts::open(secret, false) {
                    Ok(p) => list.push(Account::new(
                        secret.id.clone(),
                        p,
                        self.ctx.settings.accounts.get(&secret.id).cloned().unwrap_or_else(AccountSettings::default),
                    )),
                    Err(e) => {
                        lock(&self.transfers.quotas).insert(secret.id.clone(), Err(e.to_string()));
                    }
                }
            }
            if list.is_empty() {
                return Err("no storage account could be opened: add one under Storage".into());
            }
            self.fleet = Some(Fleet::new(list, self.ctx.vault.data_key.clone(), self.ctx.settings.device.clone(), Manifest::new()));
        }
        let fleet = self.fleet.as_mut().ok_or("no storage accounts")?;
        fleet.manifest = current;
        Ok(fleet)
    }

    fn read_quotas(&mut self) {
        if self.quotas_read {
            return;
        }
        self.quotas_read = true;
        let transfers = self.transfers.clone();
        let Ok(fleet) = self.fleet() else { return };
        let errors = fleet.refresh_quotas();
        let mut q = lock(&transfers.quotas);
        for (id, e) in errors {
            q.insert(id, Err(e.to_string()));
        }
        for a in &fleet.accounts {
            if let Some(quota) = a.quota {
                q.insert(a.id.clone(), Ok(quota));
            }
        }
    }

    /// Fold the fleet's manifest (with the work's ops) back into the session's.
    fn merge_back(&mut self) -> Result<(), String> {
        let Some(fleet) = &self.fleet else { return Ok(()) };
        lock(&self.manifest).merge(&fleet.manifest).map_err(|e| e.to_string())
    }

    fn run(mut self) {
        loop {
            match self.transfers.next() {
                Some((id, work)) => {
                    if self.transfers.state(&id) != Some(TaskState::Queued) {
                        continue;
                    }
                    self.transfers.set(&id, |t| t.state = TaskState::Running);
                    let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.work(work)))
                        .unwrap_or_else(|_| Err("stopped unexpectedly".into()));
                    self.transfers.set(&id, |t| {
                        t.at = now_ms();
                        match r {
                            Ok((copies, file)) => {
                                t.state = TaskState::Done;
                                t.copies = copies;
                                t.file = file;
                            }
                            Err(e) => {
                                t.state = TaskState::Failed;
                                t.error = Some(e);
                            }
                        }
                    });
                }
                None => {
                    // queue empty: save what changed, then stop unless work came in meanwhile
                    if self.transfers.unsynced(&self.manifest) && !self.transfers.busy.swap(true, Ordering::Relaxed) {
                        let meta_dirty = self.transfers.meta_dirty.load(Ordering::Relaxed);
                        let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.save()))
                            .unwrap_or_else(|_| Err("stopped unexpectedly".into()));
                        let failed = r.is_err();
                        *lock(&self.transfers.sync_error) = r.err();
                        if failed {
                            // tried again with the next change or drive.sync, not in a loop
                            if meta_dirty {
                                self.transfers.meta_dirty.store(true, Ordering::Relaxed);
                            }
                            let q = lock(&self.transfers.queue);
                            if q.is_empty() {
                                self.transfers.busy.store(false, Ordering::Relaxed);
                                break;
                            }
                        }
                        continue;
                    }
                    break;
                }
            }
        }
        self.collect_updates();
    }

    /// Do one piece of work: (copies stored, a downloaded file).
    fn work(&mut self, work: Work) -> Result<(usize, Option<PathBuf>), String> {
        match work {
            Work::Upload { entry, copies, source, staged } => {
                let r = self.upload(&entry, copies, &source);
                if staged {
                    let _ = std::fs::remove_file(&source);
                }
                r.map(|n| (n, None))
            }
            Work::Download { id, dest } => {
                let entry = lock(&self.manifest).get(&id).ok_or("the file is no longer in the drive")?;
                let fleet = self.fleet()?;
                let data = fleet.restore(&entry).map_err(|e| format!("couldn't read it from any storage account: {e}"))?;
                write_atomic(&dest, &data).map_err(|e| format!("can't write the downloaded file: {e}"))?;
                Ok((entry.replicas.len(), Some(dest)))
            }
            Work::Purge { gone } => {
                let fleet = self.fleet()?;
                let errors = fleet.delete_unused(&gone);
                match errors.first() {
                    Some((account, e)) => Err(format!("{} stored copies couldn't be deleted (account {account}: {e})", errors.len())),
                    None => Ok((0, None)),
                }
            }
            Work::Pull => {
                let fleet = self.fleet()?;
                let remote = match fleet.load_manifest() {
                    Ok(m) => m,
                    Err(lightcraft_cloud::CloudError::NotFound) => return Ok((0, None)), // nothing stored yet
                    Err(e) => return Err(format!("couldn't read the accounts' manifest: {e}")),
                };
                let remote_meta = remote.get(META_ID).and_then(|e| fleet.restore(&e).ok());
                let mut shared = lock(&self.manifest);
                shared.merge(&remote).map_err(|e| e.to_string())?;
                drop(shared);
                if let Some(meta) = remote_meta.and_then(|b| serde_json::from_slice::<DriveMeta>(&b).ok()) {
                    lock(&self.meta).merge(&meta);
                }
                Ok((0, None))
            }
        }
    }

    fn upload(&mut self, entry: &Entry, copies: u8, source: &Path) -> Result<usize, String> {
        let size = std::fs::metadata(source).map_err(|e| format!("can't read the file: {e}"))?.len();
        if size > cloud::MAX_FILE {
            return Err(format!("files over {} GB can't be stored yet", cloud::MAX_FILE / 1_000_000_000));
        }
        let data = std::fs::read(source).map_err(|e| format!("can't read the file: {e}"))?;
        self.read_quotas();
        let fleet = self.fleet()?;
        // the folder may have gone meanwhile: the file then lands at the top
        let mut entry = entry.clone();
        if entry.parent.as_deref().is_some_and(|p| fleet.manifest.get(p).is_none_or(|f| f.trashed)) {
            entry.parent = None;
        }
        let r = fleet.store_file(&entry, copies, &data, &mut OsRandom);
        self.merge_back()?;
        match r {
            Ok(Outcome::Stored(on)) => Ok(on.len()),
            Ok(Outcome::Unchanged) => Ok(lock(&self.manifest).get(&entry.id).map(|e| e.replicas.len()).unwrap_or(0)),
            Err(lightcraft_cloud::CloudError::QuotaExceeded) => Err("there is no room left on your storage accounts".into()),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The drive metadata (if it changed) and the manifest, to every account and the library.
    fn save(&mut self) -> Result<(), String> {
        self.transfers.meta_dirty.store(false, Ordering::Relaxed);
        let meta = serde_json::to_vec(&*lock(&self.meta)).map_err(|e| e.to_string())?;
        let library = self.ctx.library.clone();
        let (shared, transfers, shared_meta) = (self.manifest.clone(), self.transfers.clone(), self.meta.clone());
        let fleet = self.fleet()?;
        let old = fleet.manifest.get(META_ID);
        let item = Item { id: META_ID.into(), name: META_FILE.into(), level: Availability::Instant, copies: 1, everywhere: true };
        if let Ok(Outcome::Stored(_)) = fleet.backup(&item, &meta, &mut OsRandom)
            && let Some(old) = old.filter(|o| o.content.as_ref() != Some(&ContentId::of(&meta)))
            && let Some(content) = old.content.clone()
        {
            // the previous copy of the metadata isn't needed any more
            let gone: Vec<_> = old.replicas.iter().map(|(a, k)| (a.clone(), k.clone(), content.clone())).collect();
            let _ = fleet.delete_unused(&gone);
        }
        {
            let mut shared = lock(&shared);
            shared.merge(&fleet.manifest).map_err(|e| e.to_string())?;
            fleet.manifest = shared.clone();
        }
        let errors = fleet.save_manifest(&mut OsRandom).map_err(|e| format!("couldn't save the drive to any storage account: {e}"))?;
        {
            let mut q = lock(&transfers.quotas);
            for (id, e) in &errors {
                q.insert(id.clone(), Err(e.to_string()));
            }
        }
        // the session's copy: it holds this save and anything changed since
        let plain = lock(&shared).to_snapshot().map_err(|e| e.to_string())?;
        let sealed = crypto::encrypt(&fleet.data_key, &plain, &mut OsRandom).map_err(|e| e.to_string())?;
        write_atomic(&cloud::dir(&library).join(cloud::MANIFEST_FILE), &sealed).map_err(|e| format!("can't save the manifest: {e}"))?;
        if let Ok(now) = serde_json::to_vec(&*lock(&shared_meta)) {
            let _ = write_atomic(&cloud::dir(&library).join(META_FILE), &now);
        }
        transfers.synced.fetch_max(fleet.manifest.clock(), Ordering::Relaxed);
        Ok(())
    }

    /// Rotated refresh tokens, for the session's vault.
    fn collect_updates(&self) {
        let Some(fleet) = &self.fleet else { return };
        for a in &fleet.accounts {
            let Some(secret) = self.ctx.vault.accounts.iter().find(|s| s.id == a.id) else { continue };
            if let Some(u) = accounts::updated_secret(secret, a.provider.as_ref()) {
                lock(&self.transfers.updated).push(u);
            }
        }
    }
}

/// Clean an item name: no path separators or control characters, at most [`MAX_NAME`] bytes.
pub fn clean_name(name: &str) -> Option<String> {
    let mut out: String = name.trim().chars().map(|c| if matches!(c, '/' | '\\') { '-' } else { c }).filter(|c| !c.is_control()).collect();
    if out.len() > MAX_NAME {
        let mut end = MAX_NAME;
        while !out.is_char_boundary(end) {
            end -= 1;
        }
        out.truncate(end);
    }
    let out = out.trim().to_string();
    (!out.is_empty() && out != "." && out != "..").then_some(out)
}

/// `name`, or "name (1).ext" and so on when a sibling already has it (ignoring case), like Drive.
pub fn unique_name(name: &str, taken: &[String]) -> String {
    let lower: std::collections::HashSet<String> = taken.iter().map(|t| t.to_lowercase()).collect();
    if !lower.contains(&name.to_lowercase()) {
        return name.to_string();
    }
    let (base, ext) = match name.rfind('.') {
        Some(i) if i > 0 => name.split_at(i),
        _ => (name, ""),
    };
    for i in 1..100_000u32 {
        let candidate = format!("{base} ({i}){ext}");
        if !lower.contains(&candidate.to_lowercase()) {
            return candidate;
        }
    }
    format!("{base} ({}){ext}", now_ms())
}

/// The last component of a stored path (originals are named by their full path).
pub fn base_name(path: &str) -> &str {
    path.rsplit(['/', '\\']).find(|s| !s.is_empty()).unwrap_or(path)
}

/// The folder part of a stored path ("" for none).
pub fn dir_name(path: &str) -> &str {
    let trimmed = path.trim_end_matches(['/', '\\']);
    match trimmed.rfind(['/', '\\']) {
        Some(0) => trimmed.get(..1).unwrap_or(""),
        Some(i) => trimmed.get(..i).unwrap_or(""),
        None => "",
    }
}

/// A stable id for a virtual library folder (`lib.dir.<16 hex>`).
pub fn library_dir_id(dir: &str) -> String {
    let c = ContentId::of(dir.as_bytes());
    format!("lib.dir.{}", c.as_str().get(..16).unwrap_or(c.as_str()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_cleaned_and_made_unique() {
        assert_eq!(clean_name("  a/b\\c\u{7}.txt "), Some("a-b-c.txt".into()));
        assert_eq!(clean_name(".."), None);
        assert_eq!(clean_name("   "), None);
        let long = "é".repeat(200);
        let cut = clean_name(&long).unwrap();
        assert!(cut.len() <= MAX_NAME && cut.chars().all(|c| c == 'é'));
        let taken = vec!["Report.pdf".to_string(), "report (1).pdf".to_string()];
        assert_eq!(unique_name("report.pdf", &taken), "report (2).pdf");
        assert_eq!(unique_name("notes", &["Notes".to_string()]), "notes (1)");
        assert_eq!(unique_name("new.txt", &taken), "new.txt");
    }

    #[test]
    fn stored_paths_split_on_either_separator() {
        assert_eq!(base_name("/Users/me/Pictures/IMG_1.CR3"), "IMG_1.CR3");
        assert_eq!(base_name("C:\\Photos\\Trip\\a.nef"), "a.nef");
        assert_eq!(dir_name("C:\\Photos\\Trip\\a.nef"), "C:\\Photos\\Trip");
        assert_eq!(dir_name("/a.jpg"), "/");
        assert_eq!(dir_name("a.jpg"), "");
        assert!(library_dir_id("/x").starts_with("lib.dir.") && library_dir_id("/x").len() == 24);
    }

    #[test]
    fn metadata_merge_keeps_the_later_change() {
        let mut a = DriveMeta::default();
        a.items.insert("drive.1".into(), ItemMeta { added: 1, modified: 5, starred: true, trashed_at: None });
        let mut b = DriveMeta::default();
        b.items.insert("drive.1".into(), ItemMeta { added: 1, modified: 3, starred: false, trashed_at: None });
        b.items.insert("drive.2".into(), ItemMeta { added: 2, modified: 2, ..ItemMeta::default() });
        a.merge(&b);
        assert!(a.items["drive.1"].starred);
        assert!(a.items.contains_key("drive.2"));
    }
}
