//! Drive commands (`drive.*`, opt-in; `docs/cloud-drive.md`): files and folders the user stores
//! in the library's storage accounts, one drive over all of them, next to the library's own
//! backup (shown read-only under "LightCraft Library"). Which account holds a file, its copies
//! and its encryption are the engine's business; the commands speak of files and folders.
//!
//! Names, folders, stars and the trash change at once (and are saved to the accounts in the
//! background); uploads, downloads and deletions of stored copies are queued for the drive's
//! worker thread (`drive.transfers`). None of them is journaled.

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use lightcraft_cloud::crypto::{OsRandom, Random};
use lightcraft_cloud::manifest::MAX_DEPTH;
use lightcraft_cloud::{Availability, ContentId, Entry, EntryKind, Manifest, OpKind, placement};
use serde_json::{Value, json};

use super::{CommandSpec, always, bad, cmd, str_param};
use crate::drive::{self, DriveMeta, ItemMeta, Task, TaskState, Work, WorkerContext, now_ms};
use crate::{Result, Session};

/// The UI's id for the top of the drive.
const ROOT: &str = "root";
/// The read-only folder showing the library's backup, and its parts.
const LIB: &str = "lib";
const LIB_ORIGINALS: &str = "lib.originals";
const LIB_PREVIEWS: &str = "lib.previews";
const LIB_CATALOG: &str = "lib.catalog";
/// Most results of a search, recent or starred listing.
const MAX_RESULTS: usize = 500;
/// Most files queued by one upload of a folder.
const MAX_FOLDER_FILES: usize = 100_000;

fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The reason in an error, without the command's name.
fn why(e: &crate::EngineError) -> String {
    match e {
        crate::EngineError::BadParams { msg, .. } => msg.clone(),
        e => e.to_string(),
    }
}

fn random_hex(n: usize) -> std::result::Result<String, String> {
    let mut b = vec![0u8; n];
    OsRandom.fill(&mut b).map_err(|e| e.to_string())?;
    Ok(b.iter().map(|x| format!("{x:02x}")).collect())
}

/// The library folder, with the drive loaded; for the first load, the accounts' manifest is
/// fetched in the background (another device or a restore may have changed it).
fn ready(s: &mut Session, c: &str) -> Result<PathBuf> {
    let Some(dir) = s.library.as_ref().filter(|l| l.on_disk).map(|l| l.dir.clone()) else {
        return Err(bad(c, "the drive needs a library on disk"));
    };
    let Some(vault) = s.cloud.vault.clone() else {
        return Err(bad(c, "the drive is locked: unlock it with its passphrase (cloud.unlock)"));
    };
    if !s.cloud.drive.loaded {
        s.cloud.drive.load(&dir, &vault);
        if !vault.accounts.is_empty() {
            queue(s, &dir, "sync", "Checking your storage accounts", None, 0, Work::Pull, c)?;
        }
    }
    s.cloud.absorb_drive(&dir);
    Ok(dir)
}

fn context(s: &Session, dir: &Path) -> Option<WorkerContext> {
    Some(WorkerContext { vault: s.cloud.vault.clone()?, settings: s.cloud.settings.clone(), library: dir.to_path_buf() })
}

/// Start the worker (it saves changes to the accounts once its queue is empty).
fn kick(s: &Session, dir: &Path, c: &str) -> Result<()> {
    if s.cloud.vault.as_ref().is_none_or(|v| v.accounts.is_empty()) {
        return Ok(()); // nothing to save to yet: the changes wait in the library
    }
    let Some(ctx) = context(s, dir) else { return Ok(()) };
    s.cloud.drive.kick(ctx).map_err(|e| bad(c, format!("could not start the drive's transfers: {e}")))
}

/// Queue work listed as a transfer; its id.
#[allow(clippy::too_many_arguments)]
fn queue(s: &Session, dir: &Path, kind: &'static str, name: &str, entry: Option<&str>, size: u64, work: Work, c: &str) -> Result<String> {
    let id = format!("t-{}", random_hex(8).map_err(|e| bad(c, e))?);
    let task = Task {
        id: id.clone(),
        kind,
        name: name.to_string(),
        entry: entry.map(str::to_string),
        size,
        state: TaskState::Queued,
        error: None,
        copies: 0,
        at: now_ms(),
        file: None,
        parent: None,
    };
    s.cloud.drive.transfers.push(task, work);
    kick(s, dir, c)?;
    Ok(id)
}

/// Save a change made on this thread (in the library now, on the accounts in the background).
fn changed(s: &Session, dir: &Path, c: &str) -> Result<()> {
    let Some(vault) = &s.cloud.vault else { return Err(bad(c, "the drive is locked")) };
    s.cloud.drive.save_local(dir, vault).map_err(|e| bad(c, e))?;
    kick(s, dir, c)
}

fn record(s: &Session, kind: OpKind, c: &str) -> Result<()> {
    let device = s.cloud.settings.device.clone();
    lock(&s.cloud.drive.manifest).record(&device, kind).map(|_| ()).map_err(|e| bad(c, e.to_string()))
}

/// The entry or a trashed folder above it.
fn hidden(m: &Manifest, e: &Entry) -> bool {
    e.trashed || trashed_above(m, &e.id)
}

fn trashed_above(m: &Manifest, id: &str) -> bool {
    let mut cur = m.effective_parent(id);
    for _ in 0..MAX_DEPTH {
        let Some(p) = cur else { return false };
        match m.get(&p) {
            Some(pe) if pe.trashed => return true,
            Some(_) => cur = m.effective_parent(&p),
            None => return false,
        }
    }
    false
}

fn ui_parent(m: &Manifest, id: &str) -> String {
    m.effective_parent(id).unwrap_or_else(|| ROOT.to_string())
}

