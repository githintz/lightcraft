# Albums from photos already in a storage account

**Status:** step 1 of 2 done: scan the user's own folders and plan albums (commands, CLI, MCP). Step 2, LightCraft
albums whose photos stay where they are in the account, is next (see the end).

Many people already keep their photos in a cloud drive, in folders by trip, often in more than one tree that grew over
the years: some trips in one, some in the other, some split between them, some copied into both. This feature turns
those folders into albums **without changing them** and without downloading them.

## How it works

1. **Scan** (`cloud.scanFolders`): every photo and video under the chosen folders is listed, and for each file
   LightCraft reads
   - the provider's content hash (OpenDrive keeps an MD5 of every file: no download),
   - the first 256 KB of a photo (JPEG, NEF, DNG and other raws keep their metadata there: capture time to the
     millisecond, camera, serial number, pixel size). A TIFF-based file whose tag directory comes after its data
     (some converters write DNGs so) costs a second 256 KB read where the header says the directory is,
   - for MP4 / MOV: the box headers and the start of the movie header (`moov/mvhd`, often at the end of the file):
     a few small reads. Other raw video (Nikon N-RAW `.nev`, R3D, BRAW…) is identified by hash and size only.

   Workers read in parallel (6 by default, `workers` up to 16), sharing one sign-in (OpenDrive replaces its refresh
   token on every refresh, so they refresh through one source; the vault keeps the newest). What the scan learns is
   written as it goes to `<library>/cloud/organize/scan.jsonl`, so a scan that stops (cancelled, the computer asleep,
   **OpenDrive's daily download limit**, a sign-in that ran out) goes on where it stopped, and a later scan reads only
   new or changed files. The source folders are read through `lightcraft_cloud::Browse`, which has no call that changes
   anything.

2. **Plan** (`cloud.planAlbums`): offline and deterministic, from the scan.
   - **Albums by trip.** A file's trip is its folder without the subfolders a trip is split into (`RAW`, `JPG`,
     `Edited`, `Day 3`, `D2`, `2019-07-04`, a camera's `100NCZ_6`…). Folders whose names hold the same words are one
     album, across both trees: `Photos A/Iceland 2019` and `Photos B/2019/Iceland` are one "Iceland 2019". Files at a
     tree's top go to "Unsorted (…)".
   - **Identical files** (same provider hash and size; without a hash, as for a folder on this computer: same size,
     same first 64 KiB and same name) are one photo, placed in the bigger album's copy. The others are left out and
     listed.
   - **Likely duplicates**: different bytes, the same shot (same capture time to the millisecond, same camera and
     serial, same pixel size, both raw or both not; without milliseconds the file name must match too, so two frames of
     a burst aren't taken for one): an edited or re-saved copy. Every copy stays in the plan; the report lists them.
   - A raw and a JPEG of one shot are a **pair**, not duplicates.
   - **Suggestions**: albums that may be one trip because their dates overlap (by half the shorter trip) or they share
     identical files. Never applied on their own.

   It writes `plan.json` and `plan.html` (the report to review) next to the scan.

3. **Review and edit.** Merges, renames and copies to keep go in `edits.json` (or the `edits` parameter, which is
   saved there), and the plan is made again:

   ```json
   {
     "merge": [["Portugal", "Lisbon and Porto 22"]],
     "rename": [["Iceland 2019", "Iceland, July 2019"]],
     "keepBoth": ["Photos B/Greece 2021/DSC_2000.JPG"]
   }
   ```

   Albums are named by their name or the key the report shows; the first album of a merge keeps its name.

## Running it

From the CLI (the library's cloud vault must be unlocked to use an OpenDrive account):

```sh
lightcraft-cli run --library ~/Pictures/LightCraft \
  cloud.unlock passphrase='…' \
  cloud.scanFolders account=acc-… roots='["Photos A","Photos B"]' wait=true \
  cloud.planAlbums
# then open <library>/cloud/organize/plan.html
```

Without `wait` the scan runs on worker threads; `cloud.scanProgress` reports it and `cloud.scanCancel` stops it. A
folder on this computer works the same way with `folder=/path/to/drive` instead of `account` (a disk, a NAS share, a
sync client's folder). The account id is in `cloud.status`.

## Costs

- **Requests:** one listing per folder; per file one hash request and one ranged read (two for a few DNGs, a few small
  ones per MP4 / MOV).
- **Downloads:** about 256 KB per photo, 64 KB plus a few small reads per video. 240,000 photos are about 60 GB, not
  the terabytes they take. OpenDrive counts what it sends against the plan's daily download limit, which may be more
  than what is read when a connection is closed early. When the limit is reached the scan stops with OpenDrive's
  message and goes on the next day (run `cloud.scanFolders` again).
- **Storage:** none in the account. The scan, plan and report are a few hundred bytes per file in the library.

## Limits now

- Scanning works with OpenDrive accounts and folders on this computer; the other providers can follow (they need a
  listing of the user's folders, a content hash where the provider keeps one, and ranged reads).
- Camera serial numbers come from Exif's `BodySerialNumber`; Nikon bodies that keep it only in their maker notes give
  none, and duplicates of their photos are matched on time, model and size.
- Video capture times are what the camera wrote in the movie header (often local time, though the format says UTC);
  they place a video in a trip's dates but don't decide duplicates (hash and size do).
- Not yet run against a real OpenDrive account: tested against a fake OpenDrive built from its API guide.

## Step 2: albums that point at the files where they are

The plan becomes LightCraft albums whose photos stay in the account: the catalog references each original by account
and file id; thumbnails and smart previews are built by reading each photo once (for raws, often only their embedded
preview), so browsing and editing work without the originals on this computer; exporting or full-resolution work
downloads the original when needed. This builds on phase 4 of [cloud storage](cloud-storage.md) ("originals stay in
the cloud"), and works for any provider once it can be browsed.
