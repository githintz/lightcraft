//! Albums from photos already in a storage account (`docs/cloud-albums.md`): scan the user's own
//! folders without changing them (`cloud.scanFolders`, on worker threads, resumable), then plan
//! albums by trip with duplicates found (`cloud.planAlbums`), for the user to review.
//!
//! What they write lives in `<library>/cloud/organize/`: `scan.jsonl` (what the scan learned, one
//! file per line), `scan.json` (what was scanned and whether it finished), `edits.json` (the
//! user's merges, renames and "keep both"), `plan.json` and `plan.html` (the plan and its report).
//! None of these commands is journaled.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use lightcraft_cloud::opendrive::{OpenDriveConfig, OpenDriveProvider};
use lightcraft_cloud::{Browse, FolderBrowser, Net};
use lightcraft_organize::plan::{Edits, plan};
use lightcraft_organize::scan::{ScanOptions, Stop, load_cache, scan};
use serde_json::{Value, json};

use super::cloud::{library_dir, unlocked};
use super::{CommandSpec, always, bad, cmd, str_param};
use crate::cloud::ScanJob;
use crate::{Result, Session};

/// Workers reading files at once, by default and at most.
const WORKERS: usize = 6;
const MAX_WORKERS: usize = 16;
const MAX_ROOTS: usize = 32;

fn dir(library: &Path) -> PathBuf {
    library.join("cloud").join("organize")
}

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn progress_json(job: &ScanJob) -> Value {
    let p = &job.progress;
    let n = |a: &std::sync::atomic::AtomicU64| a.load(Ordering::Relaxed);
    let (stop, error) = match &*lock(&job.result) {
        None => (Value::Null, Value::Null),
        Some(Ok((_, Stop::Done))) => (json!("done"), Value::Null),
        Some(Ok((_, Stop::Cancelled))) => (json!("cancelled"), Value::Null),
        Some(Ok((_, Stop::Halted(why)))) => (json!("halted"), json!(why)),
        Some(Err(e)) => (json!("failed"), json!(e)),
    };
    json!({
        "running": job.running(),
        "roots": job.roots,
        "account": job.account,
        "listed": p.listed.load(Ordering::Relaxed),
        "folders": n(&p.folders),
        "found": n(&p.found),
        "toRead": n(&p.to_read),
        "read": n(&p.read),
        "failed": n(&p.failed),
        "bytesRead": n(&p.bytes),
        "stop": stop,
        "error": error,
    })
}

/// Save a token the scan rotated into the vault (once the scan is over).
fn absorb(s: &mut Session, dir: &Path) {
    let Some(job) = s.cloud.scan.clone().filter(|j| !j.running()) else { return };
    let (Some(id), Some(token)) = (job.account.clone(), lock(&job.token).take()) else { return };
    let Some(vault) = s.cloud.vault.as_mut() else { return };
    let Some(secret) = vault.accounts.iter_mut().find(|a| a.id == id) else { return };
    if secret.config.get("refresh_token").and_then(Value::as_str) != Some(token.as_str())
        && let Some(o) = secret.config.as_object_mut()
    {
        o.insert("refresh_token".into(), json!(token));
        if let Err(e) = s.cloud.save_vault(dir) {
            log::warn!("cloud: {e}");
        }
    }
}