/// A folder to put things in: the top, or a drive folder that isn't in the trash.
fn target_folder(m: &Manifest, id: Option<&str>, c: &str) -> Result<Option<String>> {
    match id {
        None | Some("") | Some(ROOT) => Ok(None),
        Some(id) if id == LIB || id.starts_with("lib.") => Err(bad(c, "the LightCraft Library folder is managed by the library's backup")),
        Some(id) => match m.get(id) {
            Some(e) if drive::is_drive_entry(id) && e.kind == EntryKind::Folder && !hidden(m, &e) => Ok(Some(id.to_string())),
            Some(e) if e.kind == EntryKind::Folder => Err(bad(c, "that folder is in the trash")),
            _ => Err(bad(c, format!("no folder {id:?}"))),
        },
    }
}

/// Names in `parent` (visible drive items, plus files queued for upload there).
fn sibling_names(s: &Session, m: &Manifest, parent: Option<&str>, except: Option<&str>) -> Vec<String> {
    let mut names: Vec<String> = m
        .children(parent)
        .into_iter()
        .filter(|e| drive::is_drive_entry(&e.id) && !e.trashed && Some(e.id.as_str()) != except)
        .map(|e| e.name)
        .collect();
    names.extend(
        s.cloud
            .drive
            .transfers
            .list()
            .into_iter()
            .filter(|t| t.kind == "upload" && matches!(t.state, TaskState::Queued | TaskState::Running) && t.parent.as_deref() == parent)
            .map(|t| t.name),
    );
    names
}

/// A drive item the user may change.
fn own_item(m: &Manifest, id: &str, c: &str) -> Result<Entry> {
    if !drive::is_drive_entry(id) {
        return Err(bad(c, "files in the LightCraft Library folder are managed by the library's backup"));
    }
    m.get(id).ok_or_else(|| bad(c, format!("no item {id:?}")))
}

fn ids_param(p: &Value, c: &str) -> Result<Vec<String>> {
    let ids: Vec<String> = match (p.get("ids").and_then(Value::as_array), str_param(p, "id")) {
        (Some(a), _) => a.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        (None, Some(id)) => vec![id.to_string()],
        _ => return Err(bad(c, "missing `ids`")),
    };
    if ids.is_empty() {
        return Err(bad(c, "`ids` is empty"));
    }
    Ok(ids.into_iter().take(100_000).collect())
}

fn kind_of(name: &str) -> &'static str {
    let ext = name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase()).unwrap_or_default();
    match ext.as_str() {
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "heic" | "heif" | "avif" | "tif" | "tiff" | "bmp" | "jxl" | "dng" | "cr2" | "cr3" | "nef"
        | "nrw" | "arw" | "raf" | "rw2" | "orf" | "pef" | "srw" | "svg" => "image",
        "mp4" | "mov" | "m4v" | "mkv" | "avi" | "webm" | "mts" | "3gp" => "video",
        "mp3" | "wav" | "flac" | "aac" | "m4a" | "ogg" | "opus" => "audio",
        "pdf" | "doc" | "docx" | "odt" | "rtf" | "txt" | "md" | "xls" | "xlsx" | "ods" | "csv" | "ppt" | "pptx" | "odp" | "key" | "pages"
        | "numbers" => "document",
        "zip" | "rar" | "7z" | "tar" | "gz" | "bz2" | "xz" | "zst" => "archive",
        _ => "other",
    }
}

fn item_json(m: &Manifest, meta: &DriveMeta, e: &Entry, counts: &HashMap<String, usize>) -> Value {
    let md = meta.items.get(&e.id).cloned().unwrap_or_default();
    json!({
        "id": e.id,
        "kind": e.kind,
        "name": e.name,
        "parent": ui_parent(m, &e.id),
        "size": e.size,
        "availability": e.availability,
        "copies": e.replicas.len(),
        "starred": md.starred,
        "added": (md.added > 0).then_some(md.added),
        "modified": (md.modified > 0).then_some(md.modified),
        "trashedAt": md.trashed_at,
        "items": (e.kind == EntryKind::Folder).then(|| counts.get(&e.id).copied().unwrap_or(0)),
        "readOnly": false,
    })
}

/// Which part of the library an entry belongs to.
fn library_part(id: &str) -> Option<&'static str> {
    if id.starts_with("original.") {
        Some(LIB_ORIGINALS)
    } else if id.starts_with("smart.") {
        Some(LIB_PREVIEWS)
    } else if drive::is_library_entry(id) {
        Some(LIB_CATALOG)
    } else {
        None
    }
}

fn library_file_json(e: &Entry) -> Value {
    let original = e.id.starts_with("original.");
    let parent = if original { drive::library_dir_id(drive::dir_name(&e.name)) } else { library_part(&e.id).unwrap_or(LIB).to_string() };
    json!({
        "id": e.id,
        "kind": "file",
        "name": drive::base_name(&e.name),
        "parent": parent,
        "size": e.size,
        "availability": e.availability,
        "copies": e.replicas.len(),
        "starred": false,
        "added": null,
        "modified": null,
        "trashedAt": null,
        "items": null,
        "readOnly": true,
        "path": original.then_some(&e.name),
    })
}

fn virtual_folder(id: &str, name: &str, parent: &str, items: usize, size: u64, path: Option<&str>) -> Value {
    json!({
        "id": id, "kind": "folder", "name": name, "parent": parent, "size": size, "availability": null, "copies": 0,
        "starred": false, "added": null, "modified": null, "trashedAt": null, "items": items, "readOnly": true, "path": path,
    })
}

/// Visible children per drive folder (one pass over the manifest).
fn child_counts(m: &Manifest, entries: &[Entry]) -> HashMap<String, usize> {
    let mut counts = HashMap::new();
    for e in entries.iter().filter(|e| drive::is_drive_entry(&e.id) && !e.trashed) {
        if let Some(p) = m.effective_parent(&e.id) {
            *counts.entry(p).or_insert(0) += 1;
        }
    }
    counts
}

