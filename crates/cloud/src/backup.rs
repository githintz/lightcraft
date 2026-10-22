//! Backup and restore over a set of storage accounts (a [`Fleet`]): which account(s) store each
//! file ([`crate::placement`]), encrypted blobs named by their plaintext's content id
//! ([`crate::crypto`]), and the manifest that says where everything is ([`crate::manifest`]),
//! stored on every account. No network code of its own: it drives [`Provider`]s, so the same
//! logic runs against folders and in-memory accounts in tests.
//!
//! On every account:
//!
//! ```text
//! vault.enc                              the vault (data key + accounts), written by the caller
//! manifest/snapshot-<clock>.json.enc     the manifest, the newest two kept
//! blobs/<aa>/<id>.bin                    files, encrypted under the vault's data key
//! ```

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::crypto::{self, Key, Random};
use crate::placement::{self, PlacementAccount, PlacementRequest, Weights};
use crate::{Availability, CloudError, ContentId, Entry, Manifest, ObjectName, OpKind, Provider, Quota, RemoteRef, Result, SpeedClass};

/// The object holding the vault on every account.
pub const VAULT_NAME: &str = "vault.enc";
/// Largest manifest snapshot read back.
pub const MAX_MANIFEST: usize = 512 * 1024 * 1024;
/// Snapshots kept on each account (older ones are removed after a new one is written).
pub const KEEP_SNAPSHOTS: usize = 2;

/// What the user set for an account (stored next to the vault, not secret).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct AccountSettings {
    /// Most bytes this library may store there (`None`: 90 % of the account's quota).
    pub cap: Option<u64>,
    pub speed: SpeedClass,
    /// 0–1, the user's preference among accounts.
    pub priority: f64,
    /// Off: the account is kept (files on it are still read) but gets no new files.
    pub accepts_uploads: bool,
}

impl Default for AccountSettings {
    fn default() -> Self {
        AccountSettings { cap: None, speed: SpeedClass::Standard, priority: 0.5, accepts_uploads: true }
    }
}

/// One account of a [`Fleet`].
pub struct Account {
    /// The vault's id for it (also the manifest's replica key).
    pub id: String,
    pub provider: Box<dyn Provider>,
    pub settings: AccountSettings,
    /// The last quota read ([`Fleet::refresh_quotas`]); `None` until read.
    pub quota: Option<Quota>,
    /// Share of recent requests that worked, 0–1 (placement prefers reliable accounts).
    pub reliability: f64,
}

impl Account {
    pub fn new(id: impl Into<String>, provider: Box<dyn Provider>, settings: AccountSettings) -> Account {
        Account { id: id.into(), provider, settings, quota: None, reliability: 1.0 }
    }

    fn note(&mut self, ok: bool) {
        // an exponential average: a few failures in a row make the account a last resort
        self.reliability = (self.reliability * 0.8 + if ok { 0.2 } else { 0.0 }).clamp(0.0, 1.0);
    }
}

/// One file to back up.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    /// Manifest entry id: stable for the file (`original.42`, `catalog.catalog.snap`).
    pub id: String,
    /// What it is to the library (an original's path, a catalog file's name).
    pub name: String,
    pub level: Availability,
    /// Copies on different accounts, different providers first (ignored when `everywhere`).
    pub copies: u8,
    /// On every account that accepts uploads (the catalog: losing an account never loses it).
    pub everywhere: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Already stored as often as asked, with this content.
    Unchanged,
    /// Stored on these accounts now.
    Stored(Vec<String>),
}

/// The accounts of one library, its data key and its manifest.
pub struct Fleet {
    pub accounts: Vec<Account>,
    pub data_key: Key,
    /// This device's id in manifest stamps.
    pub device: String,
    pub manifest: Manifest,
    pub weights: Weights,
    /// The snapshots [`Fleet::load_manifest`] merged (other devices' ones may be removed once a
    /// snapshot holding them is saved).
    merged: std::collections::BTreeSet<String>,
    /// `(entry, account, key)` of the copies [`Fleet::backup`] shared with another entry instead
    /// of uploading ([`Fleet::check_reused`] makes sure they are still there).
    reused: Vec<(String, String, String)>,
}