/// The browsers a scan reads with: one per worker, sharing the account's sign-in.
fn browsers(
    s: &Session,
    p: &Value,
    job_cancel: &Arc<std::sync::atomic::AtomicBool>,
    c: &str,
) -> Result<(Vec<Box<dyn Browse>>, Option<String>, Option<OpenDriveCheck>)> {
    let workers = p.get("workers").and_then(Value::as_u64).map_or(WORKERS, |w| (w as usize).clamp(1, MAX_WORKERS));
    if let Some(folder) = str_param(p, "folder") {
        let list =
            (0..workers).map(|_| FolderBrowser::new(folder).map(|b| Box::new(b) as Box<dyn Browse>)).collect::<lightcraft_cloud::Result<Vec<_>>>();
        return Ok((list.map_err(|e| bad(c, e.to_string()))?, None, None));
    }
    let id = str_param(p, "account").ok_or_else(|| bad(c, "give `account` (an OpenDrive account's id) or `folder` (a folder on this computer)"))?;
    unlocked(s, c)?;
    let secret = s.cloud.vault.as_ref().and_then(|v| v.accounts.iter().find(|a| a.id == id)).ok_or_else(|| bad(c, format!("no account {id:?}")))?;
    if secret.provider != "opendrive" {
        return Err(bad(c, format!("scanning folders works with OpenDrive accounts and folders on this computer, not {} yet", secret.provider)));
    }
    let config: OpenDriveConfig = serde_json::from_value(secret.config.clone()).map_err(|e| bad(c, e.to_string()))?;
    let mut main = OpenDriveProvider::new(config, secret.label.clone()).map_err(|e| bad(c, e.to_string()))?.into_browser();
    main.set_net(Net { cancel: job_cancel.clone(), ..Net::default() });
    let list: Vec<Box<dyn Browse>> = (0..workers).map(|_| Box::new(main.worker()) as Box<dyn Browse>).collect();
    Ok((list, Some(id.to_string()), Some(OpenDriveCheck(main))))
}

/// Keeps the account's sign-in after the scan, to save its rotated token.
struct OpenDriveCheck(lightcraft_cloud::opendrive::OpenDriveBrowser);

fn run(job: &ScanJob, browsers: Vec<Box<dyn Browse>>, keep: Option<OpenDriveCheck>, library: &Path) {
    let organize = dir(library);
    let opts = ScanOptions { roots: job.roots.clone(), cache: organize.join("scan.jsonl") };
    let r = scan(browsers, &opts, &job.progress);
    if let Some(k) = keep {
        *lock(&job.token) = Some(k.0.refresh_token());
    }
    if let Ok(s) = &r {
        let meta = json!({"roots": job.roots, "account": job.account, "complete": s.stop == Stop::Done, "files": s.records.len()});
        let _ = std::fs::write(organize.join("scan.json"), meta.to_string());
    }
    *lock(&job.result) = Some(r.map(|s| (s.records.len(), s.stop)));
}

fn scan_folders(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.scanFolders";
    let library = library_dir(s, C)?;
    if s.cloud.scan.as_ref().is_some_and(|j| j.running()) {
        return Err(bad(C, "a scan is running; wait for it or cancel it (cloud.scanCancel)"));
    }
    absorb(s, &library);
    let roots: Vec<String> = p
        .get("roots")
        .and_then(Value::as_array)
        .map(|a| a.iter().filter_map(Value::as_str).map(|r| r.trim().trim_matches('/').to_string()).filter(|r| !r.is_empty()).collect())
        .unwrap_or_default();
    if roots.is_empty() || roots.len() > MAX_ROOTS {
        return Err(bad(C, format!("give `roots`: 1 to {MAX_ROOTS} folder paths from the account's top (\"Photos A\")")));
    }
    let net_cancel = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (list, account, keep) = browsers(s, p, &net_cancel, C)?;
    let job = Arc::new(ScanJob { roots, account, net_cancel, ..ScanJob::default() });
    s.cloud.scan = Some(job.clone());
    if super::bool_or(p, "wait", false) {
        run(&job, list, keep, &library);
        absorb(s, &library);
        return Ok(progress_json(&job));
    }
    let lib = library.clone();
    let j = job.clone();
    std::thread::Builder::new()
        .name("lc-cloud-scan".into())
        .spawn(move || run(&j, list, keep, &lib))
        .map_err(|e| bad(C, format!("could not start the scan: {e}")))?;
    Ok(progress_json(&job))
}