/// Live library entries (the backup's), grouped by part.
fn library_entries(entries: &[Entry]) -> Vec<&Entry> {
    entries.iter().filter(|e| drive::is_library_entry(&e.id) && !e.trashed && e.content.is_some()).collect()
}

fn crumbs(m: &Manifest, folder: &str, entries: &[Entry]) -> Vec<Value> {
    let lib = || json!({"id": LIB, "name": "LightCraft Library"});
    match folder {
        ROOT => vec![json!({"id": ROOT, "name": "My Drive"})],
        LIB => vec![lib()],
        LIB_ORIGINALS => vec![lib(), json!({"id": LIB_ORIGINALS, "name": "Originals"})],
        LIB_PREVIEWS => vec![lib(), json!({"id": LIB_PREVIEWS, "name": "Smart previews"})],
        LIB_CATALOG => vec![lib(), json!({"id": LIB_CATALOG, "name": "Catalog"})],
        id if id.starts_with("lib.dir.") => {
            let dir = library_entries(entries)
                .into_iter()
                .filter(|e| e.id.starts_with("original."))
                .map(|e| drive::dir_name(&e.name))
                .find(|d| drive::library_dir_id(d) == id)
                .unwrap_or("");
            vec![lib(), json!({"id": LIB_ORIGINALS, "name": "Originals"}), json!({"id": id, "name": drive::base_name(dir), "path": dir})]
        }
        id => {
            let mut out = Vec::new();
            let mut cur = Some(id.to_string());
            while let Some(c) = cur {
                if out.len() >= MAX_DEPTH {
                    break;
                }
                let Some(e) = m.get(&c) else { break };
                out.push(json!({"id": e.id, "name": e.name}));
                cur = m.effective_parent(&c);
            }
            out.push(json!({"id": ROOT, "name": "My Drive"}));
            out.reverse();
            out
        }
    }
}

fn list(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.list";
    ready(s, C)?;
    let view = str_param(p, "view").unwrap_or("folder");
    let folder = str_param(p, "folder").filter(|f| !f.is_empty()).unwrap_or(ROOT);
    let m = lock(&s.cloud.drive.manifest).clone();
    let meta = lock(&s.cloud.drive.meta).clone();
    let entries: Vec<Entry> = m.entries().collect();
    let counts = child_counts(&m, &entries);
    let drive_items = || entries.iter().filter(|e| drive::is_drive_entry(&e.id));
    let mut path = Vec::new();
    let items: Vec<Value> = match view {
        "folder" if folder == LIB => {
            path = crumbs(&m, folder, &entries);
            let lib = library_entries(&entries);
            let part = |id: &str| lib.iter().filter(|e| library_part(&e.id) == Some(id)).fold((0usize, 0u64), |(n, b), e| (n + 1, b + e.size));
            let dirs =
                lib.iter().filter(|e| e.id.starts_with("original.")).map(|e| drive::dir_name(&e.name)).collect::<std::collections::BTreeSet<_>>();
            let (o, ob) = part(LIB_ORIGINALS);
            let (pv, pb) = part(LIB_PREVIEWS);
            let (ca, cb) = part(LIB_CATALOG);
            vec![
                virtual_folder(LIB_ORIGINALS, "Originals", LIB, dirs.len().max(usize::from(o > 0)), ob, None),
                virtual_folder(LIB_PREVIEWS, "Smart previews", LIB, pv, pb, None),
                virtual_folder(LIB_CATALOG, "Catalog", LIB, ca, cb, None),
            ]
        }
        "folder" if folder == LIB_ORIGINALS => {
            path = crumbs(&m, folder, &entries);
            let mut dirs: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
            for e in library_entries(&entries).into_iter().filter(|e| e.id.starts_with("original.")) {
                let d = dirs.entry(drive::dir_name(&e.name)).or_insert((0, 0));
                d.0 += 1;
                d.1 = d.1.saturating_add(e.size);
            }
            dirs.into_iter().map(|(d, (n, b))| virtual_folder(&drive::library_dir_id(d), drive::base_name(d), LIB_ORIGINALS, n, b, Some(d))).collect()
        }
        "folder" if folder == LIB_PREVIEWS || folder == LIB_CATALOG || folder.starts_with("lib.dir.") => {
            path = crumbs(&m, folder, &entries);
            library_entries(&entries)
                .into_iter()
                .filter(|e| {
                    if folder.starts_with("lib.dir.") {
                        e.id.starts_with("original.") && drive::library_dir_id(drive::dir_name(&e.name)) == folder
                    } else {
                        library_part(&e.id) == Some(folder)
                    }
                })
                .map(library_file_json)
                .collect()
        }
        "folder" => {
            let parent = target_folder(&m, Some(folder), C)?;
            path = crumbs(&m, folder, &entries);
            m.children(parent.as_deref())
                .iter()
                .filter(|e| drive::is_drive_entry(&e.id) && !e.trashed)
                .map(|e| item_json(&m, &meta, e, &counts))
                .collect()
        }
        "recent" => {
            let mut files: Vec<&Entry> = drive_items().filter(|e| e.kind == EntryKind::File && !hidden(&m, e)).collect();
            files.sort_by_key(|e| std::cmp::Reverse(meta.items.get(&e.id).map(|x| x.modified).unwrap_or(0)));
            files.into_iter().take(MAX_RESULTS).map(|e| item_json(&m, &meta, e, &counts)).collect()
        }
        "starred" => drive_items()
            .filter(|e| meta.items.get(&e.id).is_some_and(|x| x.starred) && !hidden(&m, e))
            .take(MAX_RESULTS)
            .map(|e| item_json(&m, &meta, e, &counts))
            .collect(),
        "trash" => drive_items().filter(|e| e.trashed && !trashed_above(&m, &e.id)).map(|e| item_json(&m, &meta, e, &counts)).collect(),
        "search" => {
            let q = str_param(p, "query").unwrap_or("").trim().to_lowercase();
            if q.is_empty() {
                Vec::new()
            } else {
                let mut out: Vec<Value> = drive_items()
                    .filter(|e| e.name.to_lowercase().contains(&q) && !hidden(&m, e))
                    .take(MAX_RESULTS)
                    .map(|e| item_json(&m, &meta, e, &counts))
                    .collect();
                let room = MAX_RESULTS.saturating_sub(out.len());
                out.extend(
                    library_entries(&entries)
                        .into_iter()
                        .filter(|e| e.id.starts_with("original.") && drive::base_name(&e.name).to_lowercase().contains(&q))
                        .take(room)
                        .map(library_file_json),
                );
                out
            }
        }
        other => return Err(bad(C, format!("unknown view {other:?} (folder, recent, starred, trash or search)"))),
    };
    Ok(json!({"items": items, "path": path, "version": m.clock()}))
}

