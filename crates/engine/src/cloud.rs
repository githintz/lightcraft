//! Cloud backup (opt-in, `docs/cloud-storage.md` phase 3): the library's storage accounts, kept
//! in a passphrase-sealed vault in the library, one-way backup of the catalog, originals and
//! smart previews to them, and restoring a whole library from them on another machine. Native
//! only; the commands are in `cmd/cloud.rs`.
//!
//! ```text
//! LightCraft Library/cloud/
//!   vault.enc       the vault: the data key and every account's credentials (Argon2id + AES-GCM)
//!   settings.json   per-account caps, speed and priority, copies, device id (nothing secret)
//!   manifest.enc    this device's copy of the manifest (encrypted under the data key)
//! ```
//!
//! Every account holds the same `vault.enc`, the manifest, an `account.json` naming its own id in
//! the vault, and the encrypted blobs; so one account's credentials plus the passphrase restore
//! everything. Network work runs on worker threads ([`CloudJob`]); the UI never waits on it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use lightcraft_cloud::backup::{self, Account, AccountSettings, Fleet, Item, Outcome};
use lightcraft_cloud::crypto::{self, OsRandom};
use lightcraft_cloud::{AccountSecret, Availability, CloudError, Manifest, ObjectName, Quota, Vault, VaultParams, accounts};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The library's cloud folder.
pub const DIR: &str = "cloud";
pub const VAULT_FILE: &str = "vault.enc";
pub const SETTINGS_FILE: &str = "settings.json";
pub const MANIFEST_FILE: &str = "manifest.enc";
/// Where restored originals that lived outside the library go, inside the restored library.
pub const RESTORED_ORIGINALS: &str = "Restored Originals";
/// Library files backed up with the catalog (to every account).
pub const LIBRARY_FILES: &[&str] = &["catalog.snap", "catalog.log", "presets.json", "prefs.json", "view.json"];
/// The object naming an account's own id in the vault.
pub const ACCOUNT_MARKER: &str = "account.json";
/// Largest file backed up (whole files are read into memory and encrypted in this version).
pub const MAX_FILE: u64 = 4 * 1024 * 1024 * 1024;
/// Shortest passphrase accepted for a new vault.
pub const MIN_PASSPHRASE: usize = 8;
/// Failures listed in a job's result (the count is always complete).
const MAX_LISTED_FAILURES: usize = 100;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What the user set (nothing secret).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct CloudSettings {
    /// This device's id in manifest stamps.
    pub device: String,
    pub accounts: BTreeMap<String, AccountSettings>,
    /// Copies of each original, on different accounts (different providers first).
    pub original_copies: u8,
    pub smart_previews: bool,
    /// OAuth client ids by provider (`gdrive`, `onedrive`, `dropbox`), to prefill sign-in.
    pub client_ids: BTreeMap<String, String>,
    pub last_backup: Option<String>,
    /// This library joined a drive another computer set up (`cloud.join`).
    pub joined: bool,
    /// This library has backed up to the accounts at least once.
    pub backed_up: bool,
}

impl Default for CloudSettings {
    fn default() -> Self {
        CloudSettings {
            device: String::new(),
            accounts: BTreeMap::new(),
            original_copies: 1,
            smart_previews: true,
            client_ids: BTreeMap::new(),
            last_backup: None,
            joined: false,
            backed_up: false,
        }
    }
}

/// A backup, restore or quota refresh in progress (or done).
#[derive(Debug, Default)]
pub struct CloudJob {
    /// "backup", "restore" or "refresh".
    pub what: &'static str,
    pub total: AtomicUsize,
    pub done: AtomicUsize,
    pub stored: AtomicUsize,
    pub unchanged: AtomicUsize,
    pub failed: AtomicUsize,
    pub bytes: AtomicU64,
    pub cancel: AtomicBool,
    pub finished: AtomicBool,
    /// Why the whole job failed.
    pub error: Mutex<Option<String>>,
    /// Files that failed: (name, why), at most [`MAX_LISTED_FAILURES`].
    pub failures: Mutex<Vec<(String, String)>>,
    /// Account quotas (or why they couldn't be read), by account id.
    pub quotas: Mutex<BTreeMap<String, std::result::Result<Quota, String>>>,
    /// Accounts whose credentials changed (rotated refresh tokens), for the vault.
    pub updated: Mutex<Vec<AccountSecret>>,
    /// Restore: the restored library's folder and the folders to relink originals from.
    pub restored: Mutex<Option<(PathBuf, Vec<PathBuf>)>>,
    /// Set once the drive was told about this finished backup.
    pub drive_told: AtomicBool,
}