fn scan_progress(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "cloud.scanProgress";
    let library = library_dir(s, C)?;
    absorb(s, &library);
    Ok(s.cloud.scan.as_ref().map_or(Value::Null, |j| progress_json(j)))
}

fn scan_cancel(s: &mut Session, _: &Value) -> Result<Value> {
    if let Some(j) = &s.cloud.scan {
        j.progress.cancel.store(true, Ordering::Relaxed);
        j.net_cancel.store(true, Ordering::Relaxed);
    }
    Ok(Value::Null)
}

fn plan_albums(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "cloud.planAlbums";
    let library = library_dir(s, C)?;
    let organize = dir(&library);
    let meta: Value = std::fs::read(organize.join("scan.json"))
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| bad(C, "scan the folders first (cloud.scanFolders)"))?;
    let roots: Vec<String> = meta["roots"].as_array().map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default();
    let edits_path = organize.join("edits.json");
    let edits: Edits = match p.get("edits").filter(|e| !e.is_null()) {
        Some(e) => {
            let edits: Edits = serde_json::from_value(e.clone()).map_err(|e| bad(C, format!("edits: {e}")))?;
            let text = serde_json::to_string_pretty(&edits).map_err(|e| bad(C, e.to_string()))?;
            std::fs::write(&edits_path, text).map_err(|e| bad(C, format!("can't save the edits: {e}")))?;
            edits
        }
        None => match std::fs::read(&edits_path) {
            Ok(b) => serde_json::from_slice(&b).map_err(|e| bad(C, format!("{}: {e}", edits_path.display())))?,
            Err(_) => Edits::default(),
        },
    };
    let mut records: Vec<_> = load_cache(&organize.join("scan.jsonl")).into_values().filter(|r| roots.contains(&r.root)).collect();
    records.sort_by(|a, b| (&a.root, &a.path).cmp(&(&b.root, &b.path)));
    let made = plan(&roots, &records, &edits);
    let text = serde_json::to_string_pretty(&made).map_err(|e| bad(C, e.to_string()))?;
    std::fs::write(organize.join("plan.json"), text).map_err(|e| bad(C, format!("can't save the plan: {e}")))?;
    let report = organize.join("plan.html");
    std::fs::write(&report, lightcraft_organize::report::html(&made)).map_err(|e| bad(C, format!("can't save the report: {e}")))?;
    let st = &made.stats;
    Ok(json!({
        "complete": meta["complete"] == true,
        "albums": made.albums.len(),
        "photos": st.photos,
        "videos": st.videos,
        "duplicateCopies": st.duplicate_copies,
        "duplicateBytes": st.duplicate_bytes,
        "likelyDuplicates": st.likely_duplicates,
        "suggestions": made.suggestions.len(),
        "unreadable": st.unreadable,
        "report": report.to_string_lossy(),
        "plan": organize.join("plan.json").to_string_lossy(),
        "edits": edits_path.to_string_lossy(),
    }))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "cloud.scanFolders", "Scan Folders for Albums", [], None, "{roots: [folder path…], account?: id (OpenDrive), folder?: path on this computer, workers?: 1–16, wait?} — read the photos and videos under the folders without changing them (names, sizes, hashes, the metadata at the start of each file); resumable; → progress", always, scan_folders),
        cmd!(query "cloud.scanProgress", "Folder Scan Progress", [], None, "{} → {running, found, folders, toRead, read, failed, bytesRead, stop: done|cancelled|halted|failed, error}", always, scan_progress),
        cmd!(query "cloud.scanCancel", "Stop the Folder Scan", [], None, "{} — stop between files; the next scan goes on from there", always, scan_cancel),
        cmd!(query "cloud.planAlbums", "Plan Albums", [], None, "{edits?: {merge: [[album…]], rename: [[album, name]], keepBoth: [root/path…]}} — albums by trip from the last scan, duplicates found; writes plan.json and plan.html (the report) in <library>/cloud/organize/ → summary", always, plan_albums),
    ]
}
