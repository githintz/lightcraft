//! Cloud backup commands (`cloud.*`, opt-in; `docs/cloud-storage.md`): set up and unlock the
//! library's vault, connect storage accounts (a browser sign-in for Google Drive, OneDrive and
//! Dropbox; settings for folders, S3 and WebDAV), back up, and restore a library elsewhere.
//!
//! None of them is journaled: they carry passphrases and credentials. Backup, restore and quota
//! refreshes run on a worker thread (`cloud.progress`, `cloud.cancel`), or inline with `wait`
//! (CLI, MCP, tests).

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use lightcraft_cloud::backup::{AccountSettings, Item};
use lightcraft_cloud::crypto::OsRandom;
use lightcraft_cloud::oauth::{LoopbackFlow, OAuthClient};
use lightcraft_cloud::{AccountSecret, Availability, Net, SpeedClass, accounts};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, str_param};
use crate::cloud::{self, BackupPlan, CloudJob, RestorePlan, SignIn, Source};
use crate::{Result, Session};

/// How long a browser sign-in waits for the user.
const SIGN_IN_TIMEOUT: Duration = Duration::from_secs(600);
/// Providers that sign in through the browser.
const OAUTH: &[&str] = &["gdrive", "onedrive", "dropbox"];

fn library_dir(s: &Session, c: &str) -> Result<PathBuf> {
    match &s.library {
        Some(l) if l.on_disk => Ok(l.dir.clone()),
        _ => Err(bad(c, "cloud backup needs a library on disk")),
    }
}

fn idle(s: &Session, c: &str) -> Result<()> {
    if s.cloud.job.as_ref().is_some_and(|j| j.running()) {
        return Err(bad(c, "a cloud backup or restore is running; wait for it or cancel it"));
    }
    Ok(())
}

fn unlocked(s: &Session, c: &str) -> Result<()> {
    if !s.cloud.unlocked() {
        return Err(bad(c, "the cloud vault is locked: unlock it with its passphrase"));
    }
    Ok(())
}

fn passphrase<'a>(p: &'a Value, key: &str, c: &str) -> Result<&'a str> {
    str_param(p, key).filter(|s| !s.is_empty()).ok_or_else(|| bad(c, format!("missing `{key}`")))
}

fn speed(v: &str) -> Option<SpeedClass> {
    serde_json::from_value(json!(v)).ok()
}

/// Apply `cap`, `speed`, `priority`, `acceptsUploads` from `p` to `a`.
fn read_settings(p: &Value, a: &mut AccountSettings, c: &str) -> Result<()> {
    if let Some(v) = p.get("cap") {
        a.cap = match v {
            Value::Null => None,
            v => Some(v.as_u64().ok_or_else(|| bad(c, "`cap` is a number of bytes (or null for none)"))?),
        };
    }
    if let Some(v) = str_param(p, "speed") {
        a.speed = speed(v).ok_or_else(|| bad(c, "`speed` is archive, slow, standard or fast"))?;
    }
    if let Some(v) = p.get("priority") {
        a.priority = v.as_f64().filter(|x| (0.0..=1.0).contains(x)).ok_or_else(|| bad(c, "`priority` is 0–1"))?;
    }
    if let Some(v) = p.get("acceptsUploads") {
        a.accepts_uploads = v.as_bool().ok_or_else(|| bad(c, "`acceptsUploads` is true or false"))?;
    }
    Ok(())
}