fn mkdir(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.mkdir";
    let dir = ready(s, C)?;
    let name = str_param(p, "name").and_then(drive::clean_name).ok_or_else(|| bad(C, "the folder needs a name"))?;
    let (id, created) = make_folder(s, str_param(p, "parent"), &name, super::bool_or(p, "reuse", false), C)?;
    if created {
        changed(s, &dir, C)?;
    }
    let m = lock(&s.cloud.drive.manifest).clone();
    let meta = lock(&s.cloud.drive.meta).clone();
    let e = m.get(&id).ok_or_else(|| bad(C, "the folder wasn't made"))?;
    Ok(item_json(&m, &meta, &e, &HashMap::new()))
}

/// Make a folder (or with `reuse`, find one of that name): (its id, whether it is new).
fn make_folder(s: &Session, parent: Option<&str>, name: &str, reuse: bool, c: &str) -> Result<(String, bool)> {
    let m = lock(&s.cloud.drive.manifest).clone();
    let parent = target_folder(&m, parent, c)?;
    if reuse
        && let Some(e) = m
            .children(parent.as_deref())
            .into_iter()
            .find(|e| drive::is_drive_entry(&e.id) && !e.trashed && e.kind == EntryKind::Folder && e.name == name)
    {
        return Ok((e.id, false));
    }
    let name = drive::unique_name(name, &sibling_names(s, &m, parent.as_deref(), None));
    let id = format!("{}{}", drive::PREFIX, random_hex(8).map_err(|e| bad(c, e))?);
    record(s, OpKind::Create { entry: Entry::folder(&id, &name, parent.as_deref()) }, c)?;
    let now = now_ms();
    lock(&s.cloud.drive.meta).items.insert(id.clone(), ItemMeta { added: now, modified: now, ..ItemMeta::default() });
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    Ok((id, true))
}

fn touch(s: &Session, ids: &[String]) {
    let now = now_ms();
    let mut meta = lock(&s.cloud.drive.meta);
    for id in ids {
        meta.items.entry(id.clone()).or_default().modified = now;
    }
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
}

fn rename(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.rename";
    let dir = ready(s, C)?;
    let id = str_param(p, "id").ok_or_else(|| bad(C, "missing `id`"))?.to_string();
    let name = str_param(p, "name").and_then(drive::clean_name).ok_or_else(|| bad(C, "the name can't be empty"))?;
    let m = lock(&s.cloud.drive.manifest).clone();
    let e = own_item(&m, &id, C)?;
    let parent = m.effective_parent(&id);
    let name = drive::unique_name(&name, &sibling_names(s, &m, parent.as_deref(), Some(&id)));
    if name != e.name {
        record(s, OpKind::Rename { id: id.clone(), name: name.clone() }, C)?;
        touch(s, std::slice::from_ref(&id));
        changed(s, &dir, C)?;
    }
    Ok(json!({"id": id, "name": name}))
}

fn move_items(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.move";
    let dir = ready(s, C)?;
    let ids = ids_param(p, C)?;
    let m = lock(&s.cloud.drive.manifest).clone();
    let target = target_folder(&m, str_param(p, "parent"), C)?;
    // a folder can't go inside itself
    if let Some(t) = &target {
        let mut cur = Some(t.clone());
        for _ in 0..MAX_DEPTH {
            let Some(c) = cur else { break };
            if ids.contains(&c) {
                return Err(bad(C, "a folder can't be moved into itself"));
            }
            cur = m.effective_parent(&c);
        }
    }
    let mut moved = 0usize;
    for id in &ids {
        let e = own_item(&lock(&s.cloud.drive.manifest).clone(), id, C)?;
        let now = lock(&s.cloud.drive.manifest).clone();
        if now.effective_parent(id) == target {
            continue;
        }
        let name = drive::unique_name(&e.name, &sibling_names(s, &now, target.as_deref(), Some(id)));
        if name != e.name {
            record(s, OpKind::Rename { id: id.clone(), name }, C)?;
        }
        record(s, OpKind::Move { id: id.clone(), parent: target.clone() }, C)?;
        moved += 1;
    }
    if moved > 0 {
        touch(s, &ids);
        changed(s, &dir, C)?;
    }
    Ok(json!({"moved": moved}))
}

fn star(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.star";
    let dir = ready(s, C)?;
    let ids = ids_param(p, C)?;
    let starred = super::bool_or(p, "starred", true);
    let m = lock(&s.cloud.drive.manifest).clone();
    for id in &ids {
        own_item(&m, id, C)?;
    }
    {
        let mut meta = lock(&s.cloud.drive.meta);
        for id in &ids {
            let item = meta.items.entry(id.clone()).or_default();
            item.starred = starred;
            item.modified = item.modified.max(1);
        }
    }
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    changed(s, &dir, C)?;
    Ok(json!({"starred": starred, "count": ids.len()}))
}

