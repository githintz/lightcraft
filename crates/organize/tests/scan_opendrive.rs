//! Scanning OpenDrive folders through the fake OpenDrive of `lightcraft-cloud`'s tests: parallel
//! workers sharing one rotating token, OpenDrive's MD5 for each file, nothing in the folders
//! changed, and the daily download limit stopping a scan that goes on the next day.

#![cfg(not(target_arch = "wasm32"))]

use std::sync::atomic::Ordering;

use lightcraft_cloud::opendrive::{OpenDriveProvider, login};
use lightcraft_cloud::{Browse, Net};
use lightcraft_organize::scan::{Progress, ScanOptions, Stop, scan};

#[path = "../../cloud/tests/common/mod.rs"]
mod common;
mod files;
use common::opendrive::{PASS, USER, start};

#[test]
fn opendrive_folders_are_scanned_in_parallel_and_left_as_they_were() {
    let (state, api) = start();
    let nef = files::camera_raw("NIKON CORPORATION", "NIKON D850", "3001", "2018:02:10 08:00:00", "5", 900_000);
    {
        let mut s = state.lock().unwrap();
        for i in 0..12 {
            s.add_file(
                &format!("Photos A/Japan 2018/DSC_{i:04}.JPG"),
                &files::camera_jpeg("3001", "2018:02:10 09:00:00", &format!("{i:02}"), 30_000),
            );
        }
        s.add_file("Photos A/Japan 2018/DSC_0100.NEF", &nef);
        s.add_file("Photos B/Japan/DSC_0100.NEF", &nef);
        s.add_file("Photos B/Japan/clip.MOV", &files::camera_mp4(3_600_000_000, 500_000));
    }
    let before: Vec<(String, usize)> = state.lock().unwrap().files.values().map(|f| (f.name.clone(), f.data.len())).collect();
    let config = login(Some(&api), USER, PASS, &Net::default()).unwrap();
    let main = OpenDriveProvider::new(config, "OpenDrive").unwrap().into_browser();
    let workers =
        |main: &lightcraft_cloud::opendrive::OpenDriveBrowser| (0..4).map(|_| Box::new(main.worker()) as Box<dyn Browse>).collect::<Vec<_>>();
    let dir = std::env::temp_dir().join(format!("lc-organize-od-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let opts = ScanOptions { roots: vec!["Photos A".into(), "Photos B".into()], cache: dir.join("scan.jsonl") };

    // the daily limit runs out part way: the scan stops, and says why
    state.lock().unwrap().bandwidth_left = Some(1_500_000);
    let progress = Progress::default();
    let stopped = scan(workers(&main), &opts, &progress).unwrap();
    assert!(matches!(&stopped.stop, Stop::Halted(m) if m.contains("bandwidth")), "{:?}", stopped.stop);
    let first = progress.read.load(Ordering::Relaxed);
    assert!(first < 15, "{first}");

    // the next day
    state.lock().unwrap().bandwidth_left = None;
    let progress = Progress::default();
    let done = scan(workers(&main), &opts, &progress).unwrap();
    assert_eq!(done.stop, Stop::Done);
    assert_eq!(done.records.len(), 15);
    assert_eq!(progress.to_read.load(Ordering::Relaxed), 15 - first);
    let nefs: Vec<_> = done.records.iter().filter(|r| r.path.ends_with("DSC_0100.NEF")).collect();
    assert_eq!(nefs.len(), 2);
    assert!(nefs[0].hash.is_some() && nefs[0].hash == nefs[1].hash, "OpenDrive's MD5 tells identical files");
    assert_eq!(nefs[0].taken.as_deref(), Some("2018-02-10T08:00:00.500"));
    let clip = done.records.iter().find(|r| r.path.ends_with("clip.MOV")).unwrap();
    assert!(clip.taken.is_some(), "{clip:?}");
    // the folders are as they were, and the token the browsers hold still works
    let after: Vec<(String, usize)> = state.lock().unwrap().files.values().map(|f| (f.name.clone(), f.data.len())).collect();
    assert_eq!(before, after);
    assert_eq!(main.refresh_token(), state.lock().unwrap().refresh);
    let _ = std::fs::remove_dir_all(dir);
}
