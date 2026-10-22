//! The drive end to end with folder accounts: folders and uploads, names kept unique, moves,
//! stars, the trash, deleting stored copies, downloads, the library's backup shown read-only
//! next to the drive (and never trashing drive files), and the drive surviving a restart.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Session;

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-drive-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

const PASS: &str = "correct horse battery";

/// A library with the drive set up and two folder accounts.
fn drive(tag: &str) -> (Session, PathBuf, PathBuf, PathBuf) {
    let lib = temp_dir(&format!("{tag}-lib"));
    let (acc1, acc2) = (temp_dir(&format!("{tag}-acc1")), temp_dir(&format!("{tag}-acc2")));
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("cloud.setup", &json!({"passphrase": PASS})).unwrap();
    for (dir, label) in [(&acc1, "NAS"), (&acc2, "USB disk")] {
        s.execute(
            "cloud.addAccount",
            &json!({"provider": "folder", "label": label, "config": {"path": dir.join("lc").to_string_lossy()}, "cap": 50_000_000u64}),
        )
        .unwrap();
    }
    (s, lib, acc1, acc2)
}

/// Wait for the drive's worker to finish everything queued (and save to the accounts).
fn settle(s: &mut Session) -> Value {
    let start = Instant::now();
    loop {
        let t = s.execute("drive.transfers", &json!({})).unwrap();
        if t["busy"] == false && t["unsynced"] == false {
            return t;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "the drive never settled: {t}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn names(s: &mut Session, folder: &str) -> Vec<String> {
    let r = s.execute("drive.list", &json!({"folder": folder})).unwrap();
    let mut n: Vec<String> = r["items"].as_array().unwrap().iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
    n.sort();
    n
}

fn blobs_under(dir: &Path) -> usize {
    let blobs = dir.join("lc/blobs");
    std::fs::read_dir(&blobs).map(|rd| rd.flatten().map(|e| std::fs::read_dir(e.path()).map(|r| r.count()).unwrap_or(0)).sum()).unwrap_or(0)
}

#[test]
fn files_and_folders_round_trip_through_the_accounts() {
    let (mut s, lib, acc1, acc2) = drive("roundtrip");
    let src = temp_dir("roundtrip-src");
    std::fs::write(src.join("report.pdf"), vec![7u8; 30_000]).unwrap();
    std::fs::write(src.join("notes.txt"), b"remember the milk").unwrap();

    let taxes = s.execute("drive.mkdir", &json!({"name": "Taxes"})).unwrap();
    let taxes_id = taxes["id"].as_str().unwrap().to_string();
    assert_eq!(taxes["kind"], "folder");
    let up = s.execute("drive.upload", &json!({"path": src.join("report.pdf").to_string_lossy(), "parent": taxes_id, "copies": 2})).unwrap();
    let report = up["id"].as_str().unwrap().to_string();
    // the same name twice in one folder: Drive-style "(1)"
    let up2 = s.execute("drive.upload", &json!({"path": src.join("report.pdf").to_string_lossy(), "parent": taxes_id})).unwrap();
    assert_eq!(up2["name"], "report (1).pdf");
    s.execute("drive.upload", &json!({"path": src.join("notes.txt").to_string_lossy()})).unwrap();
    let t = settle(&mut s);
    assert!(t["tasks"].as_array().unwrap().iter().filter(|x| x["kind"] == "upload").all(|x| x["state"] == "done"), "{t}");
    let first = t["tasks"].as_array().unwrap().iter().find(|x| x["entry"] == report.as_str()).unwrap().clone();
    // extra copies go to other providers only, and both accounts are folders
    assert_eq!(first["copies"], 1, "{first}");

    assert_eq!(names(&mut s, "root"), ["Taxes", "notes.txt"]);
    assert_eq!(names(&mut s, &taxes_id), ["report (1).pdf", "report.pdf"]);
    let listed = s.execute("drive.list", &json!({"folder": "root"})).unwrap();
    let folder = listed["items"].as_array().unwrap().iter().find(|i| i["name"] == "Taxes").unwrap().clone();
    assert_eq!(folder["items"], 2);
    let crumbs = s.execute("drive.list", &json!({"folder": taxes_id})).unwrap()["path"].clone();
    assert_eq!(crumbs, json!([{"id": "root", "name": "My Drive"}, {"id": taxes_id, "name": "Taxes"}]));

    // identical content is one blob per account (the copy of report.pdf shares it, wherever
    // placement put each); the drive metadata is on both
    let blobs = blobs_under(&acc1) + blobs_under(&acc2);
    assert!((2 + 2..=2 + 3).contains(&blobs), "{blobs}");

    // rename, move, star, search, recent
    s.execute("drive.rename", &json!({"id": report, "name": "2025 return.pdf"})).unwrap();
    let notes = s.execute("drive.list", &json!({"view": "search", "query": "NOTES"})).unwrap()["items"][0]["id"].as_str().unwrap().to_string();
    s.execute("drive.move", &json!({"ids": [notes], "parent": taxes_id})).unwrap();
    assert!(s.execute("drive.move", &json!({"ids": [taxes_id], "parent": taxes_id})).is_err(), "not into itself");
    s.execute("drive.star", &json!({"ids": [notes]})).unwrap();
    assert_eq!(s.execute("drive.list", &json!({"view": "starred"})).unwrap()["items"][0]["name"], "notes.txt");
    assert_eq!(s.execute("drive.list", &json!({"view": "recent"})).unwrap()["items"].as_array().unwrap().len(), 3);

    // download decrypts it from an account
    let d = s.execute("drive.download", &json!({"id": report})).unwrap();
    settle(&mut s);
    let f = s.execute("drive.file", &json!({"task": d["task"]})).unwrap();
    assert_eq!(std::fs::read(f["path"].as_str().unwrap()).unwrap(), vec![7u8; 30_000]);
    assert!(Path::new(f["path"].as_str().unwrap()).starts_with(lib.join("cloud/outbox")));

    // the trash: a trashed folder hides what's in it; restoring brings it back
    s.execute("drive.trash", &json!({"ids": [taxes_id]})).unwrap();
    assert_eq!(names(&mut s, "root"), Vec::<String>::new());
    assert_eq!(s.execute("drive.list", &json!({"view": "trash"})).unwrap()["items"].as_array().unwrap().len(), 1);
    assert!(s.execute("drive.list", &json!({"view": "search", "query": "notes"})).unwrap()["items"].as_array().unwrap().is_empty());
    s.execute("drive.restore", &json!({"ids": [taxes_id]})).unwrap();
    assert_eq!(names(&mut s, &taxes_id).len(), 3);

    // deleting forever removes the stored copies (the shared blob goes with its last owner)
    s.execute("drive.trash", &json!({"ids": [taxes_id]})).unwrap();
    let r = s.execute("drive.emptyTrash", &json!({})).unwrap();
    assert_eq!(r["deleted"], 4);
    settle(&mut s);
    assert_eq!(blobs_under(&acc1) + blobs_under(&acc2), 2, "only the drive metadata is left");
    assert_eq!(names(&mut s, "root"), Vec::<String>::new());

    // drive.wait returns once everything queued is stored
    s.execute("drive.upload", &json!({"path": src.join("notes.txt").to_string_lossy()})).unwrap();
    let t = s.execute("drive.wait", &json!({})).unwrap();
    assert_eq!((t["busy"].as_bool(), t["unsynced"].as_bool()), (Some(false), Some(false)), "{t}");
    assert_eq!(names(&mut s, "root"), ["notes.txt"]);

    // a staged upload must come from the inbox
    assert!(s.execute("drive.upload", &json!({"path": src.join("notes.txt").to_string_lossy(), "staged": true})).is_err());
    assert!(src.join("notes.txt").is_file(), "never removes a file outside the inbox");
}

#[test]
fn the_library_backup_shows_read_only_and_leaves_drive_files_alone() {
    let (mut s, lib, _acc1, _acc2) = drive("library");
    let photos = temp_dir("library-photos");
    let (w, h) = (16usize, 12usize);
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data: vec![[90, 120, 200, 255]; w * h] };
    let png = lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap();
    std::fs::write(photos.join("beach.png"), &png).unwrap();
    s.execute("library.import", &json!({"paths": [photos.to_string_lossy()]})).unwrap();

    let doc = photos.join("plan.txt");
    std::fs::write(&doc, b"the plan").unwrap();
    s.execute("drive.upload", &json!({"path": doc.to_string_lossy()})).unwrap();
    settle(&mut s);
    let r = s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(r["failed"], 0, "{r}");
    // a second backup (nothing left in the library to keep the drive file) must not trash it
    s.execute("cloud.backup", &json!({"wait": true})).unwrap();
    assert_eq!(names(&mut s, "root"), ["plan.txt"]);

    let lib_root = s.execute("drive.list", &json!({"folder": "lib"})).unwrap();
    let parts: Vec<&str> = lib_root["items"].as_array().unwrap().iter().map(|i| i["id"].as_str().unwrap()).collect();
    assert_eq!(parts, ["lib.originals", "lib.previews", "lib.catalog"]);
    let dirs = s.execute("drive.list", &json!({"folder": "lib.originals"})).unwrap();
    let dir = dirs["items"][0].clone();
    assert_eq!(dir["readOnly"], true);
    let files = s.execute("drive.list", &json!({"folder": dir["id"]})).unwrap();
    let beach = files["items"][0].clone();
    assert_eq!((beach["name"].as_str(), beach["readOnly"].as_bool()), (Some("beach.png"), Some(true)));
    assert_eq!(files["path"][2]["name"], dir["name"]);
    assert!(s.execute("drive.rename", &json!({"id": beach["id"], "name": "x.png"})).is_err(), "managed by the backup");
    assert!(s.execute("drive.trash", &json!({"ids": [beach["id"]]})).is_err());
    assert!(s.execute("drive.upload", &json!({"path": doc.to_string_lossy(), "parent": "lib.originals"})).is_err());
    // a library original downloads like any file
    let d = s.execute("drive.download", &json!({"id": beach["id"]})).unwrap();
    settle(&mut s);
    let f = s.execute("drive.file", &json!({"task": d["task"]})).unwrap();
    assert_eq!(std::fs::read(f["path"].as_str().unwrap()).unwrap(), png);

    let st = s.execute("drive.status", &json!({})).unwrap();
    assert_eq!((st["files"].as_u64(), st["lightcraft"]["photos"].as_u64()), (Some(1), Some(1)), "{st}");
    assert!(st["usage"]["stored"].as_u64().unwrap() > 0);
    assert_eq!(st["accounts"].as_array().unwrap().len(), 2);
    assert_eq!(st["synced"], true);

    // after a restart (lock + unlock) the drive is read back from the library
    s.execute("cloud.lock", &json!({})).unwrap();
    assert!(s.execute("drive.list", &json!({})).is_err(), "locked");
    s.execute("cloud.unlock", &json!({"passphrase": PASS})).unwrap();
    assert_eq!(names(&mut s, "root"), ["plan.txt"]);
    drop(lib);
}

#[test]
fn the_drive_comes_back_from_the_accounts_alone() {
    let (mut s, lib, acc1, _acc2) = drive("pull");
    let src = temp_dir("pull-src");
    std::fs::write(src.join("a.txt"), b"alpha").unwrap();
    s.execute("drive.mkdir", &json!({"name": "Docs"})).unwrap();
    s.execute("drive.upload", &json!({"path": src.join("a.txt").to_string_lossy()})).unwrap();
    settle(&mut s);
    // this device loses its copy of the manifest: the accounts still have the drive
    std::fs::remove_file(lib.join("cloud/manifest.enc")).unwrap();
    std::fs::remove_file(lib.join("cloud/drive.json")).unwrap();
    s.execute("cloud.lock", &json!({})).unwrap();
    s.execute("cloud.unlock", &json!({"passphrase": PASS})).unwrap();
    s.execute("drive.list", &json!({})).unwrap(); // loads and starts reading the accounts
    settle(&mut s);
    assert_eq!(names(&mut s, "root"), ["Docs", "a.txt"]);
    assert!(acc1.join("lc/vault.enc").exists() || acc1.join("lc/manifest").is_dir());
}

#[test]
fn the_session_and_the_worker_can_save_the_same_file_at_once() {
    // both used one temp name per process, so one of two simultaneous saves failed
    let dir = temp_dir("atomic");
    let path = dir.join("manifest.enc");
    let threads: Vec<_> = (0..8u8)
        .map(|i| {
            let path = path.clone();
            std::thread::spawn(move || (0..50).map(|_| crate::cloud::write_atomic(&path, &[i; 4096])).collect::<Result<Vec<_>, _>>())
        })
        .collect();
    for t in threads {
        assert!(t.join().unwrap().is_ok());
    }
    assert_eq!(std::fs::read(&path).unwrap().len(), 4096);
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temp files left behind");
}
