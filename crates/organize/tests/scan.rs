//! Scanning folders without downloading them: camera-like NEF, JPEG, DNG and MP4 files on a
//! folder "account", read through ranged reads only; the cache makes a second scan read nothing;
//! a scan that stops (signed out, cancelled) goes on where it stopped.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use lightcraft_cloud::{Browse, BrowseDir, CloudError, FolderBrowser};
use lightcraft_organize::Kind;
use lightcraft_organize::scan::{Progress, Record, ScanOptions, Stop, scan};

mod files;
use files::{camera_jpeg, camera_mp4, camera_raw, directory_last_dng};

fn temp(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lc-organize-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn put(root: &Path, path: &str, data: &[u8]) {
    let p = root.join(path);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, data).unwrap();
}

/// 2019-07-03T15:00:00 in QuickTime seconds.
const CLIP: u32 = (1_562_166_000i64 + 2_082_844_800) as u32;

fn trips(account: &Path) -> Vec<u8> {
    let nef = camera_raw("NIKON CORPORATION", "NIKON Z 6", "6012345", "2019:07:03 14:22:01", "37", 1_500_000);
    put(account, "Photos A/Iceland 2019/DSC_0001.NEF", &nef);
    put(account, "Photos A/Iceland 2019/DSC_0001.JPG", &camera_jpeg("6012345", "2019:07:03 14:22:01", "37", 400_000));
    put(account, "Photos A/Iceland 2019/clip.MP4", &camera_mp4(CLIP, 2_000_000));
    put(account, "Photos A/Iceland 2019/notes.txt", b"not a photo");
    put(account, "Photos A/.thumbnails/DSC_0001.JPG", b"a system's thumbnail");
    put(account, "Photos B/Iceland/DSC_0001.NEF", &nef);
    put(account, "Photos B/Iceland/Converted.DNG", &directory_last_dng("2019:07:04 09:00:00"));
    nef
}

fn options(account: &Path) -> ScanOptions {
    ScanOptions { roots: vec!["Photos A".into(), "/Photos B/".into()], cache: account.join("scan.jsonl") }
}

fn browsers(account: &Path, n: usize) -> Vec<Box<dyn Browse>> {
    (0..n).map(|_| Box::new(FolderBrowser::new(account).unwrap()) as Box<dyn Browse>).collect()
}

fn find<'a>(records: &'a [Record], root: &str, path: &str) -> &'a Record {
    records.iter().find(|r| r.root == root && r.path == path).unwrap_or_else(|| panic!("{root}/{path} in {records:#?}"))
}

#[test]
fn photos_and_videos_are_read_from_their_first_bytes() {
    let account = temp("read");
    let nef = trips(&account);
    let progress = Progress::default();
    let scanned = scan(browsers(&account, 3), &options(&account), &progress).unwrap();
    assert_eq!(scanned.stop, Stop::Done);
    let r = &scanned.records;
    assert_eq!(r.len(), 5, "photos and videos only, nothing hidden: {r:#?}");
    let a_nef = find(r, "Photos A", "Iceland 2019/DSC_0001.NEF");
    assert_eq!((a_nef.kind, a_nef.taken.as_deref()), (Some(Kind::Nef), Some("2019-07-03T14:22:01.370")));
    assert_eq!(
        (a_nef.make.as_deref(), a_nef.model.as_deref(), a_nef.serial.as_deref()),
        (Some("NIKON CORPORATION"), Some("NIKON Z 6"), Some("6012345"))
    );
    assert_eq!(a_nef.size, nef.len() as u64);
    let jpg = find(r, "Photos A", "Iceland 2019/DSC_0001.JPG");
    assert_eq!((jpg.kind, jpg.taken.as_deref(), jpg.serial.as_deref()), (Some(Kind::Jpeg), Some("2019-07-03T14:22:01.370"), Some("6012345")));
    let clip = find(r, "Photos A", "Iceland 2019/clip.MP4");
    assert_eq!((clip.kind, clip.taken.as_deref()), (Some(Kind::Video), Some("2019-07-03T15:00:00")));
    let dng = find(r, "Photos B", "Iceland/Converted.DNG");
    assert_eq!(dng.taken.as_deref(), Some("2019-07-04T09:00:00"), "a directory after the data is read where it is");
    // the same file in both folders has the same head hash; different files don't
    let b_nef = find(r, "Photos B", "Iceland/DSC_0001.NEF");
    assert_eq!(a_nef.head, b_nef.head);
    assert_ne!(a_nef.head, jpg.head);
    assert!(r.iter().all(|x| x.error.is_none() && x.hash.is_none()), "a folder has no content hash");
    // only the starts of files were read
    let total: u64 = r.iter().map(|x| x.size).sum();
    let read = progress.bytes.load(Ordering::Relaxed);
    assert!(read < total / 3, "read {read} of {total} bytes");
    assert_eq!(progress.found.load(Ordering::Relaxed), 5);

    // a second scan reads nothing; a changed file is read again
    let again = Progress::default();
    let second = scan(browsers(&account, 2), &options(&account), &again).unwrap();
    assert_eq!(second.records, scanned.records);
    assert_eq!((again.to_read.load(Ordering::Relaxed), again.bytes.load(Ordering::Relaxed)), (0, 0));
    put(&account, "Photos B/Iceland/Converted.DNG", &directory_last_dng("2019:07:05 10:00:00"));
    std::fs::remove_file(account.join("Photos A/Iceland 2019/clip.MP4")).unwrap();
    let third = Progress::default();
    let r = scan(browsers(&account, 2), &options(&account), &third).unwrap().records;
    assert_eq!(third.to_read.load(Ordering::Relaxed), 1);
    assert_eq!(r.len(), 4, "a file that went away leaves the scan");
    assert_eq!(find(&r, "Photos B", "Iceland/Converted.DNG").taken.as_deref(), Some("2019-07-05T10:00:00"));
    let cached = std::fs::read_to_string(account.join("scan.jsonl")).unwrap();
    assert_eq!(cached.lines().count(), 4, "the cache holds each file once");
    let _ = std::fs::remove_dir_all(account);
}

