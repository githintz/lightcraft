//! Plan albums from a scan: one album per trip, the copies of a file counted once, and what
//! needs the user's eye (likely duplicates, trips that may be one) listed for review. Pure and
//! deterministic: the same scan and edits give the same plan.
//!
//! - **Albums.** A file's trip is the folder it is in, without the subfolders a trip is split
//!   into (`RAW`, `JPG`, `Day 3`, `2019-07-04`, a camera's `100NCZ_6`…). Folders from different
//!   roots whose names hold the same words (`Iceland 2019`, `2019/Iceland`) are one album.
//! - **Identical files** (same provider hash and size; without a hash: same size, same first
//!   64 KiB, same name) are one photo: it goes in the album of the copy in the bigger album.
//! - **Likely duplicates**: different bytes but the same shot (same capture time to the
//!   millisecond, same camera, same pixel size, both raw or both not): an edited or re-saved copy.
//!   Both stay in the plan; the report lists them for review. A raw and a JPEG of one shot are a
//!   pair, not duplicates.
//! - **Suggestions**: albums that may be one trip (their dates overlap, or they share identical
//!   files). Never applied on their own: [`Edits::merge`] applies them.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use lightcraft_meta::DateTime;
use serde::{Deserialize, Serialize};

use crate::Kind;
use crate::scan::Record;

pub const FORMAT: &str = "lightcraft-organize-plan";

/// A file, by where it is.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct FileRef {
    pub root: String,
    pub path: String,
    pub id: String,
}

impl FileRef {
    fn of(r: &Record) -> FileRef {
        FileRef { root: r.root.clone(), path: r.path.clone(), id: r.id.clone() }
    }

