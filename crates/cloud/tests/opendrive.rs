//! The OpenDrive provider against a fake OpenDrive API v1 on `127.0.0.1` (the password and
//! refresh grants with rotating refresh tokens, folders by id and by path, create / open /
//! chunk / close uploads checked by MD5, rename, delete, listings, user info), with fault
//! injection. Offline.

#![cfg(not(target_arch = "wasm32"))]

use std::time::Duration;

mod common;
use common::opendrive::{File, PASS, Shared, USER, start};
use lightcraft_cloud::opendrive::{OpenDriveConfig, OpenDriveProvider, login};
use lightcraft_cloud::{CloudError, Net, ObjectName, Provider, Quota, contract, fetch_blob, store_blob};
use md5::{Digest, Md5};

fn signed_in(base: &str, folder: &str) -> OpenDriveProvider {
    let mut config = login(Some(base), USER, PASS, &Net::default()).unwrap();
    config.folder = folder.into();
    let mut p = OpenDriveProvider::new(config, "OpenDrive").unwrap();
    p.retry_base = Duration::from_millis(1);
    p
}

fn bytes(n: usize, seed: u32) -> Vec<u8> {
    (0..n as u32).map(|i| (i.wrapping_mul(2_654_435_761).wrapping_add(seed) >> 13) as u8).collect()
}

/// The paths of every file (folder names joined by `/`).
fn paths(state: &Shared) -> Vec<String> {
    let s = state.lock().unwrap();
    let path_of = |mut id: String| {
        let mut parts = Vec::new();
        while let Some((parent, name)) = s.folders.get(&id) {
            parts.push(name.clone());
            id = parent.clone();
        }
        parts.reverse();
        parts.join("/")
    };
    s.files.values().map(|f| format!("{}/{}", path_of(f.folder.clone()), f.name)).collect()
}

#[test]
fn passes_the_provider_contract_in_private_folders() {
    for folder in ["LightCraft", "Backups/LightCraft"] {
        let (state, base) = start();
        let mut p = signed_in(&base, folder);
        contract::check_provider(&mut p).unwrap();
        let s = state.lock().unwrap();
        assert!(s.made_public.iter().all(|v| *v == 0), "every folder is private: {:?}", s.made_public);
        assert!(s.uploads.is_empty(), "every upload was closed");
    }
}

#[test]
fn objects_live_in_the_app_folder_and_others_are_skipped() {
    let (state, base) = start();
    let mut p = signed_in(&base, "LightCraft");
    assert!(p.list().unwrap().is_empty(), "no app folder yet: nothing listed, nothing made");
    assert!(state.lock().unwrap().folders.is_empty());
    for i in 0..12u32 {
        store_blob(&mut p, &i.to_le_bytes()).unwrap();
    }
    assert!(paths(&state).iter().all(|p| p.starts_with("LightCraft/blobs/")), "{:?}", paths(&state));
    {
        let mut s = state.lock().unwrap();
        // things the provider must skip: a file outside, a name that isn't ours, an interrupted upload
        let root = s.folder_by_path("LightCraft").unwrap();
        s.files.insert("x1".into(), File { folder: "0".into(), name: "notes.txt".into(), data: vec![1] });
        s.files.insert("x2".into(), File { folder: root.clone(), name: "Photo.JPG".into(), data: vec![1] });
        s.files.insert("x3".into(), File { folder: root, name: ".lc-upload-vault.enc".into(), data: vec![] });
    }
    assert_eq!(p.list().unwrap().len(), 12);
    assert_eq!(p.quota().unwrap(), Quota { total: Some(1024 * 1024 * 1024), used: 48 + 2 });
}