/// Most snapshots [`Fleet::load_manifest`] reads and merges (the newest).
pub const MERGE_SNAPSHOTS: usize = 8;

/// A snapshot's name: the manifest's clock and the device that wrote it, so two devices saving
/// at the same clock don't overwrite each other.
fn snapshot_name(clock: u64, device: &str) -> String {
    let device: String = device.chars().filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == '-').take(40).collect();
    if device.is_empty() { format!("manifest/snapshot-{clock}.json.enc") } else { format!("manifest/snapshot-{clock}-{device}.json.enc") }
}

/// `(clock, device)` of a snapshot name (older ones have no device).
fn snapshot_parts(name: &str) -> Option<(u64, Option<&str>)> {
    let rest = name.strip_prefix("manifest/snapshot-")?.strip_suffix(".json.enc")?;
    match rest.split_once('-') {
        Some((clock, device)) => Some((clock.parse().ok()?, Some(device).filter(|d| !d.is_empty()))),
        None => Some((rest.parse().ok()?, None)),
    }
}

fn snapshot_clock(name: &str) -> Option<u64> {
    snapshot_parts(name).map(|(c, _)| c)
}

/// Largest ciphertext of a `plain`-byte blob (whatever segment size it was written with).
fn max_cipher(plain: u64) -> u64 {
    crypto::encrypted_len(plain, crypto::MIN_SEGMENT).unwrap_or(u64::MAX)
}

impl Fleet {
    pub fn new(accounts: Vec<Account>, data_key: Key, device: impl Into<String>, manifest: Manifest) -> Fleet {
        Fleet { accounts, data_key, device: device.into(), manifest, weights: Weights::default(), merged: Default::default(), reused: Vec::new() }
    }

    fn account_mut(&mut self, id: &str) -> Option<&mut Account> {
        self.accounts.iter_mut().find(|a| a.id == id)
    }

    /// Read every account's quota; the accounts that couldn't be reached, with why.
    pub fn refresh_quotas(&mut self) -> Vec<(String, CloudError)> {
        let mut errors = Vec::new();
        for a in &mut self.accounts {
            match a.provider.quota() {
                Ok(q) => {
                    a.quota = Some(q);
                    a.note(true);
                }
                Err(e) => {
                    a.note(false);
                    errors.push((a.id.clone(), e));
                }
            }
        }
        errors
    }

    /// Bytes of this library's files the manifest has on `account` (plaintext sizes).
    pub fn stored_on(&self, account: &str) -> u64 {
        self.manifest.entries().filter(|e| !e.trashed && e.replicas.contains_key(account)).fold(0u64, |n, e| n.saturating_add(e.size))
    }

    fn placement_accounts(&self, exclude: &[String]) -> Vec<PlacementAccount> {
        self.accounts
            .iter()
            .filter(|a| !exclude.contains(&a.id))
            .map(|a| {
                let (total, used) = a.quota.map(|q| (q.total, q.used)).unwrap_or((None, 0));
                let stored = self.stored_on(&a.id);
                let free = placement::usable_free(total, used, a.settings.cap, stored);
                let capacity = a.settings.cap.or_else(|| total.map(|t| (t as f64 * placement::DEFAULT_CAP_SHARE) as u64)).unwrap_or(0);
                let info = a.provider.info();
                PlacementAccount {
                    id: a.id.clone(),
                    provider: info.provider,
                    free_bytes: free,
                    capacity_bytes: capacity,
                    speed: a.settings.speed,
                    reliability: a.reliability,
                    priority: a.settings.priority,
                    accepts_uploads: a.settings.accepts_uploads,
                    max_file_size: info.max_file_size,
                }
            })
            .collect()
    }

    fn record(&mut self, kind: OpKind) -> Result<()> {
        let device = self.device.clone();
        self.manifest.record(&device, kind).map(|_| ())
    }