fn trash(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.trash";
    let dir = ready(s, C)?;
    let ids = ids_param(p, C)?;
    let m = lock(&s.cloud.drive.manifest).clone();
    for id in &ids {
        own_item(&m, id, C)?;
    }
    let now = now_ms();
    for id in &ids {
        record(s, OpKind::SetTrashed { id: id.clone(), trashed: true }, C)?;
        lock(&s.cloud.drive.meta).items.entry(id.clone()).or_default().trashed_at = Some(now);
    }
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    changed(s, &dir, C)?;
    Ok(json!({"trashed": ids.len()}))
}

fn restore(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.restore";
    let dir = ready(s, C)?;
    let mut ids = ids_param(p, C)?;
    // folders before what is in them: a child restored with its folder goes back into it
    {
        let m = lock(&s.cloud.drive.manifest).clone();
        let depth = |id: &str| {
            let (mut n, mut cur) = (0usize, m.effective_parent(id));
            while let Some(p) = cur.filter(|_| n < MAX_DEPTH) {
                n += 1;
                cur = m.effective_parent(&p);
            }
            n
        };
        ids.sort_by_cached_key(|id| depth(id));
    }
    for id in &ids {
        let m = lock(&s.cloud.drive.manifest).clone();
        let e = own_item(&m, id, C)?;
        if !e.trashed {
            continue;
        }
        // back where it was, or at the top when that folder is gone or in the trash (like Drive)
        let parent = m.effective_parent(id).filter(|p| m.get(p).is_some_and(|f| !hidden(&m, &f)));
        if parent != m.effective_parent(id) {
            record(s, OpKind::Move { id: id.clone(), parent: None }, C)?;
        }
        let name = drive::unique_name(&e.name, &sibling_names(s, &m, parent.as_deref(), Some(id)));
        if name != e.name {
            record(s, OpKind::Rename { id: id.clone(), name }, C)?;
        }
        record(s, OpKind::SetTrashed { id: id.clone(), trashed: false }, C)?;
        lock(&s.cloud.drive.meta).items.entry(id.clone()).or_default().trashed_at = None;
    }
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    changed(s, &dir, C)?;
    Ok(json!({"restored": ids.len()}))
}

/// Delete items and everything in them for good; their stored copies are deleted in the
/// background.
fn delete_forever(s: &mut Session, dir: &Path, ids: &[String], c: &str) -> Result<usize> {
    let m = lock(&s.cloud.drive.manifest).clone();
    for id in ids {
        own_item(&m, id, c)?;
    }
    // everything inside, by parent (one pass)
    let mut by_parent: HashMap<String, Vec<String>> = HashMap::new();
    for e in m.entries().filter(|e| drive::is_drive_entry(&e.id)) {
        if let Some(p) = m.effective_parent(&e.id) {
            by_parent.entry(p).or_default().push(e.id);
        }
    }
    let mut all: Vec<String> = Vec::new();
    let mut stack: Vec<String> = ids.to_vec();
    let mut seen = std::collections::HashSet::new();
    while let Some(id) = stack.pop() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(kids) = by_parent.get(&id) {
            stack.extend(kids.iter().cloned());
        }
        all.push(id);
    }
    let mut gone: Vec<(String, String, ContentId)> = Vec::new();
    for id in &all {
        if let Some(e) = m.get(id) {
            if let Some(content) = &e.content {
                gone.extend(e.replicas.iter().map(|(a, k)| (a.clone(), k.clone(), content.clone())));
            }
            record(s, OpKind::Delete { id: id.clone() }, c)?;
        }
        lock(&s.cloud.drive.meta).items.remove(id);
    }
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    s.cloud.drive.save_local(dir, s.cloud.vault.as_ref().ok_or_else(|| bad(c, "the drive is locked"))?).map_err(|e| bad(c, e))?;
    if gone.is_empty() {
        kick(s, dir, c)?;
    } else {
        let label = if all.len() == 1 { "Deleting 1 item".to_string() } else { format!("Deleting {} items", all.len()) };
        queue(s, dir, "delete", &label, None, 0, Work::Purge { gone }, c)?;
    }
    Ok(all.len())
}

fn delete(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.delete";
    let dir = ready(s, C)?;
    let ids = ids_param(p, C)?;
    let n = delete_forever(s, &dir, &ids, C)?;
    Ok(json!({"deleted": n}))
}

fn empty_trash(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "drive.emptyTrash";
    let dir = ready(s, C)?;
    let m = lock(&s.cloud.drive.manifest).clone();
    let ids: Vec<String> = m.entries().filter(|e| drive::is_drive_entry(&e.id) && e.trashed && !trashed_above(&m, &e.id)).map(|e| e.id).collect();
    if ids.is_empty() {
        return Ok(json!({"deleted": 0}));
    }
    let n = delete_forever(s, &dir, &ids, C)?;
    Ok(json!({"deleted": n}))
}

fn level(p: &Value, c: &str) -> Result<Availability> {
    match str_param(p, "level") {
        None => Ok(Availability::Standard),
        Some(v) => serde_json::from_value(json!(v)).map_err(|_| bad(c, "`level` is instant, fast, standard, slow or archive")),
    }
}