#[test]
fn large_files_go_in_chunks_that_survive_lost_answers_and_locks() {
    let (state, base) = start();
    let mut p = signed_in(&base, "LightCraft").with_chunk_size(64 * 1024).unwrap();
    {
        let mut s = state.lock().unwrap();
        s.lost_answers = 2;
        s.locked = 2;
    }
    let data = bytes(5 * 64 * 1024 + 7, 1);
    let (id, remote) = store_blob(&mut p, &data).unwrap();
    assert_eq!(fetch_blob(&mut p, &remote, &id, data.len() as u64).unwrap(), data);
    {
        let s = state.lock().unwrap();
        assert_eq!((s.lost_answers, s.locked, s.offset_errors), (0, 0, 2), "each resent chunk was answered with where to go on");
        assert_eq!(s.chunks, 6);
    }
    // replacing it keeps one file, under its name, and no upload leftovers
    let name = ObjectName::blob(&id);
    p.put(&name, b"small now").unwrap();
    assert_eq!(p.get(&remote, 100).unwrap(), b"small now");
    let files = paths(&state);
    assert_eq!(files.len(), 1, "{files:?}");
    assert!(files[0].ends_with(&format!("{id}.bin")));
    // too big for the caller's limit: refused
    assert!(matches!(p.get(&remote, 3), Err(CloudError::TooLarge { .. })));
}

#[test]
fn an_interrupted_upload_never_shows_and_is_cleaned_up() {
    let (state, base) = start();
    let mut p = signed_in(&base, "LightCraft").with_chunk_size(1024).unwrap();
    let name = ObjectName::parse("manifest/snapshot-1.json.enc").unwrap();
    p.put(&name, b"old").unwrap();
    // the server keeps failing mid-upload: the old object stays, nothing short appears, and the
    // half-sent file is removed
    let big = bytes(10 * 1024, 2);
    state.lock().unwrap().chunks = 0;
    {
        let mut s = state.lock().unwrap();
        s.broken_chunks = 100;
    }
    let r = p.put(&name, &big);
    assert!(matches!(r, Err(CloudError::Io(_))), "{r:?}");
    assert!(paths(&state).iter().all(|f| !f.contains(".lc-upload-")), "{:?}", paths(&state));
    assert_eq!(p.get(&lightcraft_cloud::RemoteRef { key: name.as_str().into() }, 100).unwrap(), b"old");
    assert_eq!(p.list().unwrap().len(), 1);
    // a crash before close left a hidden temporary file: the next upload of the name clears it
    {
        let mut s = state.lock().unwrap();
        s.broken_chunks = 0;
        let folder = s.folder_by_path("LightCraft/manifest").unwrap();
        s.files.insert("stale".into(), File { folder, name: ".lc-upload-snapshot-1.json.enc".into(), data: vec![1, 2] });
    }
    p.put(&name, &big).unwrap();
    assert_eq!(p.get(&lightcraft_cloud::RemoteRef { key: name.as_str().into() }, u64::MAX).unwrap(), big);
    assert!(paths(&state).iter().all(|f| !f.contains(".lc-upload-")), "{:?}", paths(&state));
}

#[test]
fn content_opendrive_already_holds_is_not_sent_again() {
    let (state, base) = start();
    let mut p = signed_in(&base, "LightCraft");
    state.lock().unwrap().dedup = true;
    let name = ObjectName::parse("account.json").unwrap();
    p.put(&name, b"xxxx").unwrap();
    let s = state.lock().unwrap();
    assert_eq!(s.chunks, 0);
    assert!(s.uploads.is_empty());
}

#[test]
fn sign_in_exchanges_the_password_once_and_tokens_rotate() {
    let (state, base) = start();
    let net = Net::default();
    assert!(matches!(login(Some(&base), USER, "wrong", &net), Err(CloudError::Denied(m)) if m.starts_with("OpenDrive refused the sign-in")));
    assert!(login(Some(&base), "", PASS, &net).is_err());
    let config = login(Some(&base), USER, PASS, &net).unwrap();
    let json = serde_json::to_string(&config).unwrap();
    assert!(!json.contains(PASS) && !json.contains(USER), "only the refresh token is kept: {json}");
    let first = config.refresh_token.clone();
    let mut p = OpenDriveProvider::new(config, "OpenDrive").unwrap();
    p.quota().unwrap();
    // using it replaced the refresh token; the provider has the new one for the vault
    assert_ne!(Provider::refresh_token(&p), Some(first.as_str()));
    assert_eq!(Provider::refresh_token(&p), Some(state.lock().unwrap().refresh.as_str()));
    // an access token that stopped working is replaced once, with the rotated refresh token
    state.lock().unwrap().access.clear();
    p.quota().unwrap();
    // the old refresh token no longer works
    let stale = OpenDriveConfig::new(&base, first);
    let mut old = OpenDriveProvider::new(stale, "old").unwrap();
    assert!(matches!(old.quota(), Err(CloudError::Denied(m)) if m.starts_with("sign in again")));
}

