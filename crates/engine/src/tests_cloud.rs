//! Cloud backup end to end with folder accounts: set up, connect, back up (and again: nothing
//! re-sent), restore on "another machine" into a new folder, the restored library opening with
//! its edits and originals relinked; wrong passphrases, unplugged accounts and secrets kept out
//! of the journal.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::Session;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-cloud-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_png(path: &Path, seed: u8) {
    let (w, h) = (40usize, 24usize);
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i % w * 6) as u8, (i / w * 9) as u8, seed, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    let bytes = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, bytes).unwrap();
}

const PASS: &str = "correct horse battery";

/// A library with three photos added in place and one copied into it, one of them edited, and
/// two folder accounts with room for it.
fn backed_up_library(tag: &str) -> (Session, PathBuf, PathBuf, PathBuf, PathBuf) {
    let src = temp_dir(&format!("{tag}-src"));
    let lib = temp_dir(&format!("{tag}-lib"));
    let (acc1, acc2) = (temp_dir(&format!("{tag}-acc1")), temp_dir(&format!("{tag}-acc2")));
    for i in 0..3u8 {
        write_png(&src.join(format!("trip/p{i}.png")), i * 40);
    }
    write_png(&src.join("copied/c.png"), 200);
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("library.import", &json!({"paths": [src.join("trip").to_string_lossy()]})).unwrap();
    s.execute("library.import", &json!({"paths": [src.join("copied").to_string_lossy()], "mode": "copy"})).unwrap();
    assert_eq!(s.catalog.len(), 4);
    let first = s.catalog.photos().next().unwrap().id;
    s.execute("library.select", &json!({"ids": [first.0], "active": first.0})).unwrap();
    s.execute("develop.set", &json!({"control": "light.exposure", "value": 1.25})).unwrap();

    s.execute("cloud.setup", &json!({"passphrase": PASS})).unwrap();
    for (dir, label) in [(&acc1, "NAS"), (&acc2, "USB disk")] {
        s.execute(
            "cloud.addAccount",
            &json!({"provider": "folder", "label": label, "config": {"path": dir.join("lc").to_string_lossy()}, "cap": 50_000_000u64}),
        )
        .unwrap();
    }
    (s, lib, src, acc1, acc2)
}

fn files_under(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| if e.path().is_dir() { files_under(&e.path()) } else { 1 }).sum()).unwrap_or(0)
}