    /// Store `data` as `item`: skipped when the manifest already has it with this content on as
    /// many accounts as asked; else encrypted and put on the accounts placement picks (or every
    /// account), and the manifest updated. Fails only when no account took it.
    pub fn backup(&mut self, item: &Item, data: &[u8], rng: &mut dyn Random) -> Result<Outcome> {
        let content = ContentId::of(data);
        let size = data.len() as u64;
        let existing = self.manifest.get(&item.id);
        let same = existing.as_ref().is_some_and(|e| e.content.as_ref() == Some(&content) && !e.trashed);
        // replicas of this content on accounts the fleet still has
        let holding: Vec<String> = match (&existing, same) {
            (Some(e), true) => e.replicas.keys().filter(|k| self.accounts.iter().any(|a| &a.id == *k)).cloned().collect(),
            _ => Vec::new(),
        };
        let open: Vec<String> = self.accounts.iter().filter(|a| a.settings.accepts_uploads).map(|a| a.id.clone()).collect();
        let wanted = if item.everywhere { open.len().max(1) } else { usize::from(item.copies.clamp(1, placement::MAX_COPIES)) };
        if same && (holding.len() >= wanted || (item.everywhere && open.iter().all(|a| holding.contains(a)))) {
            return Ok(Outcome::Unchanged);
        }
        // accounts that already hold these bytes for another entry: the copy there is shared (the
        // blob is named by its content) instead of sent again
        let mut reuse: Vec<(String, String)> = Vec::new();
        let shared_on = self.held_elsewhere(&item.id, &content);
        for (account, key) in shared_on {
            let want_more = if item.everywhere { open.contains(&account) } else { holding.len() + reuse.len() < wanted };
            if want_more && !holding.contains(&account) {
                reuse.push((account, key));
            }
        }
        let reused: Vec<String> = reuse.iter().map(|(a, _)| a.clone()).collect();
        let holding: Vec<String> = holding.into_iter().chain(reused.iter().cloned()).collect();

        let targets: Vec<String> = if item.everywhere {
            open.into_iter().filter(|a| !holding.contains(a)).collect()
        } else if holding.len() >= wanted {
            Vec::new()
        } else {
            let missing = wanted.saturating_sub(holding.len());
            let candidates = self.placement_accounts(&holding);
            let req = PlacementRequest {
                size: crypto::encrypted_len(size, crypto::DEFAULT_SEGMENT).unwrap_or(size),
                level: item.level,
                copies: missing.min(255) as u8,
            };
            match placement::choose(&candidates, &req, &self.weights) {
                Ok(chosen) => chosen.into_iter().map(|a| a.id.clone()).collect(),
                // fewer providers than copies asked for: one on each provider first, then on
                // other accounts (two disks are two places even if both are folders)
                Err(placement::PlacementError::NotEnoughProviders) if missing > 1 => {
                    let req = PlacementRequest { copies: 1, ..req };
                    let mut picked: Vec<String> = Vec::new();
                    let mut cands = candidates.clone();
                    while picked.len() < missing {
                        let Ok(c) = placement::choose(&cands, &req, &self.weights) else { break };
                        let Some(first) = c.first() else { break };
                        let (id, provider) = (first.id.clone(), first.provider.clone());
                        cands.retain(|a| a.provider != provider);
                        picked.push(id);
                    }
                    let mut rest: Vec<PlacementAccount> = candidates.into_iter().filter(|a| !picked.contains(&a.id)).collect();
                    while picked.len() < missing {
                        let Ok(c) = placement::choose(&rest, &req, &self.weights) else { break };
                        let Some(first) = c.first() else { break };
                        let id = first.id.clone();
                        rest.retain(|a| a.id != id);
                        picked.push(id);
                    }
                    if picked.is_empty() {
                        return Err(CloudError::QuotaExceeded);
                    }
                    picked
                }
                Err(e) => return Err(placement_error(e)),
            }
        };

        let name = ObjectName::blob(&content);
        if !same {
            match &existing {
                None => self.record(OpKind::Create { entry: Entry::file(&item.id, &item.name, None, content.clone(), size) })?,
                Some(e) => {
                    self.record(OpKind::SetContent { id: item.id.clone(), content: content.clone(), size })?;
                    if e.trashed {
                        self.record(OpKind::SetTrashed { id: item.id.clone(), trashed: false })?;
                    }
                    if e.name != item.name {
                        self.record(OpKind::Rename { id: item.id.clone(), name: item.name.clone() })?;
                    }
                    // the old content's copies no longer count
                    for account in e.replicas.keys() {
                        self.record(OpKind::RemoveReplica { id: item.id.clone(), account: account.clone() })?;
                    }
                }
            }
            self.record(OpKind::SetAvailability { id: item.id.clone(), availability: item.level })?;
        }
        for (account, key) in &reuse {
            self.record(OpKind::AddReplica { id: item.id.clone(), account: account.clone(), key: key.clone() })?;
            self.reused.push((item.id.clone(), account.clone(), key.clone()));
        }

        let mut stored = reused;
        let mut first_error = None;
        let cipher = if targets.is_empty() { Vec::new() } else { crypto::encrypt(&self.data_key, data, rng)? };
        for id in targets {
            let Some(account) = self.account_mut(&id) else { continue };
            match account.provider.put(&name, &cipher) {
                Ok(remote) => {
                    account.note(true);
                    if let Some(q) = account.quota.as_mut() {
                        q.used = q.used.saturating_add(cipher.len() as u64);
                    }
                    self.record(OpKind::AddReplica { id: item.id.clone(), account: id.clone(), key: remote.key })?;
                    stored.push(id);
                }
                Err(e) => {
                    account.note(false);
                    first_error.get_or_insert(e);
                }
            }
        }
        match (stored.is_empty() && holding.is_empty(), first_error) {
            (true, Some(e)) => Err(e),
            _ => Ok(Outcome::Stored(stored)),
        }
    }