    /// `root/path`, as the user knows it.
    pub fn full(&self) -> String {
        format!("{}/{}", self.root, self.path)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Album {
    /// What identifies it in [`Edits`]: the words of its folder names, sorted (`2019 iceland`).
    pub key: String,
    pub name: String,
    /// The folders it was made from (`Photos A/Iceland 2019`).
    pub sources: Vec<String>,
    pub files: Vec<FileRef>,
    /// The first and last capture time of its photos and videos.
    pub start: Option<String>,
    pub end: Option<String>,
    pub photos: usize,
    pub videos: usize,
    pub bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Sameness {
    /// The provider's hash and the size are equal.
    Identical,
    /// No hash to compare: the same size, first 64 KiB and name.
    SameStart,
    /// Different bytes, the same shot (capture time, camera, pixel size).
    LikelySameShot,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Duplicates {
    pub sameness: Sameness,
    /// The copy in the plan (for likely duplicates, the biggest; every copy stays in the plan).
    pub keep: FileRef,
    pub others: Vec<FileRef>,
    pub size: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Suggestion {
    /// Album keys.
    pub merge: [String; 2],
    pub reason: String,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    pub files: usize,
    pub photos: usize,
    pub videos: usize,
    pub bytes: u64,
    /// Copies left out because an identical file is in the plan, and their bytes.
    pub duplicate_copies: usize,
    pub duplicate_bytes: u64,
    pub likely_duplicates: usize,
    /// Raw + JPEG pairs of one shot.
    pub raw_jpeg_pairs: usize,
    pub unreadable: usize,
    /// Photos with no capture time (their album is their folder's).
    pub undated: usize,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub format: String,
    pub roots: Vec<String>,
    pub albums: Vec<Album>,
    pub duplicates: Vec<Duplicates>,
    pub suggestions: Vec<Suggestion>,
    /// Files whose reading failed: in their folder's album, but not checked for duplicates.
    pub unreadable: Vec<(FileRef, String)>,
    pub stats: Stats,
}

/// The user's decisions after reviewing a plan.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct Edits {
    /// Albums to make one (keys or names; the first keeps its name).
    pub merge: Vec<Vec<String>>,
    /// `(key or name, new name)`.
    pub rename: Vec<(String, String)>,
    /// Files (`root/path`) to keep even when identical to another (both copies stay).
    pub keep_both: Vec<String>,
}

/// Folder names that split a trip rather than name one.
const PARTS: &[&str] = &[
    "raw",
    "raws",
    "nef",
    "nefs",
    "dng",
    "dngs",
    "jpg",
    "jpgs",
    "jpeg",
    "jpegs",
    "video",
    "videos",
    "movie",
    "movies",
    "clips",
    "mov",
    "mp4",
    "edited",
    "edits",
    "edit",
    "export",
    "exports",
    "exported",
    "originals",
    "original",
    "selects",
    "selected",
    "picks",
    "best",
    "favorites",
    "favourites",
    "dcim",
    "camera",
    "phone",
    "iphone",
    "drone",
    "gopro",
    "nikon",
    "sd",
    "card",
];

/// Whether a folder name splits a trip (a format, a day, a date, a camera's DCIM folder).
fn is_part(name: &str) -> bool {
    let n = norm_words(name);
    if n.is_empty() || PARTS.contains(&n.join(" ").as_str()) {
        return true;
    }
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    match n.as_slice() {
        // "Day 3", "D3", "Part 2", "3"
        [w, d] if (w == "day" || w == "part" || w == "card") && digits(d) => true,
        [w] if digits(w) && w.len() <= 2 => true,
        [w] if w.len() > 1 && w.strip_prefix('d').is_some_and(digits) => true,
        // dates: 2019-07-04, 20190704, 2019 07 04
        [y, m, d] if digits(y) && y.len() == 4 && digits(m) && digits(d) => true,
        [w] if w.len() == 8 && digits(w) => true,
        // a camera's DCIM folder: 100NCZ_6, 101ND850, 100CANON
        _ => {
            let raw = name.trim();
            raw.len() == 8 && raw.get(..3).is_some_and(digits) && raw.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        }
    }
}

/// Lower-case words of a folder name, accents removed, separators (`_ - .`) as spaces.
fn norm_words(s: &str) -> Vec<String> {
    let folded: String = s
        .chars()
        .flat_map(char::to_lowercase)
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' => 'a',
            'ç' | 'č' | 'ć' => 'c',
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ě' => 'e',
            'ì' | 'í' | 'î' | 'ï' | 'ī' => 'i',
            'ñ' | 'ń' | 'ň' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => 'o',
            'ù' | 'ú' | 'û' | 'ü' | 'ū' | 'ů' => 'u',
            'ý' | 'ÿ' => 'y',
            'š' | 'ś' => 's',
            'ž' | 'ź' | 'ż' => 'z',
            'ř' => 'r',
            'ł' => 'l',
            c if c.is_alphanumeric() => c,
            _ => ' ',
        })
        .collect();
    folded.split_whitespace().map(str::to_string).collect()
}

/// The trip folders of a file's folder (`Iceland 2019/Day 1/RAW` → `[Iceland 2019]`); the
/// folder itself when every part of it splits a trip.
fn trip_dirs(dir: &str) -> Vec<&str> {
    let segs: Vec<&str> = dir.split('/').filter(|s| !s.is_empty()).collect();
    let mut keep = segs.len();
    while keep > 1 && segs.get(keep - 1).is_some_and(|s| is_part(s)) {
        keep -= 1;
    }
    segs.get(..keep).map(<[&str]>::to_vec).unwrap_or_default()
}

/// An album's key: the sorted, distinct words of its trip folders.
fn key_of(dirs: &[&str]) -> String {
    let words: BTreeSet<String> = dirs.iter().flat_map(|d| norm_words(d)).collect();
    words.into_iter().collect::<Vec<_>>().join(" ")
}

fn seconds(iso: &str) -> Option<i64> {
    DateTime::parse_iso(iso).map(|d| d.unix_seconds())
}

/// The day part of an ISO time, for messages.
fn day(iso: &str) -> &str {
    iso.get(..10).unwrap_or(iso)
}

/// The base name without extension, lower-cased (a raw and its JPEG share it).
fn stem(name: &str) -> String {
    name.rsplit_once('.').map_or(name, |(s, _)| s).to_lowercase()
}

/// Plan albums for `records` (a finished scan of `roots`), with the user's `edits`.
pub fn plan(roots: &[String], records: &[Record], edits: &Edits) -> Plan {
    let mut stats = Stats::default();
    let keep_both: BTreeSet<&str> = edits.keep_both.iter().map(String::as_str).collect();
    let media: Vec<&Record> = records.iter().filter(|r| r.kind.is_some_and(|k| k != Kind::Sidecar)).collect();

    // albums by trip folder
    let mut album_of: HashMap<(&str, &str), String> = HashMap::new();
    let mut albums: BTreeMap<String, Album> = BTreeMap::new();
    let mut names: BTreeMap<String, BTreeMap<String, usize>> = BTreeMap::new();
    for r in &media {
        let dirs = trip_dirs(r.dir());
        let (key, name, source) = if dirs.is_empty() {
            let key = format!("unsorted {}", norm_words(&r.root).join(" "));
            (key, format!("Unsorted ({})", r.root), r.root.clone())
        } else {
            (key_of(&dirs), dirs.join(" / "), format!("{}/{}", r.root, dirs.join("/")))
        };
        let a = albums.entry(key.clone()).or_insert_with(|| Album {
            key: key.clone(),
            name: String::new(),
            sources: Vec::new(),
            files: Vec::new(),
            start: None,
            end: None,
            photos: 0,
            videos: 0,
            bytes: 0,
        });
        if !a.sources.contains(&source) {
            a.sources.push(source);
        }
        *names.entry(key.clone()).or_default().entry(name).or_default() += 1;
        album_of.insert((r.root.as_str(), r.path.as_str()), key);
    }
    // an album is named as most of its files' folders are; on a tie, a single folder's name
    // ("Japan 2018" over "2018 / Japan"), then the first alphabetically
    for (key, counts) in &names {
        let best = counts.iter().max_by(|x, y| x.1.cmp(y.1).then_with(|| y.0.contains(" / ").cmp(&x.0.contains(" / "))).then_with(|| y.0.cmp(x.0)));
        if let (Some(a), Some((name, _))) = (albums.get_mut(key), best) {
            a.name = name.clone();
        }
    }
    let album_size = |root: &str, path: &str, albums: &BTreeMap<String, Album>, counts: &BTreeMap<String, BTreeMap<String, usize>>| {
        album_of.get(&(root, path)).and_then(|k| albums.get(k).map(|_| counts.get(k).map_or(0, |c| c.values().sum::<usize>()))).unwrap_or(0)
    };

    // identical files
    let mut groups: BTreeMap<(u64, String), Vec<&Record>> = BTreeMap::new();
    for r in &media {
        if r.error.is_some() {
            continue;
        }
        let id = match (&r.hash, &r.head) {
            (Some(h), _) => format!("hash:{h}"),
            (None, Some(head)) => format!("start:{head}:{}", r.name().to_lowercase()),
            (None, None) => continue,
        };
        groups.entry((r.size, id)).or_default().push(r);
    }
    let mut left_out: BTreeSet<(&str, &str)> = BTreeSet::new();
    let mut duplicates = Vec::new();
    let mut shared: BTreeMap<(String, String), usize> = BTreeMap::new();
    for ((size, id), mut copies) in groups {
        if copies.len() < 2 {
            continue;
        }
        // the copy in the bigger album stays; then the first root, then the path
        copies.sort_by(|a, b| {
            album_size(&b.root, &b.path, &albums, &names)
                .cmp(&album_size(&a.root, &a.path, &albums, &names))
                .then_with(|| roots.iter().position(|x| *x == a.root).cmp(&roots.iter().position(|x| *x == b.root)))
                .then_with(|| a.path.cmp(&b.path))
        });
        let Some((keep, others)) = copies.split_first() else { continue };
        let mut out = Vec::new();
        for o in others {
            if keep_both.contains(format!("{}/{}", o.root, o.path).as_str()) {
                continue;
            }
            left_out.insert((o.root.as_str(), o.path.as_str()));
            stats.duplicate_copies += 1;
            stats.duplicate_bytes += size;
            if let (Some(a), Some(b)) = (album_of.get(&(keep.root.as_str(), keep.path.as_str())), album_of.get(&(o.root.as_str(), o.path.as_str())))
                && a != b
            {
                let pair = if a < b { (a.clone(), b.clone()) } else { (b.clone(), a.clone()) };
                *shared.entry(pair).or_default() += 1;
            }
            out.push(FileRef::of(o));
        }
        if !out.is_empty() {
            let sameness = if id.starts_with("hash:") { Sameness::Identical } else { Sameness::SameStart };
            duplicates.push(Duplicates { sameness, keep: FileRef::of(keep), others: out, size });
        }
    }

    // likely duplicates: the same shot, different bytes
    let mut shots: BTreeMap<String, Vec<&Record>> = BTreeMap::new();
    for r in &media {
        let (Some(kind), Some(taken)) = (r.kind, &r.taken) else { continue };
        if !kind.is_photo() || left_out.contains(&(r.root.as_str(), r.path.as_str())) {
            continue;
        }
        // without milliseconds, two frames of a burst share a time: the name must match too
        let precise = taken.contains('.');
        let shot = format!(
            "{taken}|{}|{}|{}|{:?}x{:?}|{}|{}",
            r.make.as_deref().unwrap_or(""),
            r.model.as_deref().unwrap_or(""),
            r.serial.as_deref().unwrap_or(""),
            r.width,
            r.height,
            kind.is_raw(),
            if precise { String::new() } else { stem(r.name()) }
        );
        shots.entry(shot).or_default().push(r);
    }
    for mut same in shots.into_values().filter(|v| v.len() > 1) {
        same.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| (&a.root, &a.path).cmp(&(&b.root, &b.path))));
        let Some((keep, others)) = same.split_first() else { continue };
        stats.likely_duplicates += others.len();
        duplicates.push(Duplicates {
            sameness: Sameness::LikelySameShot,
            keep: FileRef::of(keep),
            others: others.iter().map(|r| FileRef::of(r)).collect(),
            size: keep.size,
        });
    }