/// Queue one file for upload into `parent`: (task id, entry id, its name).
#[allow(clippy::too_many_arguments)]
fn queue_upload(
    s: &Session,
    dir: &Path,
    source: PathBuf,
    parent: Option<String>,
    name: &str,
    level: Availability,
    copies: u8,
    staged: bool,
    c: &str,
) -> Result<(String, String, String)> {
    let size = std::fs::metadata(&source).map_err(|e| bad(c, format!("can't read {}: {e}", source.display())))?.len();
    if size > crate::cloud::MAX_FILE {
        return Err(bad(c, format!("{name}: files over {} GB can't be stored yet", crate::cloud::MAX_FILE / 1_000_000_000)));
    }
    let m = lock(&s.cloud.drive.manifest).clone();
    let name = drive::unique_name(name, &sibling_names(s, &m, parent.as_deref(), None));
    let entry_id = format!("{}{}", drive::PREFIX, random_hex(8).map_err(|e| bad(c, e))?);
    let mut entry = Entry::file(&entry_id, &name, parent.as_deref(), ContentId::of(b""), size);
    entry.availability = level;
    let now = now_ms();
    lock(&s.cloud.drive.meta).items.insert(entry_id.clone(), ItemMeta { added: now, modified: now, ..ItemMeta::default() });
    s.cloud.drive.transfers.meta_dirty.store(true, Ordering::Relaxed);
    let task_id = format!("t-{}", random_hex(8).map_err(|e| bad(c, e))?);
    let task = Task {
        id: task_id.clone(),
        kind: "upload",
        name: name.clone(),
        entry: Some(entry_id.clone()),
        size,
        state: TaskState::Queued,
        error: None,
        copies: 0,
        at: now,
        file: None,
        parent: parent.clone(),
    };
    s.cloud.drive.transfers.push(task, Work::Upload { entry, copies, source, staged });
    kick(s, dir, c)?;
    Ok((task_id, entry_id, name))
}

fn upload(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.upload";
    let dir = ready(s, C)?;
    if s.cloud.vault.as_ref().is_none_or(|v| v.accounts.is_empty()) {
        return Err(bad(C, "add a storage account first (Storage ▸ Add storage)"));
    }
    let source = str_param(p, "path").filter(|p| !p.is_empty()).map(PathBuf::from).ok_or_else(|| bad(C, "missing `path`"))?;
    let staged = super::bool_or(p, "staged", false);
    if staged {
        // only a file handed over through the library's inbox is removed after the upload
        let inbox = std::fs::canonicalize(drive::inbox(&dir)).map_err(|e| bad(C, format!("no upload inbox: {e}")))?;
        let real = std::fs::canonicalize(&source).map_err(|e| bad(C, format!("can't read {}: {e}", source.display())))?;
        if !real.starts_with(&inbox) || !real.is_file() {
            return Err(bad(C, "`staged` files must be in the drive's inbox (drive.status → inbox)"));
        }
    }
    let level = level(p, C)?;
    let copies = match p.get("copies") {
        None | Some(Value::Null) => 1,
        Some(v) => v.as_u64().filter(|n| (1..=u64::from(placement::MAX_COPIES)).contains(n)).ok_or_else(|| bad(C, "`copies` is 1–8"))? as u8,
    };
    let m = lock(&s.cloud.drive.manifest).clone();
    let parent = target_folder(&m, str_param(p, "parent"), C)?;
    let fallback = source.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    // a name given is the name (one that can't be a name is refused, never swapped for the
    // file's: a staged upload's file name is the inbox's own)
    let name = match str_param(p, "name") {
        Some(n) => drive::clean_name(n).ok_or_else(|| bad(C, format!("\"{n}\" can't be a file name")))?,
        None => drive::clean_name(&fallback).ok_or_else(|| bad(C, "the file needs a name"))?,
    };

    if !staged && source.is_dir() {
        // a whole folder: its folders are made now, its files queued; a file that can't be (too
        // big, unreadable) is reported and the rest goes on
        let mut tasks = Vec::new();
        let mut skipped: Vec<Value> = Vec::new();
        let (top, _) = make_folder(s, parent.as_deref(), &name, false, C)?;
        let mut stack = vec![(source.clone(), top.clone(), 0usize)];
        while let Some((path, folder, depth)) = stack.pop() {
            let Ok(rd) = std::fs::read_dir(&path) else { continue };
            let mut kids: Vec<_> = rd.flatten().collect();
            kids.sort_by_key(|e| e.file_name());
            for e in kids {
                if tasks.len() >= MAX_FOLDER_FILES {
                    break;
                }
                let n = e.file_name().to_string_lossy().to_string();
                let Some(n) = drive::clean_name(&n).filter(|n| !n.starts_with('.')) else { continue };
                let Ok(t) = e.file_type() else { continue };
                if t.is_dir() && depth < 32 {
                    let (sub, _) = make_folder(s, Some(&folder), &n, true, C)?;
                    stack.push((e.path(), sub, depth + 1));
                } else if t.is_file() {
                    match queue_upload(s, &dir, e.path(), Some(folder.clone()), &n, level, copies, false, C) {
                        Ok((task, _, _)) => tasks.push(task),
                        Err(err) => skipped.push(json!({"name": e.path().to_string_lossy(), "error": why(&err)})),
                    }
                }
            }
        }
        changed(s, &dir, C)?;
        if super::bool_or(p, "wait", false) {
            wait_idle(s, &Value::Null)?;
        }
        return Ok(json!({"folder": top, "tasks": tasks, "skipped": skipped}));
    }
    if !source.is_file() {
        return Err(bad(C, format!("{} is not a file", source.display())));
    }
    let (task, id, name) = queue_upload(s, &dir, source, parent, &name, level, copies, staged, C)?;
    if super::bool_or(p, "wait", false) {
        wait_idle(s, &Value::Null)?;
    }
    Ok(json!({"task": task, "id": id, "name": name}))
}

fn download(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.download";
    let dir = ready(s, C)?;
    let id = str_param(p, "id").ok_or_else(|| bad(C, "missing `id`"))?.to_string();
    let e = lock(&s.cloud.drive.manifest).get(&id).ok_or_else(|| bad(C, format!("no file {id:?}")))?;
    if e.kind != EntryKind::File || e.content.is_none() {
        return Err(bad(C, "only files can be downloaded"));
    }
    let out = drive::outbox(&dir);
    drive::clean_dir(&out, drive::OUTBOX_AGE);
    std::fs::create_dir_all(&out).map_err(|e| bad(C, format!("can't create {}: {e}", out.display())))?;
    let name = drive::clean_name(drive::base_name(&e.name)).unwrap_or_else(|| "file".into());
    // the file in the outbox gets a plain name (a user's name can hold characters some systems
    // refuse, like `:` or `?`); the browser gets the real name from drive.file
    let ext: String = name.rsplit_once('.').map(|(_, x)| x).unwrap_or("").chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    let dest = out.join(format!("{}{}{ext}", random_hex(8).map_err(|e| bad(C, e))?, if ext.is_empty() { "" } else { "." }));
    let task = queue(s, &dir, "download", &name, Some(&id), e.size, Work::Download { id: id.clone(), dest }, C)?;
    Ok(json!({"task": task}))
}

