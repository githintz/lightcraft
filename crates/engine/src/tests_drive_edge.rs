//! The drive's edge cases (`docs/cloud-test-plan.md`, B and A9–A13): names, uploads that can't
//! be stored or lose their folder or file, folder trees with odd entries, moves that can't be,
//! the trash in every order, shared content, downloads of things that went away, views, two
//! devices on the same accounts, damaged files, a backup while the drive works, OpenDrive shared
//! by the drive and a backup, and Unicode paths.

#![cfg(not(target_arch = "wasm32"))]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Session;

const PASS: &str = "correct horse battery";

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-drive-edge-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Drive {
    s: Session,
    lib: PathBuf,
    accounts: Vec<PathBuf>,
    files: PathBuf,
}

impl Drop for Drive {
    fn drop(&mut self) {
        for d in [&self.lib, &self.files].into_iter().chain(self.accounts.iter()) {
            let _ = std::fs::remove_dir_all(d);
        }
    }
}

/// A library with the drive set up and `n` folder accounts with room.
fn drive(tag: &str, n: usize) -> Drive {
    let lib = temp_dir(&format!("{tag}-lib"));
    let files = temp_dir(&format!("{tag}-files"));
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("cloud.setup", &json!({"passphrase": PASS})).unwrap();
    let mut accounts = Vec::new();
    for i in 0..n {
        let dir = temp_dir(&format!("{tag}-acc{i}"));
        s.execute(
            "cloud.addAccount",
            &json!({"provider": "folder", "label": format!("Disk {i}"), "config": {"path": dir.join("lc").to_string_lossy()}, "cap": 500_000_000u64}),
        )
        .unwrap();
        accounts.push(dir);
    }
    Drive { s, lib, accounts, files }
}

impl Drive {
    fn run(&mut self, cmd: &str, p: Value) -> Value {
        self.s.execute(cmd, &p).unwrap_or_else(|e| panic!("{cmd} {p}: {e}"))
    }

    fn fails(&mut self, cmd: &str, p: Value) -> String {
        match self.s.execute(cmd, &p) {
            Ok(v) => panic!("{cmd} {p} should fail, gave {v}"),
            Err(e) => e.to_string(),
        }
    }