fn new_account_id() -> std::result::Result<String, String> {
    let mut b = [0u8; 6];
    lightcraft_cloud::crypto::Random::fill(&mut OsRandom, &mut b).map_err(|e| e.to_string())?;
    Ok(format!("acc-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>()))
}

/// Add `secret` to the vault with `settings`, and save both.
fn add_secret(s: &mut Session, dir: &std::path::Path, secret: AccountSecret, settings: AccountSettings, c: &str) -> Result<Value> {
    let id = secret.id.clone();
    let Some(vault) = s.cloud.vault.as_mut() else { return Err(bad(c, "the cloud vault is locked")) };
    vault.accounts.push(secret);
    s.cloud.settings.accounts.insert(id.clone(), settings);
    s.cloud.save_vault(dir).map_err(|e| bad(c, e))?;
    s.cloud.save_settings(dir).map_err(|e| bad(c, format!("can't save cloud settings: {e}")))?;
    Ok(json!({ "id": id }))
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    let Some(dir) = s.library.as_ref().filter(|l| l.on_disk).map(|l| l.dir.clone()) else {
        return Ok(json!({"available": false, "why": "cloud backup needs a library on disk"}));
    };
    let now = (s.clock)();
    s.cloud.absorb(&dir, &now);
    let accounts: Vec<Value> = s
        .cloud
        .vault
        .as_ref()
        .map(|v| {
            v.accounts
                .iter()
                .map(|a| {
                    let settings = s.cloud.settings.accounts.get(&a.id).cloned().unwrap_or_default();
                    let (quota, error) = match s.cloud.quotas.get(&a.id) {
                        Some(Ok(q)) => (json!({"total": q.total, "used": q.used}), Value::Null),
                        Some(Err(e)) => (Value::Null, json!(e)),
                        None => (Value::Null, Value::Null),
                    };
                    json!({"id": a.id, "provider": a.provider, "label": a.label, "settings": settings, "quota": quota, "error": error})
                })
                .collect()
        })
        .unwrap_or_default();
    Ok(json!({
        "available": true,
        "configured": cloud::CloudState::configured(&dir),
        "unlocked": s.cloud.unlocked(),
        "accounts": accounts,
        "originalCopies": s.cloud.settings.original_copies,
        "smartPreviews": s.cloud.settings.smart_previews,
        "clientIds": s.cloud.settings.client_ids,
        "lastBackup": s.cloud.settings.last_backup,
        "job": s.cloud.job.as_ref().map(|j| j.json()),
        "signIn": s.cloud.sign_in.as_ref().map(|si| json!({"provider": si.provider, "url": si.url, "done": lock_result(si).is_some()})),
    }))
}

fn lock_result(si: &SignIn) -> Option<std::result::Result<String, String>> {
    si.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone()
}

fn setup(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.setup";
    let dir = library_dir(s, C)?;
    let pass = passphrase(p, "passphrase", C)?;
    s.cloud.setup(&dir, pass).map_err(|e| bad(C, e))?;
    Ok(json!({"configured": true, "unlocked": true}))
}

fn unlock(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.unlock";
    let dir = library_dir(s, C)?;
    s.cloud.unlock(&dir, passphrase(p, "passphrase", C)?).map_err(|e| bad(C, e))?;
    Ok(json!({"unlocked": true}))
}

fn change_passphrase(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.changePassphrase";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    idle(s, C)?;
    s.cloud.change_passphrase(&dir, passphrase(p, "old", C)?, passphrase(p, "new", C)?).map_err(|e| bad(C, e))?;
    Ok(json!({"changed": true, "note": "the accounts get the new vault with the next backup"}))
}

fn add_account(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.addAccount";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    idle(s, C)?;
    let provider = str_param(p, "provider").ok_or_else(|| bad(C, format!("missing `provider` ({})", accounts::KINDS.join(", "))))?;
    if !accounts::KINDS.contains(&provider) {
        return Err(bad(C, format!("unknown provider {provider:?} ({})", accounts::KINDS.join(", "))));
    }
    let config = p.get("config").filter(|v| v.is_object()).cloned().ok_or_else(|| bad(C, "missing `config` object"))?;
    let label = str_param(p, "label").filter(|l| !l.trim().is_empty()).unwrap_or(provider).trim().chars().take(100).collect::<String>();
    let mut settings = AccountSettings::default();
    read_settings(p, &mut settings, C)?;
    let id = new_account_id().map_err(|e| bad(C, e))?;
    let secret = AccountSecret { id, provider: provider.to_string(), label, config };
    // make the app folder / bucket and read the quota: an account that doesn't work isn't added
    if super::bool_or(p, "check", true) {
        let mut opened = accounts::open(&secret, true).map_err(|e| bad(C, format!("the account doesn't work: {e}")))?;
        match opened.quota() {
            Ok(q) => {
                s.cloud.quotas.insert(secret.id.clone(), Ok(q));
            }
            Err(e) => return Err(bad(C, format!("the account doesn't work: {e}"))),
        }
    } else {
        accounts::open(&secret, false).map_err(|e| bad(C, format!("the account settings are wrong: {e}")))?;
    }
    add_secret(s, &dir, secret, settings, C)
}

fn set_account(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.setAccount";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    let id = str_param(p, "id").ok_or_else(|| bad(C, "missing `id`"))?.to_string();
    let Some(vault) = s.cloud.vault.as_mut() else { return Err(bad(C, "the cloud vault is locked")) };
    let Some(secret) = vault.accounts.iter_mut().find(|a| a.id == id) else { return Err(bad(C, format!("no account {id:?}"))) };
    let mut relabelled = false;
    if let Some(l) = str_param(p, "label").map(str::trim).filter(|l| !l.is_empty()) {
        secret.label = l.chars().take(100).collect();
        relabelled = true;
    }
    let mut settings = s.cloud.settings.accounts.get(&id).cloned().unwrap_or_default();
    read_settings(p, &mut settings, C)?;
    s.cloud.settings.accounts.insert(id.clone(), settings.clone());
    if relabelled {
        s.cloud.save_vault(&dir).map_err(|e| bad(C, e))?;
    }
    s.cloud.save_settings(&dir).map_err(|e| bad(C, format!("can't save cloud settings: {e}")))?;
    Ok(json!({"id": id, "settings": settings}))
}

fn remove_account(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.removeAccount";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    idle(s, C)?;
    let id = str_param(p, "id").ok_or_else(|| bad(C, "missing `id`"))?.to_string();
    let Some(vault) = s.cloud.vault.as_mut() else { return Err(bad(C, "the cloud vault is locked")) };
    let before = vault.accounts.len();
    vault.accounts.retain(|a| a.id != id);
    if vault.accounts.len() == before {
        return Err(bad(C, format!("no account {id:?}")));
    }
    s.cloud.settings.accounts.remove(&id);
    s.cloud.quotas.remove(&id);
    s.cloud.save_vault(&dir).map_err(|e| bad(C, e))?;
    s.cloud.save_settings(&dir).map_err(|e| bad(C, format!("can't save cloud settings: {e}")))?;
    Ok(json!({"removed": id, "note": "files already stored on the account stay there"}))
}

fn configure(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.configure";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    if let Some(v) = p.get("originalCopies") {
        let n = v.as_u64().filter(|n| (1..=8).contains(n)).ok_or_else(|| bad(C, "`originalCopies` is 1–8"))?;
        s.cloud.settings.original_copies = n as u8;
    }
    if let Some(v) = p.get("smartPreviews") {
        s.cloud.settings.smart_previews = v.as_bool().ok_or_else(|| bad(C, "`smartPreviews` is true or false"))?;
    }
    if let Some(ids) = p.get("clientIds").and_then(Value::as_object) {
        for (k, v) in ids {
            if !OAUTH.contains(&k.as_str()) {
                return Err(bad(C, format!("client ids are for {}", OAUTH.join(", "))));
            }
            match v.as_str().map(str::trim) {
                Some(id) if !id.is_empty() => s.cloud.settings.client_ids.insert(k.clone(), id.chars().take(256).collect()),
                _ => s.cloud.settings.client_ids.remove(k),
            };
        }
    }
    s.cloud.save_settings(&dir).map_err(|e| bad(C, format!("can't save cloud settings: {e}")))?;
    Ok(
        json!({"originalCopies": s.cloud.settings.original_copies, "smartPreviews": s.cloud.settings.smart_previews, "clientIds": s.cloud.settings.client_ids}),
    )
}

fn oauth_client(provider: &str, client_id: String, secret: Option<String>) -> Option<OAuthClient> {
    match provider {
        "gdrive" => Some(OAuthClient::google_drive(client_id, secret)),
        "onedrive" => Some(OAuthClient::onedrive(client_id)),
        "dropbox" => Some(OAuthClient::dropbox(client_id, lightcraft_cloud::dropbox::DEFAULT_REDIRECT_PORT)),
        _ => None,
    }
}

/// The account configuration for a finished sign-in.
fn oauth_config(provider: &str, client: &OAuthClient, refresh: String) -> Option<Value> {
    match provider {
        "gdrive" => serde_json::to_value(lightcraft_cloud::gdrive::GoogleDriveConfig::new(client, refresh)).ok(),
        "onedrive" => serde_json::to_value(lightcraft_cloud::onedrive::OneDriveConfig::new(client, refresh)).ok(),
        "dropbox" => serde_json::to_value(lightcraft_cloud::dropbox::DropboxConfig::new(client, refresh)).ok(),
        _ => None,
    }
}

fn sign_in(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.signIn";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    let provider =
        str_param(p, "provider").filter(|v| OAUTH.contains(v)).ok_or_else(|| bad(C, format!("`provider` is one of {}", OAUTH.join(", "))))?;
    let client_id = str_param(p, "clientId")
        .map(str::to_string)
        .or_else(|| s.cloud.settings.client_ids.get(provider).cloned())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| bad(C, "missing `clientId`: register LightCraft with the provider (see docs/cloud-storage.md) and give its client id"))?;
    let client =
        oauth_client(provider, client_id.clone(), str_param(p, "clientSecret").map(str::to_string)).ok_or_else(|| bad(C, "unknown provider"))?;
    if let Some(old) = s.cloud.sign_in.take() {
        old.cancel.store(true, Ordering::Relaxed);
    }
    let flow = LoopbackFlow::start(client.clone(), &mut OsRandom).map_err(|e| bad(C, e.to_string()))?;
    let label = str_param(p, "label").map(str::trim).filter(|l| !l.is_empty()).unwrap_or(provider).chars().take(100).collect();
    let state = Arc::new(SignIn { provider: provider.to_string(), label, url: flow.authorize_url().to_string(), ..SignIn::default() });
    *state.client.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(client);
    let st = state.clone();
    std::thread::Builder::new()
        .name("lc-cloud-signin".into())
        .spawn(move || {
            let net = Net { cancel: st.cancel.clone(), ..Net::default() };
            let r =
                flow.wait(&net, SIGN_IN_TIMEOUT).and_then(|t| t.refresh_token.ok_or(lightcraft_cloud::CloudError::Denied("no refresh token".into())));
            *st.result.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(r.map_err(|e| e.to_string()));
        })
        .map_err(|e| bad(C, format!("could not start the sign-in: {e}")))?;
    s.cloud.settings.client_ids.insert(provider.to_string(), client_id);
    let _ = s.cloud.save_settings(&dir);
    let url = state.url.clone();
    s.cloud.sign_in = Some(state);
    Ok(json!({"url": url, "note": "open the url in a browser; cloud.signInStatus says when the account is added"}))
}

