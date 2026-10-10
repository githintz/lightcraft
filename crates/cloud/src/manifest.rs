//! The manifest: which files and folders exist, their names, places and content, and which
//! accounts hold each blob. An op log plus snapshots, like the catalog journal, copied to every
//! account (`docs/cloud-format.md` → Manifest).
//!
//! Every op carries a [`Stamp`] (Lamport clock, device id). Each field of an entry (existence,
//! kind, name, parent, content, availability, trashed, each replica) keeps the stamp of the op
//! that last set it, and an op only changes a field whose stamp is older. So applying the same
//! ops in any order, any number of times, gives the same manifest: devices merge by exchanging
//! ops. Concurrent edits of one field resolve to the later stamp (last writer wins); keeping both
//! versions of concurrently edited content is planned with multi-device support.
//!
//! Folders can be moved into each other concurrently and form a cycle; [`Manifest::path`] and
//! [`Manifest::children`] then show the entries of the cycle at the top level instead of looping.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::{Availability, CloudError, ContentId, Result};

pub const FORMAT: &str = "lightcraft-cloud-manifest";
pub const VERSION: u32 = 1;
/// Longest id (entry, device, account) in bytes.
pub const MAX_ID: usize = 128;
/// Longest file or folder name in bytes.
pub const MAX_NAME: usize = 1024;
/// Longest provider reference in bytes.
pub const MAX_KEY: usize = 2048;
/// Most replicas recorded per entry.
pub const MAX_REPLICAS: usize = 64;
/// Deepest folder nesting [`Manifest::path`] follows.
pub const MAX_DEPTH: usize = 256;

/// When and where an op was made. Ordered by clock, then device id.
#[derive(Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Stamp {
    pub lamport: u64,
    pub device: String,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryKind {
    #[default]
    File,
    Folder,
}

/// A visible file or folder.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub kind: EntryKind,
    pub name: String,
    /// `None` = at the top level.
    pub parent: Option<String>,
    pub content: Option<ContentId>,
    pub size: u64,
    pub availability: Availability,
    pub trashed: bool,
    /// account id → the provider's reference for the blob there.
    pub replicas: BTreeMap<String, String>,
}

impl Entry {
    pub fn file(id: &str, name: &str, parent: Option<&str>, content: ContentId, size: u64) -> Entry {
        Entry {
            id: id.into(),
            kind: EntryKind::File,
            name: name.into(),
            parent: parent.map(str::to_string),
            content: Some(content),
            size,
            availability: Availability::Standard,
            trashed: false,
            replicas: BTreeMap::new(),
        }
    }