    /// Wait until every transfer is done and saved.
    fn settle(&mut self) -> Value {
        let start = Instant::now();
        loop {
            let t = self.run("drive.transfers", json!({}));
            if t["busy"] == false && t["unsynced"] == false {
                return t;
            }
            assert!(start.elapsed() < Duration::from_secs(60), "the drive never settled: {t}");
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn file(&self, name: &str, data: &[u8]) -> PathBuf {
        let p = self.files.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, data).unwrap();
        p
    }

    /// Upload `data` as `name` into `parent` and wait until it is stored (an upload is an item
    /// once stored); its item id.
    fn upload(&mut self, name: &str, data: &[u8], parent: Option<&str>) -> String {
        let id = self.queue_upload(name, data, parent);
        self.run("drive.wait", json!({}));
        id
    }

    /// Queue an upload without waiting; the id its item will have.
    fn queue_upload(&mut self, name: &str, data: &[u8], parent: Option<&str>) -> String {
        let path = self.file(&format!("src-{}-{name}", self.files.read_dir().map(|r| r.count()).unwrap_or(0)), data);
        let mut p = json!({"path": path.to_string_lossy(), "name": name});
        if let Some(parent) = parent {
            p["parent"] = json!(parent);
        }
        let r = self.run("drive.upload", p);
        r["id"].as_str().unwrap().to_string()
    }

    fn items(&mut self, p: Value) -> Vec<Value> {
        self.run("drive.list", p)["items"].as_array().unwrap().clone()
    }

    fn names(&mut self, folder: &str) -> Vec<String> {
        let mut n: Vec<String> = self.items(json!({"folder": folder})).iter().map(|i| i["name"].as_str().unwrap().to_string()).collect();
        n.sort();
        n
    }

    fn task(&mut self, id: &str) -> Value {
        let t = self.run("drive.transfers", json!({}));
        t["tasks"].as_array().unwrap().iter().find(|x| x["id"] == id).cloned().unwrap_or(Value::Null)
    }

    fn mkdir(&mut self, name: &str, parent: Option<&str>) -> String {
        let mut p = json!({"name": name});
        if let Some(parent) = parent {
            p["parent"] = json!(parent);
        }
        self.run("drive.mkdir", p)["id"].as_str().unwrap().to_string()
    }
}

fn blobs(dir: &Path) -> usize {
    std::fs::read_dir(dir).map(|rd| rd.flatten().map(|e| if e.path().is_dir() { blobs(&e.path()) } else { 1 }).sum()).unwrap_or(0)
}

#[test]
fn gates() {
    // no library on disk
    let mut s = Session::new();
    assert_eq!(s.execute("drive.status", &json!({})).unwrap()["available"], false);
    for (cmd, p) in
        [("drive.list", json!({})), ("drive.mkdir", json!({"name": "x"})), ("drive.upload", json!({"path": "/etc/hosts"})), ("drive.sync", json!({}))]
    {
        assert!(s.execute(cmd, &p).is_err(), "{cmd}");
    }
    // not set up, then locked
    let lib = temp_dir("gates");
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    let st = s.execute("drive.status", &json!({})).unwrap();
    assert_eq!((st["configured"].as_bool(), st["unlocked"].as_bool()), (Some(false), Some(false)));
    assert!(s.execute("drive.list", &json!({})).unwrap_err().to_string().contains("locked"));
    s.execute("cloud.setup", &json!({"passphrase": PASS})).unwrap();
    assert!(s.execute("drive.list", &json!({})).is_ok(), "no accounts yet is fine");
    assert!(s.execute("drive.upload", &json!({"path": lib.join("x").to_string_lossy()})).unwrap_err().to_string().contains("storage account"));
    s.execute("cloud.lock", &json!({})).unwrap();
    for cmd in ["drive.list", "drive.transfers", "drive.emptyTrash", "drive.wait"] {
        assert!(s.execute(cmd, &json!({})).is_err(), "{cmd} while locked");
    }
    assert_eq!(s.execute("drive.status", &json!({})).unwrap()["unlocked"], false);
    let _ = std::fs::remove_dir_all(lib);
}

#[test]
fn names() {
    let mut d = drive("names", 1);
    for bad in ["", "   ", ".", "..", "\u{7}\u{0}"] {
        assert!(d.s.execute("drive.mkdir", &json!({"name": bad})).is_err(), "{bad:?}");
    }
    let made = d.run("drive.mkdir", json!({"name": "a/b\\c"}));
    assert_eq!(made["name"], "a-b-c");
    let long = "é".repeat(300);
    let made = d.run("drive.mkdir", json!({"name": long}));
    let name = made["name"].as_str().unwrap();
    assert!(name.len() <= 255 && name.chars().all(|c| c == 'é'), "cut at a character boundary: {} bytes", name.len());
    d.mkdir("📷 Ísland — 2019", None);
    d.mkdir("Résumé", None);
    let again = d.run("drive.mkdir", json!({"name": "résumé"}));
    assert_eq!(again["name"], "résumé (1)", "names clash ignoring case, as in Drive");
    let reused = d.run("drive.mkdir", json!({"name": "Résumé", "reuse": true}));
    assert_eq!(reused["name"], "Résumé");
    // where folders can't go
    let trashed = d.mkdir("Old", None);
    d.run("drive.trash", json!({"ids": [trashed]}));
    assert!(d.fails("drive.mkdir", json!({"name": "x", "parent": trashed})).contains("trash"));
    assert!(d.fails("drive.mkdir", json!({"name": "x", "parent": "lib"})).contains("Library"));
    assert!(d.fails("drive.mkdir", json!({"name": "x", "parent": "lib.originals"})).contains("Library"));
    assert!(d.fails("drive.mkdir", json!({"name": "x", "parent": "drive.0000000000000000"})).contains("no folder"));
    let file = d.upload("notes.txt", b"hi", None);
    assert!(d.fails("drive.mkdir", json!({"name": "x", "parent": file})).contains("no folder"));
    assert!(d.fails("drive.list", json!({"folder": file})).contains("no folder"));
    assert!(d.fails("drive.list", json!({"view": "everything"})).contains("unknown view"));
    d.settle();
}

#[test]
fn uploads() {
    let mut d = drive("uploads", 2);
    let empty = d.upload("empty.bin", b"", None);
    let a = d.upload("a.txt", b"same bytes", None);
    let b = d.upload("b.txt", b"same bytes", None);
    let a2 = d.upload("a.txt", b"other bytes", None);
    d.settle();
    let items = d.items(json!({}));
    let get = |id: &str| items.iter().find(|i| i["id"] == id).cloned().unwrap();
    assert_eq!((get(&empty)["size"].as_u64(), get(&empty)["copies"].as_u64()), (Some(0), Some(1)), "an empty file is stored too");
    assert_eq!(get(&a2)["name"], "a (1).txt");
    assert_eq!(get(&a)["copies"], 1);
    // the same content is stored once
    let total: usize = d.accounts.iter().map(|a| blobs(&a.join("lc/blobs"))).sum();
    let content = ["", "same bytes", "other bytes"].len();
    assert_eq!(total, content + 2, "three contents once each, and the drive's metadata on both accounts");
    // the vault and each account's marker are on every account: the drive alone can be recovered
    for acc in &d.accounts {
        assert!(acc.join("lc/vault.enc").is_file() && acc.join("lc/account.json").is_file(), "{}", acc.display());
    }
    assert_eq!(get(&b)["copies"], 1);

    // a big upload keeps the worker busy; one queued behind it can be cancelled
    let big = d.file("big.bin", &vec![7u8; 8_000_000]);
    let small = d.file("small.txt", b"small");
    d.run("drive.upload", json!({"path": big.to_string_lossy()}));
    let queued = d.run("drive.upload", json!({"path": small.to_string_lossy()}));
    let task = queued["task"].as_str().unwrap().to_string();
    let cancelled = d.run("drive.cancel", json!({"task": task}))["cancelled"] == true;
    d.settle();
    let t = d.task(&task);
    let listed = d.names("root").contains(&"small.txt".to_string());
    if cancelled {
        assert_eq!((t["state"].as_str(), listed), (Some("cancelled"), false), "{t}");
    } else {
        assert_eq!((t["state"].as_str(), listed), (Some("done"), true), "{t}");
    }
    assert_eq!(d.run("drive.cancel", json!({"task": "t-nope"}))["cancelled"], false);
    d.run("drive.clearTransfers", json!({}));
    assert!(d.run("drive.transfers", json!({}))["tasks"].as_array().unwrap().is_empty());
    // a staged upload must come from the inbox
    let outside = d.file("outside.txt", b"x");
    assert!(d.fails("drive.upload", json!({"path": outside.to_string_lossy(), "staged": true})).contains("inbox"));
    assert!(outside.exists(), "and isn't removed");
    assert!(d.fails("drive.upload", json!({"path": d.files.join("missing.txt").to_string_lossy()})).contains("not a file"));
    assert!(d.fails("drive.upload", json!({"path": outside.to_string_lossy(), "copies": 0})).contains("1–8"));
    assert!(d.fails("drive.upload", json!({"path": outside.to_string_lossy(), "level": "warp"})).contains("level"));
}

#[test]
fn uploads_that_cannot_be_stored() {
    let mut d = drive("nostore", 2);
    // no room: tiny limits on both accounts
    let ids: Vec<String> =
        d.run("cloud.status", json!({}))["accounts"].as_array().unwrap().iter().map(|a| a["id"].as_str().unwrap().to_string()).collect();
    for id in &ids {
        d.run("cloud.setAccount", json!({"id": id, "cap": 1000}));
    }
    let big = d.file("big.bin", &vec![1u8; 50_000]);
    let t = d.run("drive.upload", json!({"path": big.to_string_lossy()}))["task"].as_str().unwrap().to_string();
    d.settle();
    let task = d.task(&t);
    assert_eq!(task["state"], "failed", "{task}");
    assert!(task["error"].as_str().unwrap().contains("no room"), "{task}");
    assert!(d.names("root").is_empty(), "a file that couldn't be stored isn't listed");
    // room again; more copies than accounts: as many as there are
    for id in &ids {
        d.run("cloud.setAccount", json!({"id": id, "cap": 500_000_000u64}));
    }
    let two = d.file("two.txt", b"two copies please");
    let r = d.run("drive.upload", json!({"path": two.to_string_lossy(), "copies": 5}));
    d.settle();
    let task = d.task(r["task"].as_str().unwrap());
    assert_eq!((task["state"].as_str(), task["copies"].as_u64()), (Some("done"), Some(2)), "{task}");
    // one account unplugged: uploads go to the other
    std::fs::remove_dir_all(&d.accounts[0]).unwrap();
    let id = d.upload("after.txt", b"after the disk went", None);
    d.settle();
    let item = d.items(json!({})).into_iter().find(|i| i["id"] == id).unwrap();
    assert_eq!(item["copies"], 1, "{item}");
    let st = d.run("drive.status", json!({}));
    assert!(st["accounts"].as_array().unwrap().iter().any(|a| !a["error"].is_null()), "the unplugged disk is reported: {st}");
}

#[test]
fn folder_uploads() {
    let mut d = drive("tree", 1);
    let root = d.files.join("Trip");
    d.file("Trip/a.jpg", b"a");
    d.file("Trip/Day 1/b.jpg", b"b");
    d.file("Trip/.DS_Store", b"junk");
    d.file("Trip/.hidden/c.jpg", b"c");
    std::fs::create_dir_all(root.join("Empty")).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink("/etc/hosts", root.join("link")).unwrap();
    // a file over the size limit (sparse: takes no space)
    let huge = std::fs::File::create(root.join("huge.mov")).unwrap();
    huge.set_len(crate::cloud::MAX_FILE + 1).unwrap();
    drop(huge);
    let r = d.run("drive.upload", json!({"path": root.to_string_lossy()}));
    let skipped = r["skipped"].as_array().cloned().unwrap_or_default();
    assert_eq!(skipped.len(), 1, "the oversized file is reported, the rest goes on: {r}");
    assert!(skipped[0]["name"].as_str().unwrap().contains("huge.mov"), "{r}");
    d.settle();
    let top = r["folder"].as_str().unwrap().to_string();
    assert_eq!(d.names(&top), ["Day 1", "Empty", "a.jpg"], "hidden files, links and the oversized file are left out");
    let day = d.items(json!({"folder": top})).into_iter().find(|i| i["name"] == "Day 1").unwrap();
    assert_eq!(d.names(day["id"].as_str().unwrap()), ["b.jpg"]);
    // the folders made are saved, not only the files
    let saved = std::fs::read(d.lib.join("cloud/drive.json")).unwrap();
    assert!(String::from_utf8_lossy(&saved).contains(&top));
    // uploading the same folder again makes "Trip (1)"
    std::fs::remove_file(root.join("huge.mov")).unwrap();
    let again = d.run("drive.upload", json!({"path": root.to_string_lossy()}));
    d.settle();
    assert!(d.names("root").contains(&"Trip (1)".to_string()), "{again}");
}

#[test]
fn uploads_whose_folder_or_file_goes() {
    let mut d = drive("goes", 1);
    let busy = d.file("busy.bin", &vec![3u8; 20_000_000]);
    let folder = d.mkdir("Inbox", None);
    let gone_folder = d.mkdir("Gone", None);
    // the worker is busy with a big file; meanwhile the targets go away
    d.run("drive.upload", json!({"path": busy.to_string_lossy()}));
    let into_trashed = d.queue_upload("into-trashed.txt", b"1", Some(&folder));
    d.queue_upload("into-deleted.txt", b"2", Some(&gone_folder));
    let vanishing = d.file("vanishing.txt", b"3");
    let t = d.run("drive.upload", json!({"path": vanishing.to_string_lossy()}))["task"].as_str().unwrap().to_string();
    d.run("drive.trash", json!({"ids": [folder]}));
    d.run("drive.delete", json!({"ids": [gone_folder]}));
    std::fs::remove_file(&vanishing).unwrap();
    d.settle();
    let task = d.task(&t);
    assert_eq!(task["state"], "failed", "{task}");
    assert!(task["error"].as_str().unwrap().contains("read"), "{task}");
    // nothing is lost: a file whose folder went lands at the top
    let top = d.names("root");
    assert!(top.contains(&"into-deleted.txt".to_string()), "{top:?}");
    let all = d.items(json!({"view": "search", "query": "into-"}));
    let trashed_one = d.items(json!({"view": "trash"}));
    let found = all.iter().chain(trashed_one.iter()).any(|i| i["id"] == into_trashed) || top.contains(&"into-trashed.txt".to_string());
    assert!(found, "the file whose folder was trashed is somewhere the user can find it");
}

#[test]
fn rename_move_star() {
    let mut d = drive("rms", 1);
    let a = d.upload("a.txt", b"a", None);
    d.upload("b.txt", b"b", None);
    assert_eq!(d.run("drive.rename", json!({"id": a, "name": "B.TXT"}))["name"], "B (1).TXT");
    assert_eq!(d.run("drive.rename", json!({"id": a, "name": "B (1).TXT"}))["name"], "B (1).TXT", "renaming to its own name is no clash");
    assert!(d.fails("drive.rename", json!({"id": "original.1", "name": "x"})).contains("Library"));
    assert!(d.fails("drive.rename", json!({"id": "drive.ffffffffffffffff", "name": "x"})).contains("no item"));
    assert_eq!(d.run("drive.rename", json!({"id": a, "name": " / "}))["name"], "-", "a separator alone becomes a dash");
    assert!(d.fails("drive.rename", json!({"id": a, "name": " \u{7} "})).contains("empty"));

    let top = d.mkdir("Top", None);
    let mid = d.mkdir("Mid", Some(&top));
    let low = d.mkdir("Low", Some(&mid));
    assert!(d.fails("drive.move", json!({"ids": [top], "parent": top})).contains("itself"));
    assert!(d.fails("drive.move", json!({"ids": [top], "parent": low})).contains("itself"), "not into a descendant either");
    assert!(d.fails("drive.move", json!({"ids": [a], "parent": "lib"})).contains("Library"));
    let bin = d.mkdir("Bin", None);
    d.run("drive.trash", json!({"ids": [bin]}));
    assert!(d.fails("drive.move", json!({"ids": [a], "parent": bin})).contains("trash"));
    // two files of one name from different folders into one folder
    let x1 = d.upload("x.txt", b"1", Some(&mid));
    let x2 = d.upload("x.txt", b"2", Some(&low));
    assert_eq!(d.run("drive.move", json!({"ids": [x1, x2], "parent": "root"}))["moved"], 2);
    let names = d.names("root");
    assert!(names.contains(&"x.txt".to_string()) && names.contains(&"x (1).txt".to_string()), "{names:?}");
    assert_eq!(d.run("drive.move", json!({"ids": [x1], "parent": "root"}))["moved"], 0, "already there");

    d.run("drive.star", json!({"ids": [a, top]}));
    let starred: Vec<String> = d.items(json!({"view": "starred"})).iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(starred.len(), 2);
    d.run("drive.star", json!({"ids": [top], "starred": false}));
    assert_eq!(d.items(json!({"view": "starred"})).len(), 1);
    assert!(d.fails("drive.star", json!({"ids": []})).contains("empty"));
    d.settle();
}

#[test]
fn trash_and_restore() {
    let mut d = drive("trash", 1);
    let folder = d.mkdir("Holiday", None);
    let inner = d.mkdir("Beach", Some(&folder));
    let child = d.upload("sun.jpg", b"sun", Some(&inner));
    d.run("drive.star", json!({"ids": [child]}));
    d.run("drive.trash", json!({"ids": [folder]}));
    // hidden everywhere while its folder is in the trash
    assert!(d.names("root").is_empty());
    assert!(d.items(json!({"view": "search", "query": "sun"})).is_empty());
    assert!(d.items(json!({"view": "recent"})).is_empty());
    assert!(d.items(json!({"view": "starred"})).is_empty());
    let trash = d.items(json!({"view": "trash"}));
    assert_eq!(trash.len(), 1, "only the folder shows in the trash: {trash:?}");
    let st = d.run("drive.status", json!({}));
    assert_eq!((st["files"].as_u64(), st["trash"]["files"].as_u64()), (Some(0), Some(1)), "{st}");
    // restoring the folder brings back what's in it
    d.run("drive.restore", json!({"ids": [folder]}));
    assert_eq!(d.names(&inner), ["sun.jpg"]);
    // a file trashed, then its folder: restoring only the file (the API allows it) puts it at the top
    d.run("drive.trash", json!({"ids": [child]}));
    d.run("drive.trash", json!({"ids": [folder]}));
    d.run("drive.restore", json!({"ids": [child]}));
    assert_eq!(d.names("root"), ["sun.jpg"]);
    d.run("drive.restore", json!({"ids": [folder]}));
    assert!(d.names(&inner).is_empty());

    // a child trashed, then its folder: restoring both puts both back where they were, in any order
    for order in [[0, 1], [1, 0]] {
        let f = d.mkdir(&format!("F{}", order[0]), None);
        let c = d.upload("c.txt", b"c", Some(&f));
        d.run("drive.trash", json!({"ids": [c]}));
        d.run("drive.trash", json!({"ids": [f]}));
        let both = [&c, &f];
        d.run("drive.restore", json!({"ids": [both[order[0]], both[order[1]]]}));
        assert_eq!(d.names(&f), ["c.txt"], "restored in order {order:?}");
    }

    // empty the trash with nested trashed items
    let n = d.mkdir("Nest", None);
    let n1 = d.upload("n1.txt", b"n1", Some(&n));
    d.run("drive.trash", json!({"ids": [n1]}));
    d.run("drive.trash", json!({"ids": [n]}));
    let deleted = d.run("drive.emptyTrash", json!({}))["deleted"].as_u64().unwrap();
    assert_eq!(deleted, 2);
    assert!(d.items(json!({"view": "trash"})).is_empty());
    assert_eq!(d.run("drive.emptyTrash", json!({}))["deleted"], 0);
    d.settle();
}

#[test]
fn deleting_shared_content() {
    let mut d = drive("shared", 1);
    let a = d.upload("one.txt", b"identical content", None);
    let b = d.upload("two.txt", b"identical content", None);
    d.settle();
    let before = blobs(&d.accounts[0].join("lc/blobs"));
    d.run("drive.delete", json!({"ids": [a]}));
    d.settle();
    assert_eq!(blobs(&d.accounts[0].join("lc/blobs")), before, "the other file still uses the stored copy");
    let t = d.run("drive.download", json!({"id": b}))["task"].as_str().unwrap().to_string();
    d.settle();
    let f = d.run("drive.file", json!({"task": t}));
    assert_eq!(std::fs::read(f["path"].as_str().unwrap()).unwrap(), b"identical content");
    d.run("drive.delete", json!({"ids": [b]}));
    d.settle();
    assert_eq!(blobs(&d.accounts[0].join("lc/blobs")), before - 1, "the last user gone: the copy is deleted");
}

#[test]
fn downloads() {
    let mut d = drive("dl", 2);
    let data: Vec<u8> = (0..300_000u32).map(|i| (i * 7 % 251) as u8).collect();
    let id = d.upload("photo: \"x\" ?.raw", &data, None);
    let gone = d.upload("gone.txt", b"gone", None);
    d.settle();
    d.run("drive.trash", json!({"ids": [id]}));
    let t = d.run("drive.download", json!({"id": id}))["task"].as_str().unwrap().to_string();
    d.settle();
    let f = d.run("drive.file", json!({"task": t}));
    assert_eq!(std::fs::read(f["path"].as_str().unwrap()).unwrap(), data, "a trashed file can still be downloaded");
    assert_eq!(f["name"], "photo: \"x\" ?.raw");
    let file_name = Path::new(f["path"].as_str().unwrap()).file_name().unwrap().to_string_lossy().to_string();
    assert!(!file_name.contains([':', '"', '?', '*', '<', '>', '|']), "the outbox file name works on every system: {file_name}");
    // deleted before the worker gets to it
    let busy = d.file("busy.bin", &vec![9u8; 20_000_000]);
    d.run("drive.upload", json!({"path": busy.to_string_lossy()}));
    let t2 = d.run("drive.download", json!({"id": gone}))["task"].as_str().unwrap().to_string();
    // (the busy upload goes first, so the file is deleted before its download runs)
    d.run("drive.delete", json!({"ids": [gone]}));
    d.settle();
    let task = d.task(&t2);
    assert!(task["state"] == "failed" || task["state"] == "done", "{task}");
    assert!(d.fails("drive.download", json!({"id": "drive.0123456789abcdef"})).contains("no file"));
    let folder = d.mkdir("F", None);
    assert!(d.fails("drive.download", json!({"id": folder})).contains("only files"));
    assert!(d.fails("drive.file", json!({"task": "t-0000"})).contains("no transfer"));
}

#[test]
fn views() {
    let mut d = drive("views", 1);
    let a = d.mkdir("Alpha", None);
    let b = d.mkdir("Beta", Some(&a));
    let c = d.mkdir("Gamma", Some(&b));
    let f1 = d.upload("Report FINAL.pdf", b"1", Some(&c));
    std::thread::sleep(Duration::from_millis(5));
    let f2 = d.upload("notes.txt", b"2", None);
    d.settle();
    let path = d.run("drive.list", json!({"folder": c}))["path"].clone();
    let crumbs: Vec<&str> = path.as_array().unwrap().iter().map(|p| p["name"].as_str().unwrap()).collect();
    assert_eq!(crumbs, ["My Drive", "Alpha", "Beta", "Gamma"]);
    let found = d.items(json!({"view": "search", "query": "report final"}));
    assert_eq!(found.len(), 1);
    assert_eq!(found[0]["id"], f1.as_str());
    assert!(d.items(json!({"view": "search", "query": "   "})).is_empty());
    assert!(d.items(json!({"view": "search"})).is_empty());
    let recent: Vec<String> = d.items(json!({"view": "recent"})).iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(recent, [f2.clone(), f1.clone()], "newest first, files only");
    d.run("drive.rename", json!({"id": f1, "name": "Report v2.pdf"}));
    let recent: Vec<String> = d.items(json!({"view": "recent"})).iter().map(|i| i["id"].as_str().unwrap().to_string()).collect();
    assert_eq!(recent[0], f1, "a change makes it recent");
    let lib = d.run("drive.list", json!({"folder": "lib"}));
    assert_eq!(lib["items"].as_array().unwrap().len(), 3);
    assert_eq!(lib["items"][0]["readOnly"], true);
    let catalog = d.items(json!({"folder": "lib.catalog"}));
    assert!(catalog.is_empty(), "nothing backed up yet");
    let folder_counts = d.items(json!({}));
    assert_eq!(folder_counts.iter().find(|i| i["id"] == a).unwrap()["items"], 1);
}

#[test]
fn damaged_files() {
    let mut d = drive("damaged", 1);
    d.upload("keep.txt", b"keep", None);
    d.settle();
    let lib = d.lib.clone();
    drop(std::mem::take(&mut d.s));
    std::fs::write(lib.join("cloud/drive.json"), b"{not json").unwrap();
    std::fs::write(lib.join("cloud/manifest.enc"), b"garbage that isn't sealed").unwrap();
    std::fs::remove_dir_all(lib.join("cloud/outbox")).ok();
    let mut s = Session::new().with_fs();
    s.open_library(&lib, false).unwrap();
    s.execute("cloud.unlock", &json!({"passphrase": PASS})).unwrap();
    d.s = s;
    // the local copy is unreadable: the drive comes back from the accounts
    d.run("drive.list", json!({}));
    d.settle();
    assert_eq!(d.names("root"), ["keep.txt"]);
    let id = d.items(json!({}))[0]["id"].as_str().unwrap().to_string();
    let t = d.run("drive.download", json!({"id": id}))["task"].as_str().unwrap().to_string();
    d.settle();
    assert_eq!(std::fs::read(d.run("drive.file", json!({"task": t}))["path"].as_str().unwrap()).unwrap(), b"keep");
}

#[test]
fn the_same_folder_twice_as_two_accounts() {
    let mut d = drive("twice", 1);
    let same = d.accounts[0].join("lc");
    let e = d.fails("cloud.addAccount", json!({"provider": "folder", "label": "Again", "config": {"path": same.to_string_lossy()}}));
    assert!(e.contains("already"), "{e}");
    // nor a folder inside another account's, nor one an account's folder is in
    let e = d.fails("cloud.addAccount", json!({"provider": "folder", "label": "Inside", "config": {"path": same.join("sub").to_string_lossy()}}));
    assert!(e.contains("already"), "{e}");
    let e = d.fails("cloud.addAccount", json!({"provider": "folder", "label": "Around", "config": {"path": d.accounts[0].to_string_lossy()}}));
    assert!(e.contains("already"), "{e}");
    // nor the library itself
    let e = d.fails("cloud.addAccount", json!({"provider": "folder", "label": "Lib", "config": {"path": d.lib.join("store").to_string_lossy()}}));
    assert!(e.contains("library"), "{e}");
}

#[test]
fn two_devices() {
    let mut a = drive("dev-a", 2);
    let shared = a.mkdir("Shared", None);
    a.upload("from-a.txt", b"a", Some(&shared));
    a.settle();
    // device B: a new library joins the drive from one of its accounts and the passphrase
    let lib_b = temp_dir("dev-b");
    let mut b = Session::new().with_fs();
    b.open_library(&lib_b, false).unwrap();
    let acc = a.accounts[0].join("lc");
    let join = |b: &mut Session, pass: &str| {
        b.execute("cloud.join", &json!({"provider": "folder", "config": {"path": acc.to_string_lossy()}, "passphrase": pass}))
    };
    assert!(join(&mut b, "not the passphrase").is_err());
    let joined = join(&mut b, PASS).unwrap();
    assert_eq!(joined["accounts"].as_array().map(Vec::len), Some(2), "{joined}");
    assert!(join(&mut b, PASS).unwrap_err().to_string().contains("already"), "a library joins once");
    let mut b = Drive { s: b, lib: lib_b, accounts: Vec::new(), files: temp_dir("dev-b-files") };
    b.run("drive.list", json!({}));
    b.settle();
    assert_eq!(b.names("root"), ["Shared"], "B sees A's drive");
    assert_eq!(b.names(&shared), ["from-a.txt"]);
    // B takes A's size limits (a folder account without one would take nothing)
    let st = b.run("drive.status", json!({}));
    let caps: Vec<u64> = st["accounts"].as_array().unwrap().iter().map(|a| a["capacity"].as_u64().unwrap_or(0)).collect();
    assert_eq!(caps, [500_000_000, 500_000_000], "{st}");
    // both change it; each syncs: both see both
    b.upload("from-b.txt", b"b", Some(&shared));
    b.settle();
    a.run("drive.rename", json!({"id": shared, "name": "Shared (renamed on A)"}));
    a.settle();
    a.run("drive.sync", json!({}));
    a.settle();
    b.run("drive.sync", json!({}));
    b.settle();
    for dev in [&mut a, &mut b] {
        assert_eq!(dev.names("root"), ["Shared (renamed on A)"]);
        assert_eq!(dev.names(&shared), ["from-a.txt", "from-b.txt"]);
    }
    // B reads A's file
    let from_a = b.items(json!({"folder": shared})).into_iter().find(|i| i["name"] == "from-a.txt").unwrap();
    let t = b.run("drive.download", json!({"id": from_a["id"]}))["task"].as_str().unwrap().to_string();
    b.settle();
    assert_eq!(std::fs::read(b.run("drive.file", json!({"task": t}))["path"].as_str().unwrap()).unwrap(), b"a");
}

#[test]
fn storage_holding_another_vault_is_refused() {
    let mut a = drive("foreign-a", 1);
    a.upload("x.txt", b"x", None);
    a.settle();
    let acc = a.accounts[0].join("lc");
    // another library, set up on its own with another passphrase
    let lib_c = temp_dir("foreign-c");
    let mut c = Session::new().with_fs();
    c.open_library(&lib_c, false).unwrap();
    c.execute("cloud.setup", &json!({"passphrase": "another passphrase"})).unwrap();
    let e = c
        .execute("cloud.addAccount", &json!({"provider": "folder", "label": "A's", "config": {"path": acc.to_string_lossy()}}))
        .unwrap_err()
        .to_string();
    assert!(e.contains("another LightCraft drive"), "{e}");
    // A removing its account and adding it back is fine
    let id = a.run("cloud.status", json!({}))["accounts"][0]["id"].as_str().unwrap().to_string();
    a.run("cloud.removeAccount", json!({"id": id}));
    a.run("cloud.addAccount", json!({"provider": "folder", "label": "Back", "config": {"path": acc.to_string_lossy()}}));
    drop(c);
    let _ = std::fs::remove_dir_all(lib_c);
}

#[test]
fn a_joined_library_does_not_back_up_over_the_other_one() {
    let mut a = drive("guard-a", 1);
    let photo = a.file("p.png", &png());
    a.run("library.import", json!({"paths": [photo.to_string_lossy()]}));
    assert_eq!(a.run("cloud.backup", json!({"wait": true}))["failed"], 0);
    let lib_b = temp_dir("guard-b");
    let mut b = Session::new().with_fs();
    b.open_library(&lib_b, false).unwrap();
    let acc = a.accounts[0].join("lc");
    b.execute("cloud.join", &json!({"provider": "folder", "config": {"path": acc.to_string_lossy()}, "passphrase": PASS})).unwrap();
    let e = b.execute("cloud.backup", &json!({"wait": true})).unwrap_err().to_string();
    assert!(e.contains("another library's backup"), "{e}");
    // A's backup is intact
    a.run("drive.sync", json!({}));
    a.settle();
    assert_eq!(a.items(json!({"folder": "lib.originals"})).len(), 1);
    drop(b);
    let _ = std::fs::remove_dir_all(lib_b);
}

#[test]
fn backup_and_drive_work_at_once() {
    let mut d = drive("both", 2);
    // a photo in the library for the backup
    let photo = d.file("p.png", &crate::tests_drive_edge::png());
    d.run("library.import", json!({"paths": [photo.to_string_lossy()]}));
    for i in 0..10 {
        d.upload(&format!("doc{i}.txt"), format!("doc {i}").as_bytes(), None);
    }
    let r = d.run("cloud.backup", json!({"wait": true}));
    assert_eq!(r["failed"], 0, "{r}");
    d.settle();
    assert_eq!(d.names("root").len(), 10, "drive files survive the backup");
    let originals = d.items(json!({"folder": "lib.originals"}));
    assert_eq!(originals.len(), 1, "and the backup is next to them");
    // a second backup sends nothing; the drive is unchanged
    let r = d.run("cloud.backup", json!({"wait": true}));
    assert_eq!(r["stored"], 0, "{r}");
    d.settle();
    assert_eq!(d.names("root").len(), 10);
}

/// A tiny PNG.
pub fn png() -> Vec<u8> {
    let (w, h) = (16usize, 12usize);
    let data: Vec<[u8; 4]> = (0..w * h).map(|i| [(i * 9) as u8, (i * 5) as u8, 80, 255]).collect();
    let img = lightcraft_raster::Rgba8 { width: w, height: h, data };
    lightcraft_codecs::encode_png(&lightcraft_codecs::EncodeImage::rgba8(&img), &lightcraft_codecs::EncodeMeta::default()).unwrap()
}

#[test]
fn unicode_paths_round_trip() {
    let mut d = drive("unicode", 1);
    let photo = d.file("Ísland 📷 2019/Þingvellir — día 1.png", &png());
    d.run("library.import", json!({"paths": [photo.to_string_lossy()]}));
    let r = d.run("cloud.backup", json!({"wait": true}));
    assert_eq!(r["failed"], 0, "{r}");
    let restored = temp_dir("unicode-restored");
    std::fs::remove_dir_all(&restored).unwrap();
    let mut other = Session::new().with_fs();
    let acc = d.accounts[0].join("lc");
    let r = other
        .execute("cloud.restore", &json!({"dir": restored.to_string_lossy(), "provider": "folder", "config": {"path": acc.to_string_lossy()}, "passphrase": PASS, "wait": true}))
        .unwrap();
    assert_eq!((r["library"]["photos"].as_u64(), r["library"]["missing"].as_u64()), (Some(1), Some(0)), "{r}");
    let found = walk(&restored).into_iter().any(|p| p.ends_with("Þingvellir — día 1.png"));
    assert!(found, "the restored original keeps its name");
    drop(other);
    let _ = std::fs::remove_dir_all(restored);
}

fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).into_iter().flatten().flatten() {
        if e.path().is_dir() {
            out.extend(walk(&e.path()));
        } else {
            out.push(e.path());
        }
    }
    out
}