#[test]
fn rate_limits_are_waited_out() {
    let (state, base) = start();
    let mut p = signed_in(&base, "LightCraft");
    state.lock().unwrap().rate_limited = 4;
    store_blob(&mut p, b"after the limit").unwrap();
    state.lock().unwrap().rate_limited = 100;
    assert!(matches!(p.quota(), Err(CloudError::Io(_))));
}

#[test]
fn the_users_own_folders_can_be_read_but_nothing_else() {
    use lightcraft_cloud::Browse;
    let (state, base) = start();
    let big = bytes(300_000, 9);
    {
        let mut s = state.lock().unwrap();
        s.add_file("Photos A/Iceland 2019/DSC_0001.NEF", &big);
        s.add_file("Photos A/Iceland 2019/DSC_0001.JPG", b"jpeg");
        s.add_file("Photos A/Notes.txt", b"n");
    }
    let mut b = signed_in(&base, "LightCraft").into_browser();
    assert!(matches!(b.folder("Photos B"), Err(CloudError::NotFound)));
    let root = b.folder("/Photos A/").unwrap();
    let dir = b.list(&root).unwrap();
    assert_eq!(dir.folders.iter().map(|(_, n)| n.as_str()).collect::<Vec<_>>(), ["Iceland 2019"]);
    assert_eq!(dir.files.iter().map(|f| f.name.as_str()).collect::<Vec<_>>(), ["Notes.txt"]);
    let trip = b.list(&dir.folders[0].0).unwrap();
    let nef = trip.files.iter().find(|f| f.name == "DSC_0001.NEF").unwrap().clone();
    assert_eq!(nef.size, 300_000);
    let hash: String = Md5::digest(&big).iter().map(|b| format!("{b:02x}")).collect();
    assert_eq!(b.content_hash(&nef.id).unwrap(), Some(hash));
    // the start, a middle part, past the end
    assert_eq!(b.read_range(&nef.id, 0, 1000).unwrap(), &big[..1000]);
    assert_eq!(b.read_range(&nef.id, 299_990, 1000).unwrap(), &big[299_990..]);
    assert!(b.read_range(&nef.id, 400_000, 10).unwrap().is_empty());
    assert!(b.list("../x").is_err() && b.read_range("a/b", 0, 1).is_err(), "ids are checked before they go in a URL");
    // nothing in the folders changed
    let s = state.lock().unwrap();
    assert_eq!(s.files.len(), 3);
    assert!(s.files.values().any(|f| f.name == "DSC_0001.NEF" && f.data == big));
}

#[test]
fn parallel_workers_share_one_rotating_token() {
    use lightcraft_cloud::Browse;
    let (state, base) = start();
    let ids: Vec<String> = {
        let mut s = state.lock().unwrap();
        (0..40).map(|i| s.add_file(&format!("Photos/T/IMG_{i}.JPG"), &bytes(5000, i))).collect()
    };
    let main = signed_in(&base, "LightCraft").into_browser();
    let handles: Vec<_> = (0..8)
        .map(|w| {
            let mut b = main.worker();
            let (ids, state) = (ids.clone(), state.clone());
            std::thread::spawn(move || {
                for (i, id) in ids.iter().enumerate().filter(|(i, _)| i % 8 == w) {
                    if i == 20 {
                        // every access token stops working while the others are mid-flight
                        state.lock().unwrap().access.clear();
                    }
                    assert_eq!(b.read_range(id, 0, 10).unwrap(), &bytes(5000, i as u32)[..10]);
                    assert!(b.content_hash(id).unwrap().is_some());
                }
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    // one refresh token lineage: the one the browsers hold is the one OpenDrive accepts
    assert_eq!(main.refresh_token(), state.lock().unwrap().refresh);
}
