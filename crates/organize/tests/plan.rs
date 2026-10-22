//! Planning albums for two photo trees that overlap: trips in one tree or the other, split
//! between them, copied into both; identical and likely duplicates; raw + JPEG pairs; the user's
//! merges, renames and "keep both".

use lightcraft_organize::Kind;
use lightcraft_organize::plan::{Edits, Plan, Sameness, plan};
use lightcraft_organize::scan::Record;

fn rec(root: &str, path: &str, size: u64, hash: &str, taken: Option<&str>) -> Record {
    Record {
        root: root.into(),
        path: path.into(),
        id: format!("{root}/{path}"),
        size,
        kind: Kind::of(path),
        hash: Some(hash.into()),
        head: Some(format!("h-{hash}")),
        taken: taken.map(str::to_string),
        make: Some("NIKON CORPORATION".into()),
        model: Some("NIKON Z 6".into()),
        serial: Some("6012345".into()),
        width: Some(6048),
        height: Some(4024),
        ..Record::default()
    }
}

fn roots() -> Vec<String> {
    vec!["Photos A".into(), "Photos B".into()]
}

/// Two trees as they grew over the years.
fn records() -> Vec<Record> {
    let mut r = vec![
        // Iceland: only in A, split into days and formats
        rec("Photos A", "Iceland 2019/Day 1/RAW/DSC_0001.NEF", 30_000_000, "n1", Some("2019-07-03T10:00:00.120")),
        rec("Photos A", "Iceland 2019/Day 1/JPG/DSC_0001.JPG", 8_000_000, "j1", Some("2019-07-03T10:00:00.120")),
        rec("Photos A", "Iceland 2019/Day 2/RAW/DSC_0002.NEF", 30_000_001, "n2", Some("2019-07-04T11:00:00.500")),
        rec("Photos A", "Iceland 2019/Day 2/clip.MP4", 900_000_000, "v1", Some("2019-07-04T12:00:00")),
        // Japan: split between the trees, with a year folder in B
        rec("Photos A", "Japan 2018/DSC_1000.NEF", 31_000_000, "n10", Some("2018-02-10T08:00:00.000")),
        rec("Photos B", "2018/Japan/DSC_1001.NEF", 31_000_001, "n11", Some("2018-02-12T09:00:00.000")),
        // Greece: fully copied into both trees (identical files), plus one re-saved JPEG in B
        rec("Photos A", "Greece 2021/DSC_2000.JPG", 9_000_000, "g1", Some("2021-05-01T10:00:00.250")),
        rec("Photos A", "Greece 2021/DSC_2001.JPG", 9_000_001, "g2", Some("2021-05-01T10:05:00.250")),
        rec("Photos A", "Greece 2021/DSC_2002.JPG", 9_000_002, "g3", Some("2021-05-02T10:00:00.250")),
        rec("Photos B", "Greece 2021/DSC_2000.JPG", 9_000_000, "g1", Some("2021-05-01T10:00:00.250")),
        rec("Photos B", "Greece 2021/DSC_2001.JPG", 9_000_001, "g2", Some("2021-05-01T10:05:00.250")),
        rec("Photos B", "Greece 2021/Edited/DSC_2002.JPG", 7_500_000, "g3-resaved", Some("2021-05-02T10:00:00.250")),
        // the same Portugal trip, named differently in each tree
        rec("Photos A", "Portugal/DSC_3000.NEF", 30_000_000, "p1", Some("2022-09-10T10:00:00.100")),
        rec("Photos A", "Portugal/DSC_3001.NEF", 30_000_000, "p2", Some("2022-09-14T10:00:00.100")),
        rec("Photos B", "Lisbon and Porto 22/DSC_3002.NEF", 30_000_000, "p3", Some("2022-09-12T10:00:00.100")),
        // only in B, and a loose file at B's top
        rec("Photos B", "Scotland 2016/IMG_0001.DNG", 20_000_000, "s1", Some("2016-08-01T10:00:00")),
        rec("Photos B", "IMG_9999.JPG", 3_000_000, "loose", None),
    ];
    r.push(Record { error: Some("could not connect".into()), ..rec("Photos B", "Scotland 2016/IMG_0002.DNG", 20_000_000, "s2", None) });
    r.push(Record { kind: Some(Kind::Sidecar), ..rec("Photos A", "Iceland 2019/Day 1/RAW/DSC_0001.xmp", 2_000, "x", None) });
    r
}

fn album<'a>(p: &'a Plan, name: &str) -> &'a lightcraft_organize::plan::Album {
    p.albums
        .iter()
        .find(|a| a.name == name)
        .unwrap_or_else(|| panic!("no album {name:?} in {:?}", p.albums.iter().map(|a| &a.name).collect::<Vec<_>>()))
}