    // raw + JPEG pairs (same trip, same base name: often in RAW / JPG subfolders)
    let mut by_stem: HashMap<(&str, String), (bool, bool)> = HashMap::new();
    for r in &media {
        let Some(album) = album_of.get(&(r.root.as_str(), r.path.as_str())) else { continue };
        let e = by_stem.entry((album.as_str(), stem(r.name()))).or_default();
        match r.kind {
            Some(k) if k.is_raw() => e.0 = true,
            Some(Kind::Jpeg) => e.1 = true,
            _ => {}
        }
    }
    stats.raw_jpeg_pairs = by_stem.values().filter(|(raw, jpg)| *raw && *jpg).count();

    // fill the albums
    let mut unreadable = Vec::new();
    for r in &media {
        stats.files += 1;
        stats.bytes += r.size;
        if let Some(e) = &r.error {
            stats.unreadable += 1;
            unreadable.push((FileRef::of(r), e.clone()));
        }
        if left_out.contains(&(r.root.as_str(), r.path.as_str())) {
            continue;
        }
        let Some(a) = album_of.get(&(r.root.as_str(), r.path.as_str())).and_then(|k| albums.get_mut(k)) else { continue };
        let kind = r.kind.unwrap_or(Kind::Image);
        if kind.is_video() {
            a.videos += 1;
            stats.videos += 1;
        } else {
            a.photos += 1;
            stats.photos += 1;
            if r.taken.is_none() {
                stats.undated += 1;
            }
        }
        a.bytes += r.size;
        if let Some(t) = &r.taken {
            if a.start.as_ref().is_none_or(|s| t < s) {
                a.start = Some(t.clone());
            }
            if a.end.as_ref().is_none_or(|e| t > e) {
                a.end = Some(t.clone());
            }
        }
        a.files.push(FileRef::of(r));
    }