/// A folder browser that is signed out after `ok` reads.
struct SignedOut {
    inner: FolderBrowser,
    ok: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

impl Browse for SignedOut {
    fn kind(&self) -> &str {
        "test"
    }
    fn folder(&mut self, path: &str) -> lightcraft_cloud::Result<String> {
        self.inner.folder(path)
    }
    fn list(&mut self, folder: &str) -> lightcraft_cloud::Result<BrowseDir> {
        self.inner.list(folder)
    }
    fn content_hash(&mut self, file: &str) -> lightcraft_cloud::Result<Option<String>> {
        self.inner.content_hash(file)
    }
    fn read_range(&mut self, file: &str, offset: u64, len: u64) -> lightcraft_cloud::Result<Vec<u8>> {
        if self.ok.fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1)).is_err() {
            return Err(CloudError::Denied("sign in again: expired".into()));
        }
        self.inner.read_range(file, offset, len)
    }
}

#[test]
fn a_stopped_scan_goes_on_where_it_stopped() {
    let account = temp("resume");
    trips(&account);
    for i in 0..20 {
        put(&account, &format!("Photos A/Greece 2021/DSC_{i:04}.JPG"), &camera_jpeg("1", "2021:05:01 10:00:00", &format!("{i:02}"), 1000));
    }
    let ok = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(10));
    let one: Vec<Box<dyn Browse>> = vec![Box::new(SignedOut { inner: FolderBrowser::new(&account).unwrap(), ok: ok.clone() })];
    let progress = Progress::default();
    let stopped = scan(one, &options(&account), &progress).unwrap();
    assert!(matches!(&stopped.stop, Stop::Halted(m) if m.contains("sign in again")), "{:?}", stopped.stop);
    assert!(progress.failed.load(Ordering::Relaxed) == 0, "signing out stops the scan; it doesn't fail every file");
    let first = progress.read.load(Ordering::Relaxed);
    assert!(first > 0 && first < 25);
    // signed in again: only what wasn't read is read
    let again = Progress::default();
    let done = scan(browsers(&account, 4), &options(&account), &again).unwrap();
    assert_eq!(done.stop, Stop::Done);
    assert_eq!(done.records.len(), 25);
    assert_eq!(again.to_read.load(Ordering::Relaxed), 25 - first);
    // cancelled before it starts reading
    let cancelled = Progress::default();
    cancelled.cancel.store(true, Ordering::Relaxed);
    assert_eq!(scan(browsers(&account, 1), &options(&account), &cancelled).unwrap().stop, Stop::Cancelled);
    // a folder that isn't there
    let missing = ScanOptions { roots: vec!["Photos C".into()], ..options(&account) };
    let e = scan(browsers(&account, 1), &missing, &Progress::default()).unwrap_err();
    assert!(e.contains("no folder \"Photos C\""), "{e}");
    let _ = std::fs::remove_dir_all(account);
}