    /// `(account, key)` of the copies of `content` other entries hold on accounts of this fleet
    /// (one per account).
    fn held_elsewhere(&self, id: &str, content: &ContentId) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = Vec::new();
        for e in self.manifest.entries().filter(|e| e.id != id && e.content.as_ref() == Some(content)) {
            for (account, key) in e.replicas {
                if self.accounts.iter().any(|a| a.id == account && a.settings.accepts_uploads) && !out.iter().any(|(a, _)| *a == account) {
                    out.push((account, key));
                }
            }
        }
        out
    }

    /// Drop the shared copies [`Fleet::backup`] counted on that are no longer on their account
    /// (another worker deleted the item that held them meanwhile), so the next backup stores the
    /// file again; how many. An account that can't be listed is left as it is.
    pub fn check_reused(&mut self) -> Result<usize> {
        let reused = std::mem::take(&mut self.reused);
        let mut listed: BTreeMap<String, Option<std::collections::BTreeSet<String>>> = BTreeMap::new();
        let mut dropped = 0;
        for (id, account, key) in reused {
            if !listed.contains_key(&account) {
                let keys = self.account_mut(&account).and_then(|a| a.provider.list().ok()).map(|l| l.into_iter().map(|l| l.remote.key).collect());
                listed.insert(account.clone(), keys);
            }
            let there = listed.get(&account).and_then(Option::as_ref).is_none_or(|keys| keys.contains(&key));
            let still_used = self.manifest.get(&id).is_some_and(|e| e.replicas.get(&account) == Some(&key));
            if !there && still_used {
                self.record(OpKind::RemoveReplica { id, account })?;
                dropped += 1;
            }
        }
        Ok(dropped)
    }

    /// Mark the entries `scope` accepts that are not in `keep` as trashed (their files are gone
    /// from the library); entries outside `scope` (files the user stored directly) are left
    /// alone. Their blobs stay until [`Fleet::prune`].
    pub fn trash_except(&mut self, keep: &std::collections::BTreeSet<String>, scope: impl Fn(&str) -> bool) -> Result<usize> {
        let gone: Vec<String> = self.manifest.entries().filter(|e| !e.trashed && scope(&e.id) && !keep.contains(&e.id)).map(|e| e.id).collect();
        for id in &gone {
            self.record(OpKind::SetTrashed { id: id.clone(), trashed: true })?;
        }
        Ok(gone.len())
    }

    /// Store `data` as the file `entry` (its id, name, folder and availability; content and size
    /// come from `data`), placed like [`Fleet::backup`] with `copies` copies. A new entry is
    /// recorded only when an account took the file.
    pub fn store_file(&mut self, entry: &Entry, copies: u8, data: &[u8], rng: &mut dyn Random) -> Result<Outcome> {
        let existed = self.manifest.get(&entry.id).is_some();
        if !existed {
            let mut e = Entry::file(&entry.id, &entry.name, entry.parent.as_deref(), ContentId::of(data), data.len() as u64);
            e.availability = entry.availability;
            self.record(OpKind::Create { entry: e })?;
        }
        let item = Item { id: entry.id.clone(), name: entry.name.clone(), level: entry.availability, copies, everywhere: false };
        let r = self.backup(&item, data, rng);
        if r.is_err() && !existed {
            self.record(OpKind::Delete { id: entry.id.clone() })?;
        }
        r
    }

    /// Delete the stored copies `gone` (account id, provider key, content) of entries that were
    /// deleted, except where an entry (even a trashed one) still holds the same content on that
    /// account: identical files share one blob. The copies that couldn't be deleted, with why.
    pub fn delete_unused(&mut self, gone: &[(String, String, ContentId)]) -> Vec<(String, CloudError)> {
        let used: std::collections::BTreeSet<(String, ContentId)> =
            self.manifest.entries().filter_map(|e| e.content.map(|c| e.replicas.into_keys().map(move |a| (a, c.clone())))).flatten().collect();
        let mut errors = Vec::new();
        for (account, key, content) in gone {
            if used.contains(&(account.clone(), content.clone())) {
                continue;
            }
            let Some(a) = self.account_mut(account) else { continue };
            if let Err(e) = a.provider.delete(&RemoteRef { key: key.clone() }) {
                errors.push((account.clone(), e));
            }
        }
        errors
    }

    /// Write the manifest to every account (encrypted) and remove older snapshots; the accounts
    /// that failed, with why. Fails when no account took it.
    pub fn save_manifest(&mut self, rng: &mut dyn Random) -> Result<Vec<(String, CloudError)>> {
        let plain = self.manifest.to_snapshot()?;
        let cipher = crypto::encrypt(&self.data_key, &plain, rng)?;
        let clock = self.manifest.clock();
        let name = ObjectName::parse(&snapshot_name(clock, &self.device))?;
        let mut errors = Vec::new();
        let mut saved = 0usize;
        for a in &mut self.accounts {
            match a.provider.put(&name, &cipher) {
                Ok(_) => {
                    saved += 1;
                    // clean up (a failure only costs space): this device's newest snapshots stay;
                    // another device's go only once this one holds them (it merged them), so a
                    // device saving never loses another's changes
                    if let Ok(listed) = a.provider.list() {
                        let mut own: Vec<(u64, RemoteRef)> = Vec::new();
                        for l in listed {
                            let Some((c, device)) = snapshot_parts(l.name.as_str()) else { continue };
                            if l.name.as_str() == name.as_str() {
                                continue;
                            }
                            if device == Some(self.device.as_str()) {
                                own.push((c, l.remote));
                            } else if self.merged.contains(l.name.as_str()) && c <= clock {
                                let _ = a.provider.delete(&l.remote);
                            }
                        }
                        own.sort_by_key(|s| std::cmp::Reverse(s.0));
                        for (_, r) in own.into_iter().skip(KEEP_SNAPSHOTS.saturating_sub(1)) {
                            let _ = a.provider.delete(&r);
                        }
                    }
                }
                Err(e) => errors.push((a.id.clone(), e)),
            }
        }
        if saved == 0 {
            return Err(errors.first().map(|(_, e)| e.clone()).unwrap_or_else(|| CloudError::Unsupported("no storage accounts".into())));
        }
        Ok(errors)
    }

    /// Put the same object on every account (the sealed vault); the accounts that failed.
    pub fn put_everywhere(&mut self, name: &ObjectName, data: &[u8]) -> Vec<(String, CloudError)> {
        let mut errors = Vec::new();
        for a in &mut self.accounts {
            if let Err(e) = a.provider.put(name, data) {
                errors.push((a.id.clone(), e));
            }
        }
        errors
    }

    /// The newest manifest any account holds that opens with the data key.
    pub fn load_manifest(&mut self) -> Result<Manifest> {
        // every account's snapshots, by name (the same snapshot is on every account)
        let mut snaps: BTreeMap<String, (u64, Vec<(usize, RemoteRef)>)> = BTreeMap::new();
        let mut last_error = None;
        for (i, a) in self.accounts.iter_mut().enumerate() {
            match a.provider.list() {
                Ok(listed) => {
                    for l in listed {
                        if let Some(c) = snapshot_clock(l.name.as_str()) {
                            snaps.entry(l.name.as_str().to_string()).or_insert_with(|| (c, Vec::new())).1.push((i, l.remote));
                        }
                    }
                }
                Err(e) => last_error = Some(e),
            }
        }
        // the newest ones (several devices may each have saved one): all merged
        let mut order: Vec<(String, (u64, Vec<(usize, RemoteRef)>))> = snaps.into_iter().collect();
        order.sort_by_key(|(_, (c, _))| std::cmp::Reverse(*c));
        let mut merged: Option<Manifest> = None;
        for (name, (_, copies)) in order.into_iter().take(MERGE_SNAPSHOTS) {
            for (i, remote) in copies {
                let Some(a) = self.accounts.get_mut(i) else { continue };
                let read = a
                    .provider
                    .get(&remote, max_cipher(MAX_MANIFEST as u64))
                    .and_then(|c| crypto::decrypt(&self.data_key, &c, MAX_MANIFEST as u64))
                    .and_then(|p| Manifest::from_snapshot(&p, MAX_MANIFEST));
                match read {
                    Ok(m) => {
                        match merged.as_mut() {
                            Some(all) => all.merge(&m)?,
                            None => merged = Some(m),
                        }
                        self.merged.insert(name);
                        break;
                    }
                    Err(e) => last_error = Some(e),
                }
            }
        }
        merged.ok_or_else(|| last_error.unwrap_or(CloudError::NotFound))
    }

    /// An entry's content, read from any account holding it, decrypted and checked against its
    /// content id (a damaged or swapped copy is skipped for the next one).
    pub fn restore(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let content = entry.content.clone().ok_or_else(|| CloudError::Corrupt(format!("{} has no content", entry.id)))?;
        let mut last_error = CloudError::NotFound;
        for (account, key) in &entry.replicas {
            let Some(a) = self.accounts.iter_mut().find(|a| &a.id == account) else { continue };
            let read =
                a.provider.get(&RemoteRef { key: key.clone() }, max_cipher(entry.size)).and_then(|c| crypto::decrypt(&self.data_key, &c, entry.size));
            match read {
                Ok(plain) if ContentId::of(&plain) == content => {
                    a.note(true);
                    return Ok(plain);
                }
                Ok(plain) => {
                    a.note(false);
                    last_error = CloudError::Corrupt(format!("{} came back as other content ({} bytes)", entry.id, plain.len()));
                }
                Err(e) => {
                    a.note(false);
                    last_error = e;
                }
            }
        }
        Err(last_error)
    }

    /// Delete blobs no live manifest entry refers to, on every account; how many were removed.
    pub fn prune(&mut self) -> Result<usize> {
        let live: BTreeMap<String, ()> =
            self.manifest.entries().filter(|e| !e.trashed).filter_map(|e| e.content).map(|c| (c.as_str().to_string(), ())).collect();
        let mut removed = 0usize;
        for a in &mut self.accounts {
            for l in a.provider.list()? {
                let Some(id) = l.name.blob_id() else { continue };
                if !live.contains_key(id.as_str()) {
                    a.provider.delete(&l.remote)?;
                    removed += 1;
                }
            }
        }
        // trashed entries are forgotten with their blobs
        let trashed: Vec<String> = self.manifest.entries().filter(|e| e.trashed).map(|e| e.id).collect();
        for id in trashed {
            self.record(OpKind::Delete { id })?;
        }
        Ok(removed)
    }
}

fn placement_error(e: placement::PlacementError) -> CloudError {
    use placement::PlacementError as P;
    match e {
        P::NoAccounts => CloudError::Unsupported("no storage account accepts new files".into()),
        P::NoSpace => CloudError::QuotaExceeded,
        P::TooLarge => CloudError::TooLarge { size: 0, max: 0 },
        other => CloudError::Unsupported(other.to_string()),
    }
}