impl CloudJob {
    pub fn new(what: &'static str) -> CloudJob {
        CloudJob { what, ..CloudJob::default() }
    }

    fn fail(&self, name: &str, why: impl std::fmt::Display) {
        self.failed.fetch_add(1, Ordering::Relaxed);
        let mut f = lock(&self.failures);
        if f.len() < MAX_LISTED_FAILURES {
            f.push((name.to_string(), why.to_string()));
        }
    }

    pub fn running(&self) -> bool {
        !self.finished.load(Ordering::Relaxed)
    }

    pub fn json(&self) -> Value {
        json!({
            "what": self.what,
            "total": self.total.load(Ordering::Relaxed),
            "done": self.done.load(Ordering::Relaxed),
            "stored": self.stored.load(Ordering::Relaxed),
            "unchanged": self.unchanged.load(Ordering::Relaxed),
            "failed": self.failed.load(Ordering::Relaxed),
            "bytes": self.bytes.load(Ordering::Relaxed),
            "running": self.running(),
            "cancelled": self.cancel.load(Ordering::Relaxed),
            "error": lock(&self.error).clone(),
            "failures": lock(&self.failures).iter().map(|(n, e)| json!({"name": n, "error": e})).collect::<Vec<_>>(),
        })
    }
}

/// A sign-in running in the browser.
#[derive(Debug, Default)]
pub struct SignIn {
    pub provider: String,
    pub label: String,
    pub url: String,
    /// Signing in again to this account: its credentials are replaced and its id kept.
    pub account: Option<String>,
    pub client: Mutex<Option<lightcraft_cloud::oauth::OAuthClient>>,
    pub result: Mutex<Option<std::result::Result<String, String>>>,
    pub cancel: Arc<AtomicBool>,
    /// Set once the account was added to the vault (or the failure reported).
    pub taken: AtomicBool,
}

/// A scan of the user's folders running on worker threads (`cloud.scanFolders`).
#[derive(Debug, Default)]
pub struct ScanJob {
    pub progress: lightcraft_organize::scan::Progress,
    /// The vault account scanned (`None`: a folder on this computer).
    pub account: Option<String>,
    pub roots: Vec<String>,
    /// Stops requests in flight when the scan is cancelled.
    pub net_cancel: Arc<AtomicBool>,
    /// How it ended: `(files, how)` or what went wrong; `None` while it runs.
    pub result: Mutex<Option<std::result::Result<(usize, lightcraft_organize::scan::Stop), String>>>,
    /// The account's refresh token after the scan (OpenDrive replaces it on every refresh).
    pub token: Mutex<Option<String>>,
}

impl ScanJob {
    pub fn running(&self) -> bool {
        lock(&self.result).is_none()
    }
}

/// The library's cloud state in this session.
#[derive(Default)]
pub struct CloudState {
    /// A scan of the user's own folders (`cloud.scanFolders`), apart from backups.
    pub scan: Option<Arc<ScanJob>>,
    /// OpenDrive's API root for sign-ins that don't name one (tests point it at a local fake);
    /// `None`: OpenDrive's own.
    pub opendrive_api: Option<String>,
    /// The unlocked vault (`None` while locked or not set up).
    pub vault: Option<Vault>,
    /// Kept while unlocked, to seal the vault again when accounts change.
    passphrase: Option<String>,
    pub settings: CloudSettings,
    pub job: Option<Arc<CloudJob>>,
    pub sign_in: Option<Arc<SignIn>>,
    pub quotas: BTreeMap<String, std::result::Result<Quota, String>>,
    /// Files the user stores directly (`drive.*` commands), sharing the manifest with backups.
    pub drive: crate::drive::DriveState,
}

impl std::fmt::Debug for CloudState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudState").field("unlocked", &self.vault.is_some()).field("settings", &self.settings).finish_non_exhaustive()
    }
}

pub fn dir(library: &Path) -> PathBuf {
    library.join(DIR)
}