#[test]
fn back_up_and_restore_a_library_on_another_machine() {
    let (mut s, lib, src, acc1, acc2) = backed_up_library("roundtrip");
    let r = s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(r["failed"], 0, "{r}");
    assert_eq!(r["running"], false);
    let stored = r["stored"].as_u64().unwrap();
    assert!(stored >= 4 + 2, "four originals, the catalog files, library info: {r}");
    // the vault, the manifest and each account's marker are on both accounts
    for acc in [&acc1, &acc2] {
        for f in ["vault.enc", "account.json"] {
            assert!(acc.join("lc").join(f).is_file(), "{f} on {}", acc.display());
        }
        assert!(acc.join("lc/manifest").is_dir());
    }
    // nothing changed: nothing is sent again
    let again = s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!((again["stored"].as_u64(), again["failed"].as_u64()), (Some(0), Some(0)), "{again}");
    let status = s.execute("cloud.status", &Value::Null).unwrap();
    assert!(status["lastBackup"].is_string(), "{status}");
    assert_eq!(status["accounts"].as_array().unwrap().len(), 2);
    let edited_path = match &s.catalog.photos().next().unwrap().source {
        lightcraft_catalog::Source::File { path } => path.clone(),
        _ => panic!(),
    };
    let edited_bytes = std::fs::read(&edited_path).unwrap();
    drop(s);

    // another machine: the originals are gone, only one account and the passphrase are known
    std::fs::remove_dir_all(&src).unwrap();
    std::fs::remove_dir_all(&lib).unwrap();
    let new_lib = temp_dir("roundtrip-restored");
    let _ = std::fs::remove_dir_all(&new_lib);
    let mut s2 = Session::new().with_fs();
    let r = s2
        .execute("cloud.restore", &json!({"dir": new_lib.to_string_lossy(), "provider": "folder", "config": {"path": acc2.join("lc").to_string_lossy()}, "passphrase": PASS, "wait": true}))
        .unwrap();
    assert_eq!(r["failed"], 0, "{r}");
    let library = &r["library"];
    assert_eq!((library["photos"].as_u64(), library["missing"].as_u64()), (Some(4), Some(0)), "{r}");
    assert_eq!(library["relinked"], 4);
    // the edit came back, and the edited photo's original has the same bytes
    let restored = s2.catalog.photos().find(|p| p.develop.light.exposure == 1.25).expect("the edited photo");
    let lightcraft_catalog::Source::File { path } = &restored.source else { panic!() };
    assert!(Path::new(path).starts_with(&new_lib), "restored inside the new library: {path}");
    assert_eq!(std::fs::read(path).unwrap(), edited_bytes);
    // the restored library is set up to keep backing up to the same accounts
    s2.execute("cloud.unlock", &json!({"passphrase": PASS})).unwrap();
    assert_eq!(s2.execute("cloud.status", &Value::Null).unwrap()["accounts"].as_array().unwrap().len(), 2);
    let r = s2.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(r["failed"], 0, "{r}");

    for d in [new_lib, acc1, acc2] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn passphrases_accounts_and_failures() {
    let (mut s, lib, src, acc1, acc2) = backed_up_library("errors");
    // secrets are never journaled
    assert!(s.journal.iter().all(|(id, _)| !id.starts_with("cloud.")), "{:?}", s.journal);

    s.execute("cloud.lock", &Value::Null).unwrap();
    assert!(s.execute("cloud.backup", &json!({"wait": true})).is_err(), "locked");
    assert!(s.execute("cloud.unlock", &json!({"passphrase": "wrong passphrase"})).is_err());
    s.execute("cloud.unlock", &json!({"passphrase": PASS})).unwrap();
    assert!(s.execute("cloud.setup", &json!({"passphrase": PASS})).is_err(), "already set up");
    assert!(s.execute("cloud.changePassphrase", &json!({"old": "nope", "new": "another long one"})).is_err());
    s.execute("cloud.changePassphrase", &json!({"old": PASS, "new": "another long one"})).unwrap();
    s.execute("cloud.lock", &Value::Null).unwrap();
    s.execute("cloud.unlock", &json!({"passphrase": "another long one"})).unwrap();

    // an account that doesn't work isn't added
    let r = s.execute("cloud.addAccount", &json!({"provider": "webdav", "config": {"url": "not a url", "user": "", "password": ""}}));
    assert!(r.is_err());
    assert!(s.execute("cloud.addAccount", &json!({"provider": "terabox", "config": {}})).is_err());

    // an unplugged account: the other one still takes the backup, and says why it couldn't
    std::fs::remove_dir_all(&acc1).unwrap();
    let r = s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(r["failed"], 0, "{r}");
    let status = s.execute("cloud.status", &Value::Null).unwrap();
    let errors: Vec<&Value> = status["accounts"].as_array().unwrap().iter().filter(|a| !a["error"].is_null()).collect();
    assert_eq!(errors.len(), 1, "{status}");
    assert!(!acc1.exists(), "a missing account folder is never created");

    // restoring needs the right passphrase and a new folder
    let target = temp_dir("errors-restored");
    let source = json!({"path": acc2.join("lc").to_string_lossy()});
    let r = s.execute(
        "cloud.restore",
        &json!({"dir": target.to_string_lossy(), "provider": "folder", "config": source, "passphrase": PASS, "wait": true}),
    );
    assert!(r.is_err(), "the old passphrase no longer opens the vault on the accounts? {r:?}");
    let r = s.execute(
        "cloud.restore",
        &json!({"dir": lib.to_string_lossy(), "provider": "folder", "config": source, "passphrase": "another long one", "wait": true}),
    );
    assert!(r.unwrap_err().to_string().contains("already holds a library"));
    assert_eq!(files_under(&target), 0, "a failed restore leaves nothing behind");

    // removing an account keeps its files
    let id = status["accounts"][1]["id"].as_str().unwrap().to_string();
    s.execute("cloud.removeAccount", &json!({ "id": id })).unwrap();
    assert!(acc2.join("lc/vault.enc").is_file());

    drop(s);
    for d in [lib, src, acc2, target] {
        let _ = std::fs::remove_dir_all(d);
    }
}

#[test]
fn cloud_needs_a_library_on_disk() {
    let mut s = Session::with_demo();
    assert_eq!(s.execute("cloud.status", &Value::Null).unwrap()["available"], false);
    assert!(s.execute("cloud.setup", &json!({"passphrase": PASS})).is_err());
}

#[test]
fn restore_paths_stay_inside_the_new_library() {
    use crate::cloud::restore_path;
    let new = Path::new("/new/lib");
    let old = Path::new("/old/lib");
    assert_eq!(restore_path(new, Some(old), "/old/lib/Originals/2026/a.jpg"), Some(PathBuf::from("/new/lib/Originals/2026/a.jpg")));
    assert_eq!(restore_path(new, Some(old), "/Users/me/Pictures/b.jpg"), Some(PathBuf::from("/new/lib/Restored Originals/Users/me/Pictures/b.jpg")));
    assert_eq!(restore_path(new, None, "/a/../../etc/passwd"), None);
    assert_eq!(restore_path(new, None, "/"), None);
    assert_eq!(restore_path(new, None, "relative/c.jpg"), Some(PathBuf::from("/new/lib/Restored Originals/relative/c.jpg")));
}

/// Poll `cloud.progress` until the job ends (the app's frame loop does the same).
fn wait_for_job(s: &mut Session) -> Value {
    let start = std::time::Instant::now();
    loop {
        let p = s.execute("cloud.progress", &Value::Null).unwrap();
        if p["running"] == false {
            return p;
        }
        assert!(start.elapsed() < std::time::Duration::from_secs(60), "the job never ended: {p}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn backup_on_a_worker_thread_reports_progress_and_can_be_cancelled() {
    let (mut s, lib, src, acc1, acc2) = backed_up_library("background");
    let started = s.execute("cloud.backup", &Value::Null).unwrap();
    assert_eq!(started["what"], "backup");
    // one job at a time; account changes wait for it
    if s.execute("cloud.progress", &Value::Null).unwrap()["running"] == true {
        assert!(s.execute("cloud.backup", &Value::Null).is_err());
        assert!(s.execute("cloud.removeAccount", &json!({"id": "x"})).is_err());
    }
    let done = wait_for_job(&mut s);
    assert_eq!((done["failed"].as_u64(), done["error"].is_null()), (Some(0), true), "{done}");
    assert!(s.execute("cloud.status", &Value::Null).unwrap()["lastBackup"].is_string());

    // a cancelled backup stops between files and reports it; nothing is trashed by it
    s.execute("cloud.backup", &Value::Null).unwrap();
    s.execute("cloud.cancel", &Value::Null).unwrap();
    let cancelled = wait_for_job(&mut s);
    assert_eq!(cancelled["cancelled"], true, "{cancelled}");
    let r = s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(r["stored"], 0, "everything was already stored: {r}");

    drop(s);
    for d in [lib, src, acc1, acc2] {
        let _ = std::fs::remove_dir_all(d);
    }
}