fn sign_in_status(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "cloud.signInStatus";
    let Some(si) = s.cloud.sign_in.clone() else { return Ok(Value::Null) };
    let base = json!({"provider": si.provider, "url": si.url});
    let Some(result) = lock_result(&si) else { return Ok(merge(base, json!({"state": "waiting"}))) };
    if si.taken.swap(true, Ordering::Relaxed) {
        return Ok(merge(base, json!({"state": if result.is_ok() { "added" } else { "failed" }, "error": result.err()})));
    }
    match result {
        Err(e) => Ok(merge(base, json!({"state": "failed", "error": e}))),
        Ok(refresh) => {
            let dir = library_dir(s, C)?;
            let client = si.client.lock().unwrap_or_else(std::sync::PoisonError::into_inner).clone().ok_or_else(|| bad(C, "sign-in state lost"))?;
            let config = oauth_config(&si.provider, &client, refresh).ok_or_else(|| bad(C, "unknown provider"))?;
            let id = new_account_id().map_err(|e| bad(C, e))?;
            let secret = AccountSecret { id, provider: si.provider.clone(), label: si.label.clone(), config };
            let added = add_secret(s, &dir, secret, AccountSettings::default(), C)?;
            Ok(merge(base, json!({"state": "added", "account": added["id"]})))
        }
    }
}