    pub fn folder(id: &str, name: &str, parent: Option<&str>) -> Entry {
        Entry {
            id: id.into(),
            kind: EntryKind::Folder,
            name: name.into(),
            parent: parent.map(str::to_string),
            content: None,
            size: 0,
            availability: Availability::Standard,
            trashed: false,
            replicas: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "camelCase")]
pub enum OpKind {
    /// Create (or recreate after a delete) an entry with all its fields.
    Create {
        entry: Entry,
    },
    Rename {
        id: String,
        name: String,
    },
    Move {
        id: String,
        parent: Option<String>,
    },
    SetContent {
        id: String,
        content: ContentId,
        size: u64,
    },
    SetAvailability {
        id: String,
        availability: Availability,
    },
    SetTrashed {
        id: String,
        trashed: bool,
    },
    AddReplica {
        id: String,
        account: String,
        key: String,
    },
    RemoveReplica {
        id: String,
        account: String,
    },
    Delete {
        id: String,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Op {
    pub stamp: Stamp,
    #[serde(flatten)]
    pub kind: OpKind,
}

/// A field and the stamp of the op that last set it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Lww<T> {
    at: Stamp,
    value: T,
}

impl<T: Ord> Lww<T> {
    /// Equal stamps (a buggy or hostile device reusing one) fall back to comparing values, so
    /// the result still doesn't depend on order.
    fn set(&mut self, at: &Stamp, value: T) {
        if *at > self.at || (*at == self.at && value > self.value) {
            self.at = at.clone();
            self.value = value;
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
struct Record {
    exists: Lww<bool>,
    kind: Lww<EntryKind>,
    name: Lww<String>,
    parent: Lww<Option<String>>,
    content: Lww<Option<(ContentId, u64)>>,
    availability: Lww<Availability>,
    trashed: Lww<bool>,
    replicas: BTreeMap<String, Lww<Option<String>>>,
}

impl Record {
    fn visible(&self) -> bool {
        self.exists.value
    }

    fn entry(&self, id: &str) -> Entry {
        let (content, size) = match &self.content.value {
            Some((c, s)) => (Some(c.clone()), *s),
            None => (None, 0),
        };
        Entry {
            id: id.into(),
            kind: self.kind.value,
            name: self.name.value.clone(),
            parent: self.parent.value.clone(),
            content,
            size,
            availability: self.availability.value,
            trashed: self.trashed.value,
            replicas: self.replicas.iter().filter_map(|(a, r)| r.value.clone().map(|k| (a.clone(), k))).collect(),
        }
    }

    fn set_replica(&mut self, at: &Stamp, account: &str, key: Option<String>) -> Result<()> {
        if !self.replicas.contains_key(account) && self.replicas.len() >= MAX_REPLICAS {
            return Err(CloudError::Corrupt(format!("more than {MAX_REPLICAS} replicas")));
        }
        self.replicas.entry(account.to_string()).or_default().set(at, key);
        Ok(())
    }
}

fn check_id(what: &str, s: &str) -> Result<()> {
    if s.is_empty() || s.len() > MAX_ID || !s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
        return Err(CloudError::Corrupt(format!("bad {what} id {:?}", s.chars().take(40).collect::<String>())));
    }
    Ok(())
}

fn check_text(what: &str, s: &str, max: usize) -> Result<()> {
    if s.is_empty() || s.len() > max || s.chars().any(char::is_control) {
        return Err(CloudError::Corrupt(format!("bad {what} {:?}", s.chars().take(40).collect::<String>())));
    }
    Ok(())
}

impl Op {
    /// Refuse ops with ids, names or keys a reader can't safely show or store.
    pub fn check(&self) -> Result<()> {
        check_id("device", &self.stamp.device)?;
        match &self.kind {
            OpKind::Create { entry } => {
                check_id("entry", &entry.id)?;
                check_text("name", &entry.name, MAX_NAME)?;
                if let Some(p) = &entry.parent {
                    check_id("parent", p)?;
                }
                if entry.replicas.len() > MAX_REPLICAS {
                    return Err(CloudError::Corrupt(format!("more than {MAX_REPLICAS} replicas")));
                }
                for (account, key) in &entry.replicas {
                    check_id("account", account)?;
                    check_text("key", key, MAX_KEY)?;
                }
            }
            OpKind::Rename { id, name } => {
                check_id("entry", id)?;
                check_text("name", name, MAX_NAME)?;
            }
            OpKind::Move { id, parent } => {
                check_id("entry", id)?;
                if let Some(p) = parent {
                    check_id("parent", p)?;
                }
            }
            OpKind::AddReplica { id, account, key } => {
                check_id("entry", id)?;
                check_id("account", account)?;
                check_text("key", key, MAX_KEY)?;
            }
            OpKind::RemoveReplica { id, account } => {
                check_id("entry", id)?;
                check_id("account", account)?;
            }
            OpKind::SetContent { id, .. } | OpKind::SetAvailability { id, .. } | OpKind::SetTrashed { id, .. } | OpKind::Delete { id } => {
                check_id("entry", id)?;
            }
        }
        Ok(())
    }

    fn target(&self) -> &str {
        match &self.kind {
            OpKind::Create { entry } => &entry.id,
            OpKind::Rename { id, .. }
            | OpKind::Move { id, .. }
            | OpKind::SetContent { id, .. }
            | OpKind::SetAvailability { id, .. }
            | OpKind::SetTrashed { id, .. }
            | OpKind::AddReplica { id, .. }
            | OpKind::RemoveReplica { id, .. }
            | OpKind::Delete { id } => id,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Highest Lamport clock seen.
    clock: u64,
    records: BTreeMap<String, Record>,
}

#[derive(Serialize, Deserialize)]
struct Snapshot {
    format: String,
    version: u32,
    manifest: Manifest,
}

impl Manifest {
    pub fn new() -> Manifest {
        Manifest::default()
    }

    /// The stamp for this device's next op.
    pub fn next_stamp(&mut self, device: &str) -> Stamp {
        self.clock = self.clock.saturating_add(1);
        Stamp { lamport: self.clock, device: device.to_string() }
    }

    pub fn clock(&self) -> u64 {
        self.clock
    }

    /// Apply one op (validated first). Ops about entries not created yet are kept and take
    /// effect when the entry appears.
    pub fn apply(&mut self, op: &Op) -> Result<()> {
        op.check()?;
        self.clock = self.clock.max(op.stamp.lamport);
        let at = &op.stamp;
        let r = self.records.entry(op.target().to_string()).or_default();
        match &op.kind {
            OpKind::Create { entry } => {
                r.exists.set(at, true);
                r.kind.set(at, entry.kind);
                r.name.set(at, entry.name.clone());
                r.parent.set(at, entry.parent.clone());
                r.content.set(at, entry.content.clone().map(|c| (c, entry.size)));
                r.availability.set(at, entry.availability);
                r.trashed.set(at, entry.trashed);
                for (account, key) in &entry.replicas {
                    r.set_replica(at, account, Some(key.clone()))?;
                }
            }
            OpKind::Rename { name, .. } => r.name.set(at, name.clone()),
            OpKind::Move { parent, .. } => r.parent.set(at, parent.clone()),
            OpKind::SetContent { content, size, .. } => r.content.set(at, Some((content.clone(), *size))),
            OpKind::SetAvailability { availability, .. } => r.availability.set(at, *availability),
            OpKind::SetTrashed { trashed, .. } => r.trashed.set(at, *trashed),
            OpKind::AddReplica { account, key, .. } => r.set_replica(at, account, Some(key.clone()))?,
            OpKind::RemoveReplica { account, .. } => r.set_replica(at, account, None)?,
            OpKind::Delete { .. } => r.exists.set(at, false),
        }
        Ok(())
    }

    /// Make an op stamped for `device`, apply it, and return it (to append to the log).
    pub fn record(&mut self, device: &str, kind: OpKind) -> Result<Op> {
        let op = Op { stamp: self.next_stamp(device), kind };
        self.apply(&op)?;
        Ok(op)
    }

    pub fn get(&self, id: &str) -> Option<Entry> {
        self.records.get(id).filter(|r| r.visible()).map(|r| r.entry(id))
    }

    /// Every visible entry, by id.
    pub fn entries(&self) -> impl Iterator<Item = Entry> + '_ {
        self.records.iter().filter(|(_, r)| r.visible()).map(|(id, r)| r.entry(id))
    }

    /// The folder an entry shows in: its parent when that is a visible folder and the entry is
    /// not itself part of a cycle of folders, else the top level (`None`).
    pub fn effective_parent(&self, id: &str) -> Option<String> {
        let is_folder = |i: &str| self.records.get(i).filter(|r| r.visible() && r.kind.value == EntryKind::Folder);
        let first = self.records.get(id)?.parent.value.clone()?;
        is_folder(&first)?;
        let mut seen = BTreeSet::new();
        let mut cur = first.clone();
        for _ in 0..MAX_DEPTH {
            if cur == id {
                return None; // id is inside its own ancestry: a member of the cycle
            }
            if !seen.insert(cur.clone()) {
                break; // a cycle further up that id is not part of
            }
            match is_folder(&cur).and_then(|r| r.parent.value.clone()) {
                Some(p) => cur = p,
                None => break,
            }
        }
        Some(first)
    }

    /// Visible entries directly in `parent` (`None` = the top level), by name then id.
    pub fn children(&self, parent: Option<&str>) -> Vec<Entry> {
        let mut out: Vec<Entry> = self.entries().filter(|e| self.effective_parent(&e.id).as_deref() == parent).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.id.cmp(&b.id)));
        out
    }

    /// Names from the top level down to `id`, following [`Manifest::effective_parent`].
    pub fn path(&self, id: &str) -> Vec<String> {
        let mut names = Vec::new();
        let mut cur = Some(id.to_string());
        while let Some(c) = cur {
            if names.len() >= MAX_DEPTH {
                break;
            }
            let Some(e) = self.get(&c) else { break };
            names.push(e.name);
            cur = self.effective_parent(&c);
        }
        names.reverse();
        names
    }

    pub fn to_snapshot(&self) -> Result<Vec<u8>> {
        serde_json::to_vec(&Snapshot { format: FORMAT.into(), version: VERSION, manifest: self.clone() }).map_err(|e| CloudError::Io(e.to_string()))
    }

    /// Read a snapshot, refusing one larger than `max_len` bytes or holding invalid data.
    pub fn from_snapshot(data: &[u8], max_len: usize) -> Result<Manifest> {
        if data.len() > max_len {
            return Err(CloudError::TooLarge { size: data.len() as u64, max: max_len as u64 });
        }
        let s: Snapshot = serde_json::from_slice(data).map_err(|e| CloudError::Corrupt(format!("manifest snapshot: {e}")))?;
        if s.format != FORMAT {
            return Err(CloudError::Corrupt("not a manifest snapshot".into()));
        }
        if s.version > VERSION {
            return Err(CloudError::Unsupported(format!("manifest version {} (newer LightCraft)", s.version)));
        }
        for (id, r) in &s.manifest.records {
            check_id("entry", id)?;
            if r.visible() {
                check_text("name", &r.name.value, MAX_NAME)?;
            }
            if r.replicas.len() > MAX_REPLICAS {
                return Err(CloudError::Corrupt(format!("more than {MAX_REPLICAS} replicas")));
            }
            for (account, key) in &r.replicas {
                check_id("account", account)?;
                if let Some(k) = &key.value {
                    check_text("key", k, MAX_KEY)?;
                }
            }
        }
        Ok(s.manifest)
    }
}

/// Ops as JSON lines, one per line.
pub fn write_ops(ops: &[Op]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for op in ops {
        serde_json::to_writer(&mut out, op).map_err(|e| CloudError::Io(e.to_string()))?;
        out.push(b'\n');
    }
    Ok(out)
}

/// Read JSON-lines ops, at most `max_ops` of them; any invalid line is an error.
pub fn read_ops(data: &[u8], max_ops: usize) -> Result<Vec<Op>> {
    let mut ops = Vec::new();
    for (n, line) in data.split(|b| *b == b'\n').enumerate() {
        if line.iter().all(u8::is_ascii_whitespace) {
            continue;
        }
        if ops.len() >= max_ops {
            return Err(CloudError::TooLarge { size: ops.len() as u64 + 1, max: max_ops as u64 });
        }
        let op: Op = serde_json::from_slice(line).map_err(|e| CloudError::Corrupt(format!("manifest op on line {}: {e}", n + 1)))?;
        op.check()?;
        ops.push(op);
    }
    Ok(ops)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cid(s: &str) -> ContentId {
        ContentId::of(s.as_bytes())
    }

    fn stamp(l: u64, d: &str) -> Stamp {
        Stamp { lamport: l, device: d.into() }
    }

    fn op(l: u64, d: &str, kind: OpKind) -> Op {
        Op { stamp: stamp(l, d), kind }
    }

    fn sample_ops() -> Vec<Op> {
        vec![
            op(1, "mac", OpKind::Create { entry: Entry::folder("f1", "Trips", None) }),
            op(2, "mac", OpKind::Create { entry: Entry::file("p1", "IMG_1.CR3", Some("f1"), cid("a"), 10) }),
            op(3, "mac", OpKind::AddReplica { id: "p1".into(), account: "nas".into(), key: "blobs/aa/x.bin".into() }),
            op(3, "pc", OpKind::Rename { id: "p1".into(), name: "Beach.CR3".into() }),
            op(4, "mac", OpKind::Rename { id: "p1".into(), name: "Dunes.CR3".into() }),
            op(4, "pc", OpKind::SetAvailability { id: "p1".into(), availability: Availability::Archive }),
            op(5, "pc", OpKind::AddReplica { id: "p1".into(), account: "b2".into(), key: "k2".into() }),
            op(6, "mac", OpKind::RemoveReplica { id: "p1".into(), account: "nas".into() }),
            op(6, "pc", OpKind::Create { entry: Entry::file("p2", "IMG_2.CR3", None, cid("b"), 20) }),
            op(7, "mac", OpKind::Delete { id: "p2".into() }),
            op(8, "pc", OpKind::SetContent { id: "p1".into(), content: cid("c"), size: 30 }),
        ]
    }

    fn apply_all(ops: &[Op]) -> Manifest {
        let mut m = Manifest::new();
        for o in ops {
            m.apply(o).unwrap();
        }
        m
    }

    #[test]
    fn order_and_duplicates_do_not_matter() {
        let ops = sample_ops();
        let forward = apply_all(&ops);
        let mut reversed = ops.clone();
        reversed.reverse();
        assert_eq!(apply_all(&reversed), forward);
        // a fixed shuffle, and every op twice
        let mut shuffled: Vec<Op> = ops.iter().step_by(2).chain(ops.iter().skip(1).step_by(2)).cloned().collect();
        shuffled.extend(ops.iter().cloned());
        assert_eq!(apply_all(&shuffled), forward);

        let p1 = forward.get("p1").unwrap();
        assert_eq!(p1.name, "Dunes.CR3", "the later rename wins");
        assert_eq!(p1.availability, Availability::Archive);
        assert_eq!((p1.content, p1.size), (Some(cid("c")), 30));
        assert_eq!(p1.replicas, BTreeMap::from([("b2".to_string(), "k2".to_string())]));
        assert!(forward.get("p2").is_none(), "deleted");
        assert_eq!(forward.path("p1"), ["Trips", "Dunes.CR3"]);
        assert_eq!(forward.clock(), 8);
    }

    #[test]
    fn equal_stamps_still_converge() {
        let a = op(5, "x", OpKind::Rename { id: "p".into(), name: "A".into() });
        let b = op(5, "x", OpKind::Rename { id: "p".into(), name: "B".into() });
        let c = op(1, "x", OpKind::Create { entry: Entry::folder("p", "start", None) });
        assert_eq!(apply_all(&[c.clone(), a.clone(), b.clone()]), apply_all(&[b, a, c]));
    }

    #[test]
    fn recreate_after_delete_follows_stamps() {
        let ops = [
            op(1, "a", OpKind::Create { entry: Entry::folder("f", "F", None) }),
            op(2, "a", OpKind::Delete { id: "f".into() }),
            op(3, "b", OpKind::Create { entry: Entry::folder("f", "F again", None) }),
        ];
        let m = apply_all(&ops);
        assert_eq!(m.get("f").map(|e| e.name), Some("F again".into()));
        let mut rev = ops.to_vec();
        rev.reverse();
        assert_eq!(apply_all(&rev), m);
    }

    #[test]
    fn cycles_show_at_the_top_level() {
        let mut m = apply_all(&[
            op(1, "a", OpKind::Create { entry: Entry::folder("x", "X", None) }),
            op(2, "a", OpKind::Create { entry: Entry::folder("y", "Y", None) }),
            op(3, "a", OpKind::Create { entry: Entry::file("p", "P", Some("y"), cid("p"), 1) }),
        ]);
        // two devices move each folder into the other at once
        m.apply(&op(4, "a", OpKind::Move { id: "x".into(), parent: Some("y".into()) })).unwrap();
        m.apply(&op(4, "b", OpKind::Move { id: "y".into(), parent: Some("x".into()) })).unwrap();
        let top: Vec<String> = m.children(None).into_iter().map(|e| e.id).collect();
        assert_eq!(top, ["x", "y"]);
        assert_eq!(m.path("p"), ["Y", "P"]);
        // a parent that doesn't exist (yet) also shows at the top level
        m.apply(&op(5, "a", OpKind::Move { id: "p".into(), parent: Some("ghost".into()) })).unwrap();
        assert_eq!(m.path("p"), ["P"]);
    }

    #[test]
    fn snapshots_and_op_logs_round_trip_and_merge() {
        let ops = sample_ops();
        let (first, rest) = ops.split_at(5);
        let snap = apply_all(first).to_snapshot().unwrap();
        let mut m = Manifest::from_snapshot(&snap, 1 << 20).unwrap();
        for o in read_ops(&write_ops(rest).unwrap(), 100).unwrap() {
            m.apply(&o).unwrap();
        }
        assert_eq!(m, apply_all(&ops));
        assert!(Manifest::from_snapshot(&snap, 10).is_err());
    }

    #[test]
    fn hostile_ops_and_snapshots_are_refused() {
        let mut m = Manifest::new();
        for bad in [
            op(1, "", OpKind::Delete { id: "p".into() }),
            op(1, "a", OpKind::Delete { id: "../p".into() }),
            op(1, "a", OpKind::Rename { id: "p".into(), name: "a\u{0}b".into() }),
            op(1, "a", OpKind::Rename { id: "p".into(), name: "x".repeat(MAX_NAME + 1) }),
            op(1, "a", OpKind::AddReplica { id: "p".into(), account: "a b".into(), key: "k".into() }),
        ] {
            assert!(m.apply(&bad).is_err(), "{bad:?}");
        }
        for i in 0..MAX_REPLICAS {
            m.apply(&op(1, "a", OpKind::AddReplica { id: "p".into(), account: format!("acct{i}"), key: "k".into() })).unwrap();
        }
        assert!(m.apply(&op(1, "a", OpKind::AddReplica { id: "p".into(), account: "one-more".into(), key: "k".into() })).is_err());
        assert!(read_ops(b"{\"not\":\"an op\"}\n", 10).is_err());
        assert!(read_ops(&write_ops(&sample_ops()).unwrap(), 3).is_err());
        assert!(Manifest::from_snapshot(b"{\"format\":\"other\",\"version\":1,\"manifest\":{\"clock\":0,\"records\":{}}}", 1000).is_err());
        let newer = format!("{{\"format\":\"{FORMAT}\",\"version\":99,\"manifest\":{{\"clock\":0,\"records\":{{}}}}}}");
        assert!(matches!(Manifest::from_snapshot(newer.as_bytes(), 1000), Err(CloudError::Unsupported(_))));
        // a deep chain of folders doesn't overflow anything
        let mut deep = Manifest::new();
        for i in 0..(MAX_DEPTH * 2) {
            let parent = if i == 0 { None } else { Some(format!("d{}", i - 1)) };
            deep.apply(&op(i as u64 + 1, "a", OpKind::Create { entry: Entry::folder(&format!("d{i}"), "D", parent.as_deref()) })).unwrap();
        }
        assert!(deep.path(&format!("d{}", MAX_DEPTH * 2 - 1)).len() <= MAX_DEPTH);
    }
}
