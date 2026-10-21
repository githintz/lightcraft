//! Backup and restore over several in-memory accounts: placement across providers, skipping
//! what is already stored, content changes, the manifest on every account, restore with a
//! damaged copy, an account that fails, and pruning.

use std::sync::{Arc, Mutex};

use lightcraft_cloud::backup::{Account, AccountSettings, Fleet, Item, Outcome};
use lightcraft_cloud::crypto::{Key, TestRandom};
use lightcraft_cloud::{
    Availability, CloudError, ContentId, Listed, Manifest, MemProvider, ObjectName, Provider, ProviderInfo, Quota, RemoteRef, Result,
};

/// A memory account the test keeps a handle on, under a provider name of its choosing.
#[derive(Clone)]
struct Shared {
    kind: &'static str,
    mem: Arc<Mutex<MemProvider>>,
}

impl Provider for Shared {
    fn info(&self) -> ProviderInfo {
        ProviderInfo { provider: self.kind.into(), ..self.mem.lock().unwrap().info() }
    }
    fn quota(&mut self) -> Result<Quota> {
        self.mem.lock().unwrap().quota()
    }
    fn put(&mut self, name: &ObjectName, data: &[u8]) -> Result<RemoteRef> {
        self.mem.lock().unwrap().put(name, data)
    }
    fn get(&mut self, remote: &RemoteRef, max_len: u64) -> Result<Vec<u8>> {
        self.mem.lock().unwrap().get(remote, max_len)
    }
    fn delete(&mut self, remote: &RemoteRef) -> Result<()> {
        self.mem.lock().unwrap().delete(remote)
    }
    fn list(&mut self) -> Result<Vec<Listed>> {
        self.mem.lock().unwrap().list()
    }
}

fn shared(kind: &'static str, label: &str) -> Shared {
    Shared { kind, mem: Arc::new(Mutex::new(MemProvider::new(label).with_capacity(100_000_000))) }
}

fn fleet(accounts: &[(&str, &Shared)]) -> Fleet {
    let accounts = accounts.iter().map(|(id, s)| Account::new(*id, Box::new((*s).clone()), AccountSettings::default())).collect();
    let mut f = Fleet::new(accounts, Key::from_bytes([7; 32]), "laptop", Manifest::new());
    assert!(f.refresh_quotas().is_empty());
    f
}

fn item(id: &str, copies: u8) -> Item {
    Item { id: id.into(), name: format!("/photos/{id}.jpg"), level: Availability::Standard, copies, everywhere: false }
}

fn blob_count(s: &Shared) -> usize {
    s.mem.lock().unwrap().list().unwrap().iter().filter(|l| l.name.blob_id().is_some()).count()
}

#[test]
fn backup_places_copies_on_different_providers_and_skips_what_is_stored() {
    let (a, b, c) = (shared("gdrive", "a"), shared("gdrive", "b"), shared("s3", "c"));
    let mut f = fleet(&[("a", &a), ("b", &b), ("c", &c)]);
    let mut rng = TestRandom(1);
    let photo = vec![9u8; 50_000];

    let Outcome::Stored(on) = f.backup(&item("original.1", 2), &photo, &mut rng).unwrap() else { panic!() };
    assert_eq!(on.len(), 2);
    assert!(on.contains(&"c".to_string()), "one copy on each provider: {on:?}");
    assert_eq!(f.backup(&item("original.1", 2), &photo, &mut rng).unwrap(), Outcome::Unchanged);

    // stored encrypted: no account holds the plaintext
    let id = ContentId::of(&photo);
    let stored = c.mem.lock().unwrap().get(&RemoteRef { key: ObjectName::blob(&id).as_str().into() }, u64::MAX).unwrap();
    assert_ne!(stored, photo);
    assert!(!stored.windows(64).any(|w| w == &photo[..64]));

    // the catalog goes everywhere
    let catalog = Item { id: "catalog.catalog.snap".into(), name: "catalog.snap".into(), level: Availability::Instant, copies: 1, everywhere: true };
    assert_eq!(f.backup(&catalog, b"{\"catalog\":1}", &mut rng).unwrap(), Outcome::Stored(vec!["a".into(), "b".into(), "c".into()]));

    // new content for the same file: stored again, the old copies no longer count
    let edited = vec![8u8; 40_000];
    let Outcome::Stored(on2) = f.backup(&item("original.1", 1), &edited, &mut rng).unwrap() else { panic!() };
    assert_eq!(on2.len(), 1);
    let e = f.manifest.get("original.1").unwrap();
    assert_eq!((e.content, e.size, e.replicas.len()), (Some(ContentId::of(&edited)), 40_000, 1));
}