fn task_json(t: &Task) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.insert("ready".into(), json!(t.file.is_some()));
    }
    v
}

fn transfers(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "drive.transfers";
    ready(s, C)?;
    let t = &s.cloud.drive.transfers;
    Ok(json!({
        "tasks": t.list().iter().map(task_json).collect::<Vec<_>>(),
        "busy": t.busy(),
        "unsynced": s.cloud.drive.unsynced(),
        "syncError": lock(&t.sync_error).clone(),
    }))
}

/// A finished download's file (for `lightcraft-cli drive` to send to the browser).
fn file(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.file";
    let dir = ready(s, C)?;
    let id = str_param(p, "task").ok_or_else(|| bad(C, "missing `task`"))?;
    let t = s.cloud.drive.transfers.list().into_iter().find(|t| t.id == id).ok_or_else(|| bad(C, format!("no transfer {id:?}")))?;
    let path = t.file.filter(|f| f.starts_with(drive::outbox(&dir)) && f.is_file()).ok_or_else(|| bad(C, "the download isn't ready"))?;
    Ok(json!({"path": path.to_string_lossy(), "name": t.name, "size": t.size}))
}

fn status(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "drive.status";
    let Some(dir) = s.library.as_ref().filter(|l| l.on_disk).map(|l| l.dir.clone()) else {
        return Ok(json!({"available": false, "why": "the drive needs a library on disk"}));
    };
    let configured = crate::cloud::CloudState::configured(&dir);
    if !s.cloud.unlocked() {
        return Ok(json!({"available": true, "configured": configured, "unlocked": false, "library": dir.to_string_lossy()}));
    }
    ready(s, C)?;
    let now = (s.clock)();
    s.cloud.absorb(&dir, &now);
    let m = lock(&s.cloud.drive.manifest).clone();
    let entries: Vec<Entry> = m.entries().collect();
    let mut kinds: BTreeMap<&str, (usize, u64)> = BTreeMap::new();
    let (mut files, mut folders, mut bytes, mut trash, mut trash_bytes) = (0usize, 0usize, 0u64, 0usize, 0u64);
    for e in entries.iter().filter(|e| drive::is_drive_entry(&e.id)) {
        if hidden(&m, e) {
            if e.kind == EntryKind::File {
                trash += 1;
                trash_bytes = trash_bytes.saturating_add(e.size);
            }
            continue;
        }
        match e.kind {
            EntryKind::Folder => folders += 1,
            EntryKind::File => {
                files += 1;
                bytes = bytes.saturating_add(e.size);
                let k = kinds.entry(kind_of(&e.name)).or_insert((0, 0));
                k.0 += 1;
                k.1 = k.1.saturating_add(e.size);
            }
        }
    }
    let lib = library_entries(&entries);
    let lib_bytes = lib.iter().fold(0u64, |n, e| n.saturating_add(e.size));
    let photos = lib.iter().filter(|e| e.id.starts_with("original.")).count();

    // space over every account: what LightCraft stores there (copies count) and what's left
    let (mut stored_total, mut capacity_total, mut free_total) = (0u64, 0u64, 0u64);
    let mut accounts = Vec::new();
    for a in s.cloud.vault.as_ref().map(|v| v.accounts.clone()).unwrap_or_default() {
        let settings = s.cloud.settings.accounts.get(&a.id).cloned().unwrap_or_default();
        let stored = entries.iter().filter(|e| e.replicas.contains_key(&a.id)).fold(0u64, |n, e| n.saturating_add(e.size));
        let (quota, error) = match s.cloud.quotas.get(&a.id) {
            Some(Ok(q)) => (Some(*q), None),
            Some(Err(e)) => (None, Some(e.clone())),
            None => (None, None),
        };
        let total = quota.and_then(|q| q.total);
        let capacity = settings.cap.or_else(|| total.map(|t| (t as f64 * placement::DEFAULT_CAP_SHARE) as u64));
        let free =
            quota.map(|q| placement::usable_free(q.total, q.used, settings.cap, stored)).or_else(|| settings.cap.map(|c| c.saturating_sub(stored)));
        stored_total = stored_total.saturating_add(stored);
        capacity_total = capacity_total.saturating_add(capacity.unwrap_or(0));
        if settings.accepts_uploads {
            free_total = free_total.saturating_add(free.unwrap_or(0));
        }
        accounts.push(json!({
            "id": a.id, "label": a.label, "provider": a.provider, "stored": stored, "capacity": capacity, "free": free,
            "quota": quota.map(|q| json!({"total": q.total, "used": q.used})), "error": error, "acceptsUploads": settings.accepts_uploads,
        }));
    }
    let t = &s.cloud.drive.transfers;
    let tasks = t.list();
    let active = tasks.iter().filter(|x| matches!(x.state, TaskState::Queued | TaskState::Running) && x.kind != "sync").count();
    let failed = tasks.iter().filter(|x| x.state == TaskState::Failed && x.kind != "sync").count();
    Ok(json!({
        "available": true,
        "configured": configured,
        "unlocked": true,
        "library": dir.to_string_lossy(),
        "version": m.clock(),
        "files": files,
        "folders": folders,
        "bytes": bytes,
        "byKind": kinds.into_iter().map(|(k, (n, b))| (k.to_string(), json!({"files": n, "bytes": b}))).collect::<serde_json::Map<_, _>>(),
        "trash": {"files": trash, "bytes": trash_bytes},
        "lightcraft": {"files": lib.len(), "photos": photos, "bytes": lib_bytes, "lastBackup": s.cloud.settings.last_backup, "job": s.cloud.job.as_ref().map(|j| j.json())},
        "usage": {"stored": stored_total, "capacity": capacity_total, "free": free_total},
        "accounts": accounts,
        "transfers": {"active": active, "failed": failed, "busy": t.busy()},
        "synced": !s.cloud.drive.unsynced(),
        "syncError": lock(&t.sync_error).clone(),
        "inbox": drive::inbox(&dir).to_string_lossy(),
    }))
}

