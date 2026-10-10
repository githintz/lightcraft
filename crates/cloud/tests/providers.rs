//! Every provider passes the shared contract; provider-specific edge cases; placement driving
//! real stores.

use std::path::PathBuf;

use lightcraft_cloud::{
    Availability, CloudError, ContentId, FolderProvider, MemProvider, ObjectName, PlacementAccount, PlacementRequest, Provider, RemoteRef,
    SpeedClass, Weights, choose, contract, fetch_blob, store_blob, usable_free,
};

fn temp_dir(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-cloud-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    d
}

#[test]
fn memory_provider_passes_the_contract() {
    contract::check_provider(&mut MemProvider::new("mem")).unwrap();
}

#[test]
fn folder_provider_passes_the_contract() {
    let dir = temp_dir("contract");
    let mut p = FolderProvider::create(&dir, "folder").unwrap();
    contract::check_provider(&mut p).unwrap();
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn folder_open_never_creates_a_missing_folder() {
    let dir = temp_dir("missing");
    assert!(matches!(FolderProvider::open(&dir, "x"), Err(CloudError::Io(_))));
    assert!(!dir.exists(), "an unplugged disk must not become an empty folder");
}

#[test]
fn folder_list_skips_foreign_and_temporary_files() {
    let dir = temp_dir("foreign");
    let mut p = FolderProvider::create(&dir, "folder").unwrap();
    let (id, _) = store_blob(&mut p, b"keep").unwrap();
    std::fs::write(dir.join("Desktop.ini"), b"x").unwrap(); // upper case: not an object name
    std::fs::write(dir.join(".tmp-1-2-x"), b"x").unwrap(); // a crashed upload's temp file
    std::fs::create_dir_all(dir.join("a/b/c/d/e")).unwrap();
    std::fs::write(dir.join("a/b/c/d/e/deep"), b"x").unwrap(); // deeper than MAX_DEPTH
    let names: Vec<String> = p.list().unwrap().into_iter().map(|l| l.name.to_string()).collect();
    assert_eq!(names, [ObjectName::blob(&id).to_string()]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn folder_refuses_hostile_refs() {
    let dir = temp_dir("hostile");
    let mut p = FolderProvider::create(&dir, "folder").unwrap();
    for key in ["../secret", "/etc/passwd", "a/../../b", ""] {
        let r = RemoteRef { key: key.into() };
        assert!(matches!(p.get(&r, 10), Err(CloudError::InvalidName(_))), "{key}");
        assert!(matches!(p.delete(&r), Err(CloudError::InvalidName(_))), "{key}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[cfg(unix)]
#[test]
fn folder_does_not_follow_links_out_of_the_folder() {
    let dir = temp_dir("link");
    let mut p = FolderProvider::create(&dir, "folder").unwrap();
    let outside = temp_dir("link-target");
    std::fs::write(&outside, b"private").unwrap();
    std::os::unix::fs::symlink(&outside, dir.join("leak")).unwrap();
    assert!(matches!(p.get(&RemoteRef { key: "leak".into() }, 100), Err(CloudError::Corrupt(_))));
    assert!(p.list().unwrap().is_empty());
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::remove_file(&outside).unwrap();
}

#[test]
fn memory_provider_limits_and_injected_failures() {
    let mut p = MemProvider::new("small").with_capacity(10).with_max_file_size(8);
    assert!(matches!(store_blob(&mut p, &[0; 9]), Err(CloudError::TooLarge { size: 9, max: 8 })));
    store_blob(&mut p, &[1; 6]).unwrap();
    assert_eq!(store_blob(&mut p, &[2; 6]).err(), Some(CloudError::QuotaExceeded));
    // replacing an object counts only the difference
    store_blob(&mut p, &[1; 6]).unwrap();
    p.fail_next(CloudError::Io("connection reset".into()));
    assert_eq!(p.list().err(), Some(CloudError::Io("connection reset".into())));
    assert_eq!(p.list().map(|l| l.len()).ok(), Some(1));
    assert_eq!(p.quota().ok().map(|q| (q.total, q.used)), Some((Some(10), 6)));
}

#[test]
fn damaged_blob_is_reported_not_returned() {
    let mut p = MemProvider::new("mem");
    let (id, remote) = store_blob(&mut p, b"original").unwrap();
    p.tamper(&ObjectName::blob(&id), b"tampered".to_vec());
    assert!(matches!(fetch_blob(&mut p, &remote, &id, 100), Err(CloudError::Corrupt(_))));
}

/// Placement picks accounts from their live state; the blob lands on each chosen account and
/// reads back from any of them.
#[test]
fn placement_drives_stores_across_accounts() {
    let mut accounts: Vec<(String, SpeedClass, MemProvider)> = vec![
        ("fast-a".into(), SpeedClass::Fast, MemProvider::new("fast-a").with_capacity(1_000)),
        ("cold-b".into(), SpeedClass::Archive, MemProvider::new("cold-b").with_capacity(100_000)),
        ("std-c".into(), SpeedClass::Standard, MemProvider::new("std-c").with_capacity(50_000)),
    ];
    let snapshot = |accounts: &mut Vec<(String, SpeedClass, MemProvider)>| -> Vec<PlacementAccount> {
        accounts
            .iter_mut()
            .map(|(id, speed, p)| {
                let q = p.quota().unwrap();
                PlacementAccount {
                    id: id.clone(),
                    provider: id.clone(),
                    free_bytes: usable_free(q.total, q.used, q.total, q.used),
                    capacity_bytes: q.total.unwrap_or(0),
                    speed: *speed,
                    reliability: 1.0,
                    priority: 0.5,
                    accepts_uploads: true,
                    max_file_size: None,
                }
            })
            .collect()
    };

    // a smart preview: Instant → only the fast account qualifies
    let preview = vec![7u8; 600];
    let plan = snapshot(&mut accounts);
    let chosen: Vec<String> =
        choose(&plan, &PlacementRequest::new(600, Availability::Instant), &Weights::default()).unwrap().iter().map(|a| a.id.clone()).collect();
    assert_eq!(chosen, ["fast-a"]);
    store_blob(&mut accounts[0].2, &preview).unwrap();

    // an original, two copies on Archive: the cold account first, never the nearly-full fast one
    let original = vec![9u8; 5_000];
    let plan = snapshot(&mut accounts);
    let req = PlacementRequest { copies: 2, ..PlacementRequest::new(5_000, Availability::Archive) };
    let chosen: Vec<String> = choose(&plan, &req, &Weights::default()).unwrap().iter().map(|a| a.id.clone()).collect();
    assert_eq!(chosen, ["cold-b", "std-c"]);

    let id = ContentId::of(&original);
    let mut refs = Vec::new();
    for (aid, _, p) in accounts.iter_mut().filter(|(aid, _, _)| chosen.contains(aid)) {
        let (got, r) = store_blob(p, &original).unwrap();
        assert_eq!(got, id);
        refs.push((aid.clone(), r));
    }
    for (aid, r) in &refs {
        let p = &mut accounts.iter_mut().find(|(a, _, _)| a == aid).unwrap().2;
        assert_eq!(fetch_blob(p, r, &id, 10_000).unwrap(), original);
    }
}