#[test]
fn trips_become_albums_across_both_trees() {
    let p = plan(&roots(), &records(), &Edits::default());
    let names: Vec<&str> = p.albums.iter().map(|a| a.name.as_str()).collect();
    // in time order, the undated last
    assert_eq!(names, ["Scotland 2016", "Japan 2018", "Iceland 2019", "Greece 2021", "Portugal", "Lisbon and Porto 22", "Unsorted (Photos B)"]);

    let iceland = album(&p, "Iceland 2019");
    assert_eq!((iceland.photos, iceland.videos), (3, 1), "days and formats are one trip; the sidecar isn't a photo");
    assert_eq!((iceland.start.as_deref(), iceland.end.as_deref()), (Some("2019-07-03T10:00:00.120"), Some("2019-07-04T12:00:00")));
    assert_eq!(iceland.sources, ["Photos A/Iceland 2019"]);

    let japan = album(&p, "Japan 2018");
    assert_eq!(japan.files.len(), 2, "split between the trees, one album");
    assert_eq!(japan.sources, ["Photos A/Japan 2018", "Photos B/2018/Japan"]);

    // Greece: each identical file once; the re-saved JPEG stays, listed for review
    let greece = album(&p, "Greece 2021");
    assert_eq!(greece.photos, 4);
    assert!(greece.files.iter().all(|f| f.root == "Photos A" || f.path.contains("Edited")), "{:?}", greece.files);
    let identical: Vec<_> = p.duplicates.iter().filter(|d| d.sameness == Sameness::Identical).collect();
    assert_eq!(identical.len(), 2);
    assert!(identical.iter().all(|d| d.keep.root == "Photos A" && d.others[0].root == "Photos B"));
    let likely: Vec<_> = p.duplicates.iter().filter(|d| d.sameness == Sameness::LikelySameShot).collect();
    assert_eq!(likely.len(), 1);
    assert_eq!((likely[0].keep.path.as_str(), likely[0].others[0].path.as_str()), ("Greece 2021/DSC_2002.JPG", "Greece 2021/Edited/DSC_2002.JPG"));

    // a raw and its JPEG are a pair, not duplicates
    assert_eq!(p.stats.raw_jpeg_pairs, 1, "Iceland's pair, in separate RAW / JPG folders");
    assert!(!p.duplicates.iter().any(|d| d.keep.path.ends_with("DSC_0001.NEF") || d.keep.path.ends_with("DSC_0001.JPG")));

    // Portugal and "Lisbon and Porto 22" overlap in time: suggested, not merged
    assert!(p.suggestions.iter().any(|s| s.merge.contains(&album(&p, "Portugal").key) && s.reason.contains("dates overlap")), "{:?}", p.suggestions);
    assert!(!p.suggestions.iter().any(|s| s.merge.contains(&iceland.key)));

    assert_eq!(p.stats.duplicate_copies, 2);
    assert_eq!(p.stats.duplicate_bytes, 18_000_001);
    assert_eq!((p.stats.unreadable, p.unreadable[0].0.path.as_str()), (1, "Scotland 2016/IMG_0002.DNG"));
    assert_eq!(album(&p, "Scotland 2016").photos, 2, "an unreadable file stays in its folder's album");
    assert_eq!(p.stats.undated, 2);
    assert_eq!(p.stats.files, 18, "sidecars aren't counted");

    // the same input, the same plan
    assert_eq!(plan(&roots(), &records(), &Edits::default()), p);
    let html = lightcraft_organize::report::html(&p);
    assert!(html.contains("Lisbon and Porto 22") && html.contains("May be one trip (1)"));
}

#[test]
fn the_users_edits_are_applied() {
    let base = plan(&roots(), &records(), &Edits::default());
    let lisbon = album(&base, "Lisbon and Porto 22").key.clone();
    let edits = Edits {
        merge: vec![vec!["Portugal".into(), lisbon]],
        rename: vec![("portugal".into(), "Portugal, September 2022".into())],
        keep_both: vec!["Photos B/Greece 2021/DSC_2000.JPG".into()],
    };
    let p = plan(&roots(), &records(), &edits);
    let pt = album(&p, "Portugal, September 2022");
    assert_eq!(pt.photos, 3);
    assert_eq!(pt.sources.len(), 2);
    assert!(!p.albums.iter().any(|a| a.name == "Lisbon and Porto 22"));
    assert!(p.suggestions.is_empty(), "a merged suggestion is gone: {:?}", p.suggestions);
    assert_eq!(album(&p, "Greece 2021").photos, 5, "the copy kept on purpose is in the album too");
    assert_eq!(p.stats.duplicate_copies, 1);
    // edits naming albums that don't exist change nothing
    let none = Edits { merge: vec![vec!["Atlantis".into(), "Mars".into()]], rename: vec![("Atlantis".into(), "x".into())], keep_both: vec![] };
    assert_eq!(plan(&roots(), &records(), &none), base);
}