fn merge(mut a: Value, b: Value) -> Value {
    if let (Some(a), Value::Object(b)) = (a.as_object_mut(), b) {
        a.extend(b);
    }
    a
}

/// The backup's items, read now (the catalog) or listed for the worker (originals, smart
/// previews).
fn plan_items(s: &mut Session, dir: &std::path::Path, c: &str) -> Result<Vec<(Item, Source)>> {
    // the catalog as saved: log first, then snapshot (a compaction in between still gives a
    // pair that loads to the same catalog: the log's records carry their sequence numbers)
    s.persist()?;
    let mut items = Vec::new();
    let everywhere = |id: String, name: &str| Item { id, name: name.to_string(), level: Availability::Instant, copies: 1, everywhere: true };
    let mut files: Vec<&str> = cloud::LIBRARY_FILES.to_vec();
    files.sort_by_key(|f| *f != "catalog.log");
    for f in files {
        match std::fs::read(dir.join(f)) {
            Ok(b) => items.push((everywhere(format!("catalog.{f}"), f), Source::Bytes(b))),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(bad(c, format!("can't read {f}: {e}"))),
        }
    }
    // nothing that changes by itself (a time) goes in, so an unchanged library sends nothing
    let info = json!({"format": "lightcraft-library-info", "dir": dir.to_string_lossy(), "device": s.cloud.settings.device});
    items.push((everywhere("library.info".into(), "library"), Source::Bytes(info.to_string().into_bytes())));
    let settings = cloud::CloudSettings { last_backup: None, ..s.cloud.settings.clone() };
    let settings = serde_json::to_vec(&settings).map_err(|e| bad(c, e.to_string()))?;
    items.push((everywhere("cloud.settings".into(), "cloud settings"), Source::Bytes(settings)));

    let copies = s.cloud.settings.original_copies.clamp(1, 8);
    let mut seen = std::collections::HashSet::new();
    for photo in s.catalog.photos() {
        let Some(path) = super::missing::checked_path(photo) else { continue };
        // virtual copies share their original
        if !seen.insert(path.to_string()) {
            continue;
        }
        let item =
            Item { id: format!("original.{}", photo.id.0), name: path.to_string(), level: cloud::level_of("original"), copies, everywhere: false };
        items.push((item, Source::File(PathBuf::from(path))));
    }
    if s.cloud.settings.smart_previews
        && let Some(smart) = s.media.smart_dir.clone()
        && let Ok(rd) = std::fs::read_dir(&smart)
    {
        for e in rd.flatten().take(5_000_000) {
            let name = e.file_name().to_string_lossy().to_string();
            let id: String = name.chars().filter(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.')).take(100).collect();
            if id.is_empty() || !e.file_type().is_ok_and(|t| t.is_file()) {
                continue;
            }
            let item = Item { id: format!("smart.{id}"), name, level: cloud::level_of("smart"), copies: 1, everywhere: false };
            items.push((item, Source::File(e.path())));
        }
    }
    Ok(items)
}

fn backup(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.backup";
    let dir = library_dir(s, C)?;
    unlocked(s, C)?;
    idle(s, C)?;
    let (Some(vault), Some(pass)) = (s.cloud.vault.clone(), s.cloud.passphrase()) else { return Err(bad(C, "the cloud vault is locked")) };
    if vault.accounts.is_empty() {
        return Err(bad(C, "no storage account: add one first (cloud.addAccount or cloud.signIn)"));
    }
    let items = plan_items(s, &dir, C)?;
    s.cloud.drive.load(&dir, &vault);
    let shared = s.cloud.drive.manifest.clone();
    let plan = BackupPlan { items, vault, passphrase: pass, settings: s.cloud.settings.clone(), library: dir.clone(), shared };
    let job = Arc::new(CloudJob::new("backup"));
    s.cloud.job = Some(job.clone());
    if super::bool_or(p, "wait", false) {
        let r = cloud::run_backup(plan, &job);
        if let Err(e) = &r {
            *job.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
        }
        job.finished.store(true, Ordering::Relaxed);
        let now = (s.clock)();
        s.cloud.absorb(&dir, &now);
        r.map_err(|e| bad(C, e))?;
        return Ok(job.json());
    }
    cloud::spawn(job.clone(), "lc-cloud-backup", move |j| cloud::run_backup(plan, j)).map_err(|e| bad(C, format!("could not start: {e}")))?;
    Ok(job.json())
}

fn refresh(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.refresh";
    library_dir(s, C)?;
    unlocked(s, C)?;
    idle(s, C)?;
    let Some(vault) = s.cloud.vault.clone() else { return Err(bad(C, "the cloud vault is locked")) };
    let settings = s.cloud.settings.clone();
    let job = Arc::new(CloudJob::new("refresh"));
    s.cloud.job = Some(job.clone());
    if super::bool_or(p, "wait", false) {
        cloud::run_refresh(&vault, &settings, &job);
        job.finished.store(true, Ordering::Relaxed);
        return status(s, &Value::Null);
    }
    cloud::spawn(job.clone(), "lc-cloud-refresh", move |j| {
        cloud::run_refresh(&vault, &settings, j);
        Ok(())
    })
    .map_err(|e| bad(C, format!("could not start: {e}")))?;
    Ok(job.json())
}

fn restore(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.restore";
    idle(s, C)?;
    let dir = str_param(p, "dir").filter(|d| !d.is_empty()).map(PathBuf::from).ok_or_else(|| bad(C, "missing `dir`: the new library's folder"))?;
    let provider = str_param(p, "provider")
        .filter(|k| accounts::KINDS.contains(k))
        .ok_or_else(|| bad(C, format!("`provider` is one of {}", accounts::KINDS.join(", "))))?;
    let config =
        p.get("config").filter(|v| v.is_object()).cloned().ok_or_else(|| bad(C, "missing `config` object: the account holding the backup"))?;
    let pass = passphrase(p, "passphrase", C)?.to_string();
    let source = AccountSecret { id: "restore-source".into(), provider: provider.to_string(), label: provider.to_string(), config };
    let plan = RestorePlan { dir, source, passphrase: pass };
    let job = Arc::new(CloudJob::new("restore"));
    s.cloud.job = Some(job.clone());
    if super::bool_or(p, "wait", false) {
        let r = cloud::run_restore(plan, &job);
        if let Err(e) = &r {
            *job.error.lock().unwrap_or_else(std::sync::PoisonError::into_inner) = Some(e.clone());
        }
        job.finished.store(true, Ordering::Relaxed);
        r.map_err(|e| bad(C, e))?;
        let mut out = job.json();
        let finished = finish_restore(s, &Value::Null)?;
        if let Some(o) = out.as_object_mut() {
            o.insert("library".into(), finished);
        }
        return Ok(out);
    }
    cloud::spawn(job.clone(), "lc-cloud-restore", move |j| cloud::run_restore(plan, j)).map_err(|e| bad(C, format!("could not start: {e}")))?;
    Ok(job.json())
}

/// After a restore: open the restored library and relink its originals from where the restore
/// put them.
fn finish_restore(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "cloud.finishRestore";
    let Some(job) = s.cloud.job.clone().filter(|j| j.what == "restore") else { return Err(bad(C, "no restore to finish")) };
    if job.running() {
        return Err(bad(C, "the restore is still running"));
    }
    let Some((dir, relink)) = job.restored.lock().unwrap_or_else(std::sync::PoisonError::into_inner).take() else {
        return Err(bad(C, "the restore didn't finish (see cloud.progress for why)"));
    };
    s.open_library(&dir, false)?;
    let mut relinked = 0usize;
    for folder in relink.iter().filter(|f| f.is_dir()) {
        let r = s.execute("library.findMissing", &json!({ "folder": folder.to_string_lossy() }))?;
        relinked += r["found"].as_array().map(Vec::len).unwrap_or(0);
    }
    let missing = super::missing::missing(s).len();
    Ok(json!({"dir": dir.to_string_lossy(), "photos": s.catalog.len(), "relinked": relinked, "missing": missing}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "cloud.status", "Cloud Backup Status", [], None, "{} → {available, configured, unlocked, accounts: [{id, provider, label, settings: {cap, speed, priority, acceptsUploads}, quota: {total, used}, error}], originalCopies, smartPreviews, clientIds, lastBackup, job, signIn}", always, status),
        cmd!(query "cloud.setup", "Set Up Cloud Backup", [], None, "{passphrase} — create the library's cloud vault (a new data key, no accounts) sealed with the passphrase (8+ characters; a lost passphrase loses the backup)", always, setup),
        cmd!(query "cloud.unlock", "Unlock Cloud Backup", [], None, "{passphrase}", always, unlock),
        cmd!(query "cloud.lock", "Lock Cloud Backup", [], None, "{} — forget the vault and passphrase in memory", always, |s, _| {
            s.cloud.lock();
            Ok(json!({"unlocked": false}))
        }),
        cmd!(query "cloud.changePassphrase", "Change Cloud Passphrase", [], None, "{old, new}", always, change_passphrase),
        cmd!(query "cloud.addAccount", "Add Storage Account", [], None, "{provider: folder|s3|webdav|gdrive|onedrive|dropbox, config, label?, cap?, speed?, priority?, acceptsUploads?, check?: true} — folder {path}; s3 {endpoint, region, bucket, prefix, access_key, secret_key, path_style}; webdav {url, user, password}; OAuth providers {client_id, refresh_token, …} (or cloud.signIn). With check (default) the app folder / bucket is made and the quota read (network) → {id}", always, add_account),
        cmd!(query "cloud.signIn", "Sign In to a Storage Account", [], None, "{provider: gdrive|onedrive|dropbox, clientId?, clientSecret?, label?} — start a browser sign-in (PKCE, loopback redirect) → {url}; the account is added when cloud.signInStatus sees it finished", always, sign_in),
        cmd!(query "cloud.signInStatus", "Storage Sign-In Status", [], None, "{} → {state: waiting|added|failed, provider, url, account?, error?} | null", always, sign_in_status),
        cmd!(query "cloud.setAccount", "Storage Account Settings", [], None, "{id, label?, cap?: bytes|null, speed?: archive|slow|standard|fast, priority?: 0–1, acceptsUploads?}", always, set_account),
        cmd!(query "cloud.removeAccount", "Remove Storage Account", [], None, "{id} — forget the account (files on it stay)", always, remove_account),
        cmd!(query "cloud.configure", "Cloud Backup Settings", [], None, "{originalCopies?: 1–8, smartPreviews?: bool, clientIds?: {gdrive?, onedrive?, dropbox?}}", always, configure),
        cmd!(query "cloud.backup", "Back Up Now", [], None, "{wait?: bool} — back up the catalog (to every account), originals (placed by free space, speed and priority; originalCopies copies on different providers) and smart previews, encrypted; unchanged files are skipped → progress {what, total, done, stored, unchanged, failed, bytes, running, failures} (cloud.progress, cloud.cancel)", always, backup),
        cmd!(query "cloud.refresh", "Refresh Storage Accounts", [], None, "{wait?: bool} — read every account's quota", always, refresh),
        cmd!(query "cloud.progress", "Cloud Backup Progress", [], None, "{} → the running or last job | null", always, |s, _| {
            if let Some(dir) = s.library.as_ref().filter(|l| l.on_disk).map(|l| l.dir.clone()) {
                let now = (s.clock)();
                s.cloud.absorb(&dir, &now);
            }
            Ok(s.cloud.job.as_ref().map(|j| j.json()).unwrap_or(Value::Null))
        }),
        cmd!(query "cloud.cancel", "Cancel Cloud Backup", [], None, "{}", always, |s, _| {
            if let Some(j) = &s.cloud.job {
                j.cancel.store(true, Ordering::Relaxed);
            }
            if let Some(si) = &s.cloud.sign_in {
                si.cancel.store(true, Ordering::Relaxed);
            }
            Ok(Value::Null)
        }),
        cmd!(query "cloud.restore", "Restore Library from Cloud", [], None, "{dir, provider, config, passphrase, wait?} — restore the library backed up to the account into the new folder dir (never writes outside it; originals that lived outside the old library go under dir/Restored Originals) → progress; when done, cloud.finishRestore opens it and relinks the originals (with wait, done at once → {…, library: {dir, photos, relinked, missing}})", always, restore),
        cmd!(query "cloud.finishRestore", "Open Restored Library", [], None, "{} → {dir, photos, relinked, missing}", always, finish_restore),
    ]
}