/// Block until the worker has done everything queued and saved to the accounts (CLI, MCP and
/// scripts, whose process would otherwise end with transfers still queued).
fn wait_idle(s: &mut Session, p: &Value) -> Result<Value> {
    const C: &str = "drive.wait";
    let dir = ready(s, C)?;
    let timeout = std::time::Duration::from_millis(p.get("timeoutMs").and_then(Value::as_u64).unwrap_or(3_600_000));
    let start = std::time::Instant::now();
    while s.cloud.drive.transfers.busy() {
        if start.elapsed() >= timeout {
            return Err(bad(C, "the drive's transfers are still running"));
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    s.cloud.absorb_drive(&dir);
    transfers(s, &Value::Null)
}

fn sync(s: &mut Session, _: &Value) -> Result<Value> {
    const C: &str = "drive.sync";
    let dir = ready(s, C)?;
    if s.cloud.vault.as_ref().is_none_or(|v| v.accounts.is_empty()) {
        return Ok(json!({"task": null}));
    }
    let task = queue(s, &dir, "sync", "Checking your storage accounts", None, 0, Work::Pull, C)?;
    Ok(json!({"task": task}))
}

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "drive.status", "Drive Status", [], None, "{} → {available, configured, unlocked, files, folders, bytes, byKind: {image|video|audio|document|archive|other: {files, bytes}}, trash, lightcraft: {files, photos, bytes, lastBackup, job}, usage: {stored, capacity, free}, accounts: [{id, label, provider, stored, capacity, free, quota, error, acceptsUploads}], transfers: {active, failed, busy}, synced, syncError, inbox, version}", always, status),
        cmd!(query "drive.list", "List Drive", [], None, "{view?: folder|recent|starred|trash|search, folder?: root|lib|lib.originals|lib.previews|lib.catalog|lib.dir.…|<folder id>, query?} → {items: [{id, kind, name, parent, size, availability, copies, starred, added, modified, trashedAt, items, readOnly, path?}], path: [{id, name}], version}. \"lib\" is the read-only LightCraft Library folder (the library's backup)", always, list),
        cmd!(query "drive.mkdir", "New Drive Folder", [], None, "{name, parent?: folder id (default: the top), reuse?: bool (return an existing folder of that name)} → item", always, mkdir),
        cmd!(query "drive.rename", "Rename Drive Item", [], None, "{id, name} → {id, name} (made unique in its folder, like \"name (1).ext\")", always, rename),
        cmd!(query "drive.move", "Move Drive Items", [], None, "{ids, parent: folder id | root} → {moved}", always, move_items),
        cmd!(query "drive.star", "Star Drive Items", [], None, "{ids, starred?: bool (default true)}", always, star),
        cmd!(query "drive.trash", "Move Drive Items to Trash", [], None, "{ids} — their stored copies stay until deleted forever", always, trash),
        cmd!(query "drive.restore", "Restore Drive Items", [], None, "{ids} — out of the trash, into their folder (or the top when it is gone)", always, restore),
        cmd!(query "drive.delete", "Delete Drive Items Forever", [], None, "{ids} — with everything in them; stored copies are deleted in the background → {deleted}", always, delete),
        cmd!(query "drive.emptyTrash", "Empty Drive Trash", [], None, "{} → {deleted}", always, empty_trash),
        cmd!(query "drive.upload", "Upload to Drive", [], None, "{path, parent?: folder id, name?, level?: instant|fast|standard|slow|archive, copies?: 1–8, staged?: bool (a file in the inbox, removed once stored), wait?: bool (until stored)} — a file is queued → {task, id, name}; a folder is made with its folders and its files queued → {folder, tasks}", always, upload),
        cmd!(query "drive.download", "Download from Drive", [], None, "{id} — queue decrypting the file into the outbox → {task}; when done, drive.file {task} gives its path", always, download),
        cmd!(query "drive.transfers", "Drive Transfers", [], None, "{} → {tasks: [{id, kind: upload|download|delete|sync, name, entry, size, state: queued|running|done|failed|cancelled, error, copies, at, ready}], busy, unsynced, syncError}", always, transfers),
        cmd!(query "drive.file", "Downloaded Drive File", [], None, "{task} → {path, name, size}", always, file),
        cmd!(query "drive.cancel", "Cancel Drive Transfer", [], None, "{task} — a queued one", always, |s, p| {
            ready(s, "drive.cancel")?;
            let id = str_param(p, "task").ok_or_else(|| bad("drive.cancel", "missing `task`"))?;
            Ok(json!({"cancelled": s.cloud.drive.transfers.cancel(id)}))
        }),
        cmd!(query "drive.clearTransfers", "Clear Finished Drive Transfers", [], None, "{}", always, |s, _| {
            ready(s, "drive.clearTransfers")?;
            s.cloud.drive.transfers.clear_finished();
            Ok(Value::Null)
        }),
        cmd!(query "drive.wait", "Wait for Drive Transfers", [], None, "{timeoutMs?} — block until every queued transfer is done and saved to the accounts (scripts: run it before the process ends) → drive.transfers", always, wait_idle),
        cmd!(query "drive.sync", "Sync Drive", [], None, "{} — read the accounts' copy of the drive (another device, a restore) and save this one → {task}", always, sync),
    ]
}