    // the user's merges and renames
    let find = |albums: &BTreeMap<String, Album>, k: &str| -> Option<String> {
        if albums.contains_key(k) {
            return Some(k.to_string());
        }
        albums.values().find(|a| a.name.eq_ignore_ascii_case(k)).map(|a| a.key.clone())
    };
    for group in &edits.merge {
        let keys: Vec<String> = group.iter().filter_map(|k| find(&albums, k)).collect::<BTreeSet<_>>().into_iter().collect();
        let Some(first) = group.iter().find_map(|k| find(&albums, k)) else { continue };
        for k in keys.into_iter().filter(|k| *k != first) {
            let Some(other) = albums.remove(&k) else { continue };
            if let Some(a) = albums.get_mut(&first) {
                a.sources.extend(other.sources);
                a.files.extend(other.files);
                a.photos += other.photos;
                a.videos += other.videos;
                a.bytes += other.bytes;
                a.start = [a.start.take(), other.start].into_iter().flatten().min();
                a.end = [a.end.take(), other.end].into_iter().flatten().max();
            }
        }
    }
    for (k, name) in &edits.rename {
        if let Some(a) = find(&albums, k).and_then(|k| albums.get_mut(&k)) {
            a.name = name.trim().to_string();
        }
    }

    // suggestions: albums that may be one trip
    let mut suggestions = Vec::new();
    for ((a, b), n) in &shared {
        if albums.contains_key(a) && albums.contains_key(b) {
            suggestions.push(Suggestion { merge: [a.clone(), b.clone()], reason: format!("{n} identical file(s) are in both") });
        }
    }
    let list: Vec<&Album> = albums.values().collect();
    for (i, x) in list.iter().enumerate() {
        for y in list.iter().skip(i + 1) {
            if suggestions.iter().any(|s| s.merge == [x.key.clone(), y.key.clone()]) {
                continue;
            }
            // a trip covers the whole of its last day (a one-photo album has a span too)
            let span = |a: &Album| Some((seconds(a.start.as_deref()?)?, seconds(a.end.as_deref()?)? + 86_400));
            let (Some((xs, xe)), Some((ys, ye))) = (span(x), span(y)) else { continue };
            let overlap = xe.min(ye) - xs.max(ys);
            // a day at least, so one-day trips can overlap
            let shorter = (xe - xs).min(ye - ys).max(86_400);
            if overlap > 0 && overlap * 2 >= shorter {
                let (xw, yw): (BTreeSet<&str>, BTreeSet<&str>) = (x.key.split(' ').collect(), y.key.split(' ').collect());
                let common: Vec<&str> = xw.intersection(&yw).copied().collect();
                let same_words = if common.is_empty() { String::new() } else { format!("; both named “{}”", common.join(" ")) };
                let reason = format!(
                    "dates overlap ({} to {}, and {} to {}){same_words}",
                    day(x.start.as_deref().unwrap_or("")),
                    day(x.end.as_deref().unwrap_or("")),
                    day(y.start.as_deref().unwrap_or("")),
                    day(y.end.as_deref().unwrap_or(""))
                );
                suggestions.push(Suggestion { merge: [x.key.clone(), y.key.clone()], reason });
            }
        }
    }