#[test]
fn a_manifest_saved_to_the_accounts_restores_everything_even_with_a_damaged_copy() {
    let (a, b) = (shared("gdrive", "a"), shared("dropbox", "b"));
    let mut rng = TestRandom(2);
    let files: Vec<(String, Vec<u8>)> = (0..5u8).map(|i| (format!("original.{i}"), vec![i; 10_000 + usize::from(i)])).collect();
    {
        let mut f = fleet(&[("a", &a), ("b", &b)]);
        for (id, data) in &files {
            f.backup(&item(id, 2), data, &mut rng).unwrap();
        }
        assert!(f.save_manifest(&mut rng).unwrap().is_empty());
        // later runs keep only the newest snapshots
        f.backup(&item("original.9", 1), b"later", &mut rng).unwrap();
        f.save_manifest(&mut rng).unwrap();
        f.backup(&item("original.10", 1), b"later still", &mut rng).unwrap();
        f.save_manifest(&mut rng).unwrap();
    }
    let snapshots = a.mem.lock().unwrap().list().unwrap().iter().filter(|l| l.name.as_str().starts_with("manifest/")).count();
    assert_eq!(snapshots, 2);

    // another machine: the same accounts and key, an empty manifest
    let mut f = fleet(&[("a", &a), ("b", &b)]);
    f.manifest = f.load_manifest().unwrap();
    assert_eq!(f.manifest.entries().count(), 7);

    // one account's copy of the first file is damaged: the other copy is used
    let first = f.manifest.get("original.0").unwrap();
    let key = first.replicas["a"].clone();
    a.mem.lock().unwrap().tamper(&ObjectName::parse(&key).unwrap(), vec![1, 2, 3]);
    for (id, data) in &files {
        let e = f.manifest.get(id).unwrap();
        assert_eq!(&f.restore(&e).unwrap(), data);
    }

    // a wrong key opens nothing
    let mut wrong = fleet(&[("a", &a), ("b", &b)]);
    wrong.data_key = Key::from_bytes([8; 32]);
    assert!(wrong.load_manifest().is_err());
}

#[test]
fn a_failing_account_is_skipped_and_reported() {
    let (a, b) = (shared("gdrive", "a"), shared("s3", "b"));
    let mut f = fleet(&[("a", &a), ("b", &b)]);
    let mut rng = TestRandom(3);
    a.mem.lock().unwrap().fail_next(CloudError::Io("connection reset".into()));
    let r = f.backup(&item("original.1", 2), b"bytes", &mut rng).unwrap();
    assert_eq!(r, Outcome::Stored(vec!["b".into()]), "stored where it could be");
    // the next run adds the missing copy
    let r = f.backup(&item("original.1", 2), b"bytes", &mut rng).unwrap();
    assert_eq!(r, Outcome::Stored(vec!["a".into()]));

    // nowhere to store it is an error
    let full = Shared { kind: "s3", mem: Arc::new(Mutex::new(MemProvider::new("full").with_capacity(10))) };
    let mut f = fleet(&[("full", &full)]);
    assert!(f.backup(&item("original.2", 1), &[0u8; 100], &mut rng).is_err());
    assert!(f.manifest.get("original.2").is_none(), "nothing recorded when no account has room");
}

#[test]
fn prune_removes_blobs_no_file_refers_to() {
    let a = shared("gdrive", "a");
    let mut f = fleet(&[("a", &a)]);
    let mut rng = TestRandom(4);
    f.backup(&item("original.1", 1), b"first version", &mut rng).unwrap();
    f.backup(&item("original.1", 1), b"second version", &mut rng).unwrap();
    f.backup(&item("original.2", 1), b"deleted photo", &mut rng).unwrap();
    assert_eq!(blob_count(&a), 3);
    let keep = ["original.1".to_string()].into_iter().collect();
    assert_eq!(f.trash_except(&keep).unwrap(), 1);
    assert_eq!(f.prune().unwrap(), 2);
    assert_eq!(blob_count(&a), 1);
    assert!(f.manifest.get("original.2").is_none());
    let e = f.manifest.get("original.1").unwrap();
    assert_eq!(f.restore(&e).unwrap(), b"second version");
}
