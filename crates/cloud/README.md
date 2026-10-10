# lightcraft-cloud

Opt-in distributed storage over the user's own accounts (L1). Proposal and phases: `docs/cloud-storage.md`; on-storage
format: `docs/cloud-format.md`.

- `Availability` / `SpeedClass`: five availability levels, four account speed classes.
- `placement`: which account(s) store a blob, a pure function ported from the Storage Distributor engine with its tests.
- `Provider`: blocking storage interface (run on a worker thread); `FolderProvider` (a folder on a disk, NAS or synced
  drive) and `MemProvider` (tests, fault injection).
- `ContentId` / `ObjectName`: blobs named by SHA-256, validated names that can't leave the app folder; `fetch_blob`
  verifies content against its id.
- `contract::check_provider`: the checks every provider passes (`tests/providers.rs`).

No network code yet; no UI. Builds for wasm32.