    let mut albums: Vec<Album> = albums.into_values().filter(|a| !a.files.is_empty()).collect();
    for a in &mut albums {
        a.files.sort();
        a.sources.sort();
    }
    albums.sort_by(|a, b| a.start.is_none().cmp(&b.start.is_none()).then_with(|| a.start.cmp(&b.start)).then_with(|| a.name.cmp(&b.name)));
    duplicates.sort_by(|a, b| a.sameness.cmp(&b.sameness).then_with(|| a.keep.cmp(&b.keep)));
    Plan { format: FORMAT.into(), roots: roots.to_vec(), albums, duplicates, suggestions, unreadable, stats }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trip_folders() {
        assert_eq!(trip_dirs("Iceland 2019/Day 1/RAW"), ["Iceland 2019"]);
        assert_eq!(trip_dirs("2019/Iceland/2019-07-04"), ["2019", "Iceland"]);
        assert_eq!(trip_dirs("Japan 2018/DCIM/100NCZ_6"), ["Japan 2018"]);
        assert_eq!(trip_dirs("RAW"), ["RAW"], "a part with nothing above it is the trip");
        assert!(trip_dirs("").is_empty());
        assert_eq!(key_of(&["Iceland 2019"]), key_of(&["2019", "Iceland"]));
        assert_eq!(key_of(&["Île-de-France_2020"]), "2020 de france ile");
        for part in ["raw", "JPG", "Day 12", "d3", "07", "2019-07-04", "20190704", "100ND850", "Edited", "Videos"] {
            assert!(is_part(part), "{part}");
        }
        for trip in ["Iceland", "Iceland 2019", "Paris", "Day trip to Bath", "2019"] {
            assert!(!is_part(trip), "{trip}");
        }
    }
}
