# lightcraft-organize

Albums from photos already in a storage account, without changing them (L2; design and use:
[docs/cloud-albums.md](../../docs/cloud-albums.md)).

- `scan`: walk folders through `lightcraft_cloud::Browse` (read-only by construction), read each file's provider
  hash and the metadata at its start with ranged reads, in parallel, resumable through a JSON-lines cache.
- `video`: an MP4 / MOV's creation time from its movie header, reading only box headers.
- `plan`: albums by trip across trees, identical and likely duplicates, raw + JPEG pairs, merge suggestions, the
  user's edits. Pure and deterministic.
- `report`: the plan as a self-contained HTML page to review.

Tests: `tests/scan.rs` (camera-like files on a folder), `tests/scan_opendrive.rs` (the fake OpenDrive of
`lightcraft-cloud`'s tests: parallel workers, the daily download limit), `tests/plan.rs` (two overlapping trees).