/// Write `data` to `path` atomically (a temp file, synced, renamed over it).
pub fn write_atomic(path: &Path, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // unique per call: two threads may save the same file at once (the drive's worker and the session)
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let tmp = path.with_extension(format!("tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    let result = (|| {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(data)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

fn random_id(prefix: &str) -> Result<String, CloudError> {
    let mut b = [0u8; 8];
    crypto::Random::fill(&mut OsRandom, &mut b)?;
    Ok(format!("{prefix}{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
}

impl CloudState {
    pub fn configured(library: &Path) -> bool {
        dir(library).join(VAULT_FILE).is_file()
    }

    pub fn unlocked(&self) -> bool {
        self.vault.is_some()
    }

    fn load_settings(library: &Path) -> CloudSettings {
        std::fs::read(dir(library).join(SETTINGS_FILE)).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save_settings(&self, library: &Path) -> std::io::Result<()> {
        let data = serde_json::to_vec_pretty(&self.settings).map_err(std::io::Error::other)?;
        write_atomic(&dir(library).join(SETTINGS_FILE), &data)
    }

    /// Seal the vault under the passphrase and write it to the library.
    pub fn save_vault(&self, library: &Path) -> Result<Vec<u8>, String> {
        let (Some(vault), Some(pass)) = (&self.vault, &self.passphrase) else { return Err("the cloud vault is locked".into()) };
        let sealed = vault.seal(pass, VaultParams::default(), &mut OsRandom).map_err(|e| e.to_string())?;
        write_atomic(&dir(library).join(VAULT_FILE), &sealed).map_err(|e| format!("can't save the cloud vault: {e}"))?;
        Ok(sealed)
    }

    /// Create the vault (a new data key, no accounts) with `passphrase`.
    pub fn setup(&mut self, library: &Path, passphrase: &str) -> Result<(), String> {
        if Self::configured(library) {
            return Err("cloud backup is already set up for this library (unlock it instead)".into());
        }
        if passphrase.chars().count() < MIN_PASSPHRASE {
            return Err(format!("the passphrase needs at least {MIN_PASSPHRASE} characters"));
        }
        self.vault = Some(Vault::new(&mut OsRandom).map_err(|e| e.to_string())?);
        self.passphrase = Some(passphrase.to_string());
        self.settings = CloudSettings { device: random_id("dev-").map_err(|e| e.to_string())?, ..CloudSettings::default() };
        self.save_vault(library)?;
        self.save_settings(library).map_err(|e| format!("can't save cloud settings: {e}"))?;
        Ok(())
    }

    pub fn unlock(&mut self, library: &Path, passphrase: &str) -> Result<(), String> {
        let data = std::fs::read(dir(library).join(VAULT_FILE)).map_err(|e| format!("cloud backup isn't set up for this library ({e})"))?;
        let vault = Vault::open(&data, passphrase).map_err(|e| e.to_string())?;
        self.vault = Some(vault);
        self.passphrase = Some(passphrase.to_string());
        self.settings = Self::load_settings(library);
        if self.settings.device.is_empty() {
            self.settings.device = random_id("dev-").map_err(|e| e.to_string())?;
            let _ = self.save_settings(library);
        }
        Ok(())
    }

    /// Use the drive (and storage) another computer set up: the vault from `source`, any one of
    /// its accounts, opened with `passphrase`, becomes this library's (sealed with the same
    /// passphrase), with `source`'s fresh credentials, a new device id and `joined` set. The
    /// drive is then read from the accounts (`drive.*` load it). The labels of the accounts.
    pub fn join(&mut self, library: &Path, source: AccountSecret, passphrase: &str) -> Result<Vec<String>, String> {
        if Self::configured(library) {
            return Err("this library already has its own cloud vault: join from a library that isn't set up yet".into());
        }
        let source = accounts::sign_in(source, &lightcraft_cloud::Net::default()).map_err(|e| format!("can't sign in to the account: {e}"))?;
        let mut provider = accounts::open(&source, false).map_err(|e| format!("can't open the account: {e}"))?;
        let listed = provider.list().map_err(|e| format!("can't list the account: {e}"))?;
        let find = |name: &str| listed.iter().find(|l| l.name.as_str() == name).map(|l| l.remote.clone());
        let vault_ref = find(backup::VAULT_NAME).ok_or("this account holds no LightCraft drive or backup (no vault.enc)")?;
        let sealed = provider.get(&vault_ref, lightcraft_cloud::vault::MAX_VAULT as u64).map_err(|e| e.to_string())?;
        let mut vault = Vault::open(&sealed, passphrase).map_err(|e| e.to_string())?;
        let marker = find(ACCOUNT_MARKER).and_then(|r| provider.get(&r, 64 * 1024).ok()).and_then(|b| serde_json::from_slice::<Marker>(&b).ok());
        if let Some(a) = marker.as_ref().and_then(|m| vault.accounts.iter_mut().find(|a| a.id == m.id)) {
            a.config = source.config.clone();
        }
        // the accounts' size limits, speeds and pauses as the other device set them
        let mut account_settings = marker.map(|m| m.accounts).unwrap_or_default();
        account_settings.retain(|id, _| vault.accounts.iter().any(|a| &a.id == id));
        // the sign-in this account used just now (a rotated token) is the one to keep
        let rotated = vault.accounts.iter().find(|a| a.config == source.config).and_then(|a| accounts::updated_secret(a, provider.as_ref()));
        if let Some(u) = rotated
            && let Some(a) = vault.accounts.iter_mut().find(|a| a.id == u.id)
        {
            *a = u;
        }
        let labels = vault.accounts.iter().map(|a| a.label.clone()).collect();
        self.vault = Some(vault);
        self.passphrase = Some(passphrase.to_string());
        self.settings = CloudSettings {
            device: random_id("dev-").map_err(|e| e.to_string())?,
            joined: true,
            accounts: account_settings,
            ..CloudSettings::default()
        };
        self.drive = Default::default();
        self.save_vault(library)?;
        self.save_settings(library).map_err(|e| format!("can't save cloud settings: {e}"))?;
        Ok(labels)
    }

    /// The passphrase while unlocked (a backup reseals the vault with it).
    pub(crate) fn passphrase(&self) -> Option<String> {
        self.passphrase.clone()
    }

    pub fn lock(&mut self) {
        self.vault = None;
        self.passphrase = None;
        self.drive = Default::default();
    }

    /// Fold the drive worker's findings into the session: quotas and rotated credentials.
    pub fn absorb_drive(&mut self, library: &Path) {
        let t = self.drive.transfers.clone();
        let quotas = std::mem::take(&mut *lock(&t.quotas));
        self.quotas.extend(quotas);
        let updated = std::mem::take(&mut *lock(&t.updated));
        if let Some(v) = self.vault.as_mut()
            && !updated.is_empty()
        {
            for u in updated {
                if let Some(a) = v.accounts.iter_mut().find(|a| a.id == u.id) {
                    *a = u;
                }
            }
            if let Err(e) = self.save_vault(library) {
                log::warn!("cloud: {e}");
            }
        }
    }

    pub fn change_passphrase(&mut self, library: &Path, old: &str, new: &str) -> Result<(), String> {
        if self.passphrase.as_deref() != Some(old) {
            return Err("the current passphrase is wrong".into());
        }
        if new.chars().count() < MIN_PASSPHRASE {
            return Err(format!("the passphrase needs at least {MIN_PASSPHRASE} characters"));
        }
        self.passphrase = Some(new.to_string());
        self.save_vault(library).map(|_| ())
    }

    /// Fold a finished job's findings into the session: new credentials, quotas, last backup.
    pub fn absorb(&mut self, library: &Path, now: &str) {
        let Some(job) = self.job.clone() else { return };
        if job.running() {
            return;
        }
        let updated = std::mem::take(&mut *lock(&job.updated));
        if let Some(v) = self.vault.as_mut()
            && !updated.is_empty()
        {
            for u in updated {
                if let Some(a) = v.accounts.iter_mut().find(|a| a.id == u.id) {
                    *a = u;
                }
            }
            if let Err(e) = self.save_vault(library) {
                log::warn!("cloud: {e}");
            }
        }
        let quotas = std::mem::take(&mut *lock(&job.quotas));
        self.quotas.extend(quotas);
        if job.what == "backup"
            && lock(&job.error).is_none()
            && !job.cancel.load(Ordering::Relaxed)
            && self.settings.last_backup.as_deref() != Some(now)
        {
            self.settings.last_backup = Some(now.to_string());
            self.settings.backed_up = true;
            let _ = self.save_settings(library);
        }
        // a backup merges into the drive's manifest: the drive saves it to the accounts (once per
        // backup), so it isn't left "not yet saved" with nothing to do
        if job.what == "backup" && self.drive.loaded && !job.drive_told.swap(true, Ordering::Relaxed) && self.drive.unsynced() {
            let has_accounts = self.vault.as_ref().is_some_and(|v| !v.accounts.is_empty());
            if let (true, Some(vault)) = (has_accounts, self.vault.clone()) {
                let ctx = crate::drive::WorkerContext { vault, settings: self.settings.clone(), library: library.to_path_buf() };
                if let Err(e) = self.drive.kick(ctx) {
                    log::warn!("drive: {e}");
                }
            }
        }
    }
}

/// One file the backup reads.
pub enum Source {
    /// Captured on the session thread (the catalog: a consistent pair of snapshot and log).
    Bytes(Vec<u8>),
    File(PathBuf),
}

pub struct BackupPlan {
    pub items: Vec<(Item, Source)>,
    pub vault: Vault,
    pub passphrase: String,
    pub settings: CloudSettings,
    pub library: PathBuf,
    /// The session's manifest (drive changes made while the backup runs are merged, not lost).
    pub shared: Arc<Mutex<Manifest>>,
}

fn open_fleet(vault: &Vault, settings: &CloudSettings, job: &CloudJob, manifest: Manifest) -> Fleet {
    let mut list = Vec::new();
    for secret in &vault.accounts {
        match accounts::open(secret, false) {
            Ok(p) => list.push(Account::new(secret.id.clone(), p, settings.accounts.get(&secret.id).cloned().unwrap_or_default())),
            Err(e) => {
                lock(&job.quotas).insert(secret.id.clone(), Err(e.to_string()));
            }
        }
    }
    Fleet::new(list, vault.data_key.clone(), settings.device.clone(), manifest)
}

pub(crate) fn read_local_manifest(library: &Path, vault: &Vault) -> Option<Manifest> {
    let data = std::fs::read(dir(library).join(MANIFEST_FILE)).ok()?;
    let plain = crypto::decrypt(&vault.data_key, &data, backup::MAX_MANIFEST as u64).ok()?;
    Manifest::from_snapshot(&plain, backup::MAX_MANIFEST).ok()
}

fn record_quotas(fleet: &mut Fleet, job: &CloudJob) {
    for (id, e) in fleet.refresh_quotas() {
        lock(&job.quotas).insert(id, Err(e.to_string()));
    }
    for a in &fleet.accounts {
        if let Some(q) = a.quota {
            lock(&job.quotas).insert(a.id.clone(), Ok(q));
        }
    }
}

/// Rotated refresh tokens, into the job (for the session's vault) and `vault`.
fn collect_updates(fleet: &Fleet, vault: &mut Vault, job: &CloudJob) -> bool {
    let mut changed = false;
    for a in &fleet.accounts {
        let Some(secret) = vault.accounts.iter_mut().find(|s| s.id == a.id) else { continue };
        if let Some(u) = accounts::updated_secret(secret, a.provider.as_ref()) {
            *secret = u.clone();
            lock(&job.updated).push(u);
            changed = true;
        }
    }
    changed
}

/// Read quotas only (Settings ▸ Cloud refresh).
pub fn run_refresh(vault: &Vault, settings: &CloudSettings, job: &CloudJob) {
    let mut fleet = open_fleet(vault, settings, job, Manifest::new());
    record_quotas(&mut fleet, job);
    let mut v = vault.clone();
    collect_updates(&fleet, &mut v, job);
}

/// Back up everything in `plan`. Runs on a worker thread.
pub fn run_backup(mut plan: BackupPlan, job: &CloudJob) -> Result<(), String> {
    let mut rng = OsRandom;
    // the newer of this device's manifest and the accounts' (another device, or a first run here)
    let local = read_local_manifest(&plan.library, &plan.vault);
    let mut fleet = open_fleet(&plan.vault, &plan.settings, job, Manifest::new());
    if fleet.accounts.is_empty() {
        return Err("no storage account could be opened".into());
    }
    let remote = fleet.load_manifest().ok();
    // the manifest is merged from every copy: ops from another device or the drive all count
    let mut manifest = lock(&plan.shared).clone();
    for m in [local, remote].into_iter().flatten() {
        manifest.merge(&m).map_err(|e| e.to_string())?;
    }
    fleet.manifest = manifest;
    // a library that joined another computer's drive must not back up over that computer's
    // library: its backup would mark the other library's photos as removed
    if plan.settings.joined && !plan.settings.backed_up && fleet.manifest.get("library.info").is_some_and(|e| !e.trashed) {
        return Err("these storage accounts already hold another library's backup (this library joined them for the drive): \
                    backing it up there would mark that library's photos as removed. Back this library up to other accounts."
            .into());
    }
    record_quotas(&mut fleet, job);

    // each account says which vault account it is (restore uses it)
    for a in &mut fleet.accounts {
        if let Ok(name) = ObjectName::parse(ACCOUNT_MARKER)
            && let Err(e) = a.provider.put(&name, marker(&a.id, &plan.settings).as_bytes())
        {
            lock(&job.quotas).insert(a.id.clone(), Err(e.to_string()));
        }
    }

    job.total.store(plan.items.len(), Ordering::Relaxed);
    let keep: BTreeSet<String> = plan.items.iter().map(|(i, _)| i.id.clone()).collect();
    let mut cancelled = false;
    for (item, source) in std::mem::take(&mut plan.items) {
        if job.cancel.load(Ordering::Relaxed) {
            cancelled = true;
            break;
        }
        let data = match source {
            Source::Bytes(b) => Ok(b),
            Source::File(path) => match std::fs::metadata(&path) {
                Ok(m) if m.len() > MAX_FILE => Err(format!("{} bytes is more than this version backs up ({MAX_FILE})", m.len())),
                Ok(_) => std::fs::read(&path).map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            },
        };
        match data {
            Ok(data) => match fleet.backup(&item, &data, &mut rng) {
                Ok(Outcome::Unchanged) => {
                    job.unchanged.fetch_add(1, Ordering::Relaxed);
                }
                Ok(Outcome::Stored(on)) => {
                    if on.is_empty() {
                        job.unchanged.fetch_add(1, Ordering::Relaxed);
                    } else {
                        job.stored.fetch_add(1, Ordering::Relaxed);
                        job.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                    }
                }
                Err(e) => job.fail(&item.name, e),
            },
            Err(e) => job.fail(&item.name, e),
        }
        job.done.fetch_add(1, Ordering::Relaxed);
    }
    // photos no longer in the library: their entries are trashed (blobs stay until a prune)
    if !cancelled {
        fleet.trash_except(&keep, crate::drive::is_library_entry).map_err(|e| e.to_string())?;
    }
    // a copy shared with a drive file that was deleted forever meanwhile may be gone with it:
    // dropped from the manifest, so the next backup stores the photo again
    if let Err(e) = fleet.check_reused() {
        log::warn!("cloud: checking shared copies: {e}");
    }
    // drive changes made meanwhile join in, and the session sees what the backup stored
    {
        let mut shared = lock(&plan.shared);
        shared.merge(&fleet.manifest).map_err(|e| e.to_string())?;
        fleet.manifest = shared.clone();
    }

    let errors = fleet.save_manifest(&mut rng).map_err(|e| format!("couldn't save the manifest to any account: {e}"))?;
    for (id, e) in errors {
        lock(&job.quotas).insert(id, Err(e.to_string()));
    }
    let plain = fleet.manifest.to_snapshot().map_err(|e| e.to_string())?;
    let local = crypto::encrypt(&fleet.data_key, &plain, &mut rng).map_err(|e| e.to_string())?;
    write_atomic(&dir(&plan.library).join(MANIFEST_FILE), &local).map_err(|e| format!("can't save the manifest in the library: {e}"))?;

    // the vault (with any rotated credentials) on every account, and in the library
    let changed = collect_updates(&fleet, &mut plan.vault, job);
    let sealed = plan.vault.seal(&plan.passphrase, VaultParams::default(), &mut rng).map_err(|e| e.to_string())?;
    if changed {
        write_atomic(&dir(&plan.library).join(VAULT_FILE), &sealed).map_err(|e| format!("can't save the cloud vault: {e}"))?;
    }
    let vault_name = ObjectName::parse(backup::VAULT_NAME).map_err(|e| e.to_string())?;
    for (id, e) in fleet.put_everywhere(&vault_name, &sealed) {
        lock(&job.quotas).insert(id, Err(e.to_string()));
    }
    record_quotas(&mut fleet, job);
    Ok(())
}

/// What restoring needs: one account's credentials and the passphrase.
pub struct RestorePlan {
    pub dir: PathBuf,
    pub source: AccountSecret,
    pub passphrase: String,
}

/// Where a restored original goes inside the new library `dir`: under the same relative path
/// when it was inside the old library, else mirrored under [`RESTORED_ORIGINALS`]. Never outside
/// `dir`; `None` for a path that can't be mirrored safely.
pub fn restore_path(dir: &Path, old_library: Option<&Path>, original: &str) -> Option<PathBuf> {
    let path = Path::new(original);
    if let Some(rel) = old_library.and_then(|l| path.strip_prefix(l).ok()) {
        return safe_join(dir.to_path_buf(), rel);
    }
    safe_join(dir.join(RESTORED_ORIGINALS), path)
}

/// `base` + the normal components of `rel` (drive letters become a folder; `..` refuses).
fn safe_join(mut base: PathBuf, rel: &Path) -> Option<PathBuf> {
    let mut any = false;
    for c in rel.components() {
        match c {
            Component::Normal(s) => {
                base.push(s);
                any = true;
            }
            Component::Prefix(p) => {
                let drive: String = p.as_os_str().to_string_lossy().chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                if !drive.is_empty() {
                    base.push(drive);
                }
            }
            Component::RootDir | Component::CurDir => {}
            Component::ParentDir => return None,
        }
    }
    any.then_some(base)
}

#[derive(Deserialize)]
struct Marker {
    id: String,
    /// Every account's settings as the device that wrote it had them (a joining device starts
    /// from them: a folder account without its size limit would take nothing).
    #[serde(default)]
    accounts: BTreeMap<String, AccountSettings>,
}

/// The `account.json` an account holds: which vault account it is, and every account's settings.
pub(crate) fn marker(id: &str, settings: &CloudSettings) -> String {
    json!({"format": "lightcraft-cloud-account", "id": id, "accounts": settings.accounts}).to_string()
}

/// Restore a library into `plan.dir` (empty or new). Runs on a worker thread.
pub fn run_restore(plan: RestorePlan, job: &CloudJob) -> Result<(), String> {
    if plan.dir.join("catalog.snap").exists() || plan.dir.join("catalog.log").exists() {
        return Err(format!("{} already holds a library; restore into a new folder", plan.dir.display()));
    }
    // an OpenDrive account given by its password is signed in first (the password goes no further)
    let source = accounts::sign_in(plan.source, &lightcraft_cloud::Net::default()).map_err(|e| format!("can't sign in to the account: {e}"))?;
    let plan = RestorePlan { source, ..plan };
    let mut source = accounts::open(&plan.source, false).map_err(|e| format!("can't open the account: {e}"))?;
    let listed = source.list().map_err(|e| format!("can't list the account: {e}"))?;
    let find = |name: &str| listed.iter().find(|l| l.name.as_str() == name).map(|l| l.remote.clone());
    let vault_ref = find(backup::VAULT_NAME).ok_or("this account holds no LightCraft backup (no vault.enc)")?;
    let sealed = source.get(&vault_ref, lightcraft_cloud::vault::MAX_VAULT as u64).map_err(|e| e.to_string())?;
    let vault = Vault::open(&sealed, &plan.passphrase).map_err(|e| e.to_string())?;
    // which vault account this is: its fresh credentials replace the stored ones
    let source_id =
        find(ACCOUNT_MARKER).and_then(|r| source.get(&r, 64 * 1024).ok()).and_then(|b| serde_json::from_slice::<Marker>(&b).ok()).map(|m| m.id);

    let settings = CloudSettings { device: random_id("dev-").map_err(|e| e.to_string())?, ..CloudSettings::default() };
    let mut fleet = open_fleet(&vault, &settings, job, Manifest::new());
    let source_id = source_id.unwrap_or_else(|| "restore-source".into());
    fleet.accounts.retain(|a| a.id != source_id);
    fleet.accounts.insert(0, Account::new(source_id.clone(), source, AccountSettings::default()));
    let manifest = fleet.load_manifest().map_err(|e| format!("can't read the backup's manifest: {e}"))?;

    let entries: Vec<lightcraft_cloud::Entry> = manifest.entries().filter(|e| !e.trashed && e.content.is_some()).collect();
    job.total.store(entries.len(), Ordering::Relaxed);
    let old_library = entries
        .iter()
        .find(|e| e.id == "library.info")
        .and_then(|e| fleet.restore(e).ok())
        .and_then(|b| serde_json::from_slice::<Value>(&b).ok())
        .and_then(|v| v["dir"].as_str().map(PathBuf::from));
    std::fs::create_dir_all(&plan.dir).map_err(|e| format!("can't create {}: {e}", plan.dir.display()))?;
    let smart_dir = crate::smart::dir(&plan.dir);
    let mut relink: BTreeSet<PathBuf> = BTreeSet::new();
    for e in &entries {
        if job.cancel.load(Ordering::Relaxed) {
            return Err("restore cancelled".into());
        }
        let target = if let Some(file) = e.id.strip_prefix("catalog.") {
            LIBRARY_FILES.contains(&file).then(|| plan.dir.join(file))
        } else if e.id.starts_with("smart.") {
            // the preview's file name only (a hostile `../x` keeps just `x`)
            Path::new(&e.name).file_name().map(|n| smart_dir.join(n))
        } else if e.id.starts_with("original.") {
            let t = restore_path(&plan.dir, old_library.as_deref(), &e.name);
            if let Some(parent) = t.as_ref().and_then(|t| t.parent()) {
                // the folder Find Missing searches: the top folder the originals went under
                let top =
                    if parent.starts_with(plan.dir.join(RESTORED_ORIGINALS)) { plan.dir.join(RESTORED_ORIGINALS) } else { parent.to_path_buf() };
                relink.insert(top);
            }
            t
        } else {
            None // library.info, cloud.settings: read, not written as files
        };
        let Some(target) = target else {
            job.done.fetch_add(1, Ordering::Relaxed);
            continue;
        };
        match fleet.restore(e) {
            Ok(data) => match write_atomic(&target, &data) {
                Ok(()) => {
                    job.stored.fetch_add(1, Ordering::Relaxed);
                    job.bytes.fetch_add(data.len() as u64, Ordering::Relaxed);
                }
                Err(err) => job.fail(&e.name, err),
            },
            Err(err) => job.fail(&e.name, err),
        }
        job.done.fetch_add(1, Ordering::Relaxed);
    }
    if !plan.dir.join("catalog.snap").exists() && !plan.dir.join("catalog.log").exists() {
        return Err("the backup holds no catalog".into());
    }

    // the restored library keeps backing up to the same accounts, with the credentials as they
    // are now: refresh tokens the restore rotated (OpenDrive replaces its own on every use)
    let mut vault = vault;
    if let Some(s) = vault.accounts.iter_mut().find(|a| a.id == source_id) {
        s.config = plan.source.config.clone();
    }
    collect_updates(&fleet, &mut vault, job);
    let sealed = vault.seal(&plan.passphrase, VaultParams::default(), &mut OsRandom).map_err(|e| e.to_string())?;
    write_atomic(&dir(&plan.dir).join(VAULT_FILE), &sealed).map_err(|e| e.to_string())?;
    let mut restored_settings = entries
        .iter()
        .find(|e| e.id == "cloud.settings")
        .and_then(|e| fleet.restore(e).ok())
        .and_then(|b| serde_json::from_slice::<CloudSettings>(&b).ok())
        .unwrap_or_default();
    restored_settings.device = settings.device;
    restored_settings.last_backup = None;
    let data = serde_json::to_vec_pretty(&restored_settings).map_err(|e| e.to_string())?;
    write_atomic(&dir(&plan.dir).join(SETTINGS_FILE), &data).map_err(|e| e.to_string())?;
    let plain = manifest.to_snapshot().map_err(|e| e.to_string())?;
    let local = crypto::encrypt(&vault.data_key, &plain, &mut OsRandom).map_err(|e| e.to_string())?;
    write_atomic(&dir(&plan.dir).join(MANIFEST_FILE), &local).map_err(|e| e.to_string())?;

    *lock(&job.restored) = Some((plan.dir.clone(), relink.into_iter().collect()));
    Ok(())
}

/// Start `f` on a worker thread for `job`, which is marked finished when it returns.
pub fn spawn(job: Arc<CloudJob>, name: &str, f: impl FnOnce(&CloudJob) -> Result<(), String> + Send + 'static) -> std::io::Result<()> {
    std::thread::Builder::new().name(name.into()).spawn(move || {
        // a panic in a provider must not leave the job "running" forever
        let r =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&job))).unwrap_or_else(|_| Err("the cloud job stopped unexpectedly".into()));
        if let Err(e) = r {
            log::warn!("cloud {}: {e}", job.what);
            *lock(&job.error) = Some(e);
        }
        job.finished.store(true, Ordering::Relaxed);
    })?;
    Ok(())
}

/// The availability each kind of library file asks for.
pub fn level_of(kind: &str) -> Availability {
    match kind {
        "catalog" | "smart" => Availability::Instant,
        _ => Availability::Standard,
    }
}
