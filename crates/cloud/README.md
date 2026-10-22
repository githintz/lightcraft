# lightcraft-cloud

Opt-in distributed storage over the user's own accounts (L1). Proposal and phases: `docs/cloud-storage.md`; on-storage
format: `docs/cloud-format.md`.

- `Availability` / `SpeedClass`: five availability levels, four account speed classes.
- `placement`: which account(s) store a blob, a pure function ported from the Storage Distributor engine with its tests.
- `Provider`: blocking storage interface (run on a worker thread); `FolderProvider` (a folder on a disk, NAS or synced
  drive) and `MemProvider` (tests, fault injection).
- `ContentId` / `ObjectName`: blobs named by SHA-256, validated names that can't leave the app folder; `fetch_blob`
  verifies content against its id.
- `crypto`: segmented AES-256-GCM blob encryption (file keys wrapped by the library's data key); `vault`: the data key
  and account credentials under an Argon2id passphrase key; `manifest`: entries and replicas as an op log whose ops
  merge in any order (per-field last writer wins), with snapshots.
- `s3` (S3-compatible, SigV4 by hand), `webdav` (RFC 4918, basic auth), `gdrive` (Google Drive v3, `drive.file`
  scope), `onedrive` (Microsoft Graph, app folder) and `dropbox` (API v2, app folder), with resumable uploads: network
  providers on `lightcraft-fetch`'s pure-Rust HTTPS client, native only. `oauth`: sign-in with PKCE and a loopback
  redirect (RFC 7636, RFC 8252), no server, and `TokenSource` (access tokens from a refresh token);
  `examples/cloud-signin.rs` prints a refresh token. Live tests: `docs/cloud-storage.md` → Testing.
- `backup`: a `Fleet` of accounts: back up (placement, encrypted blobs, skip what is stored), store a file into a
  folder (`store_file`, the drive), delete copies no entry uses any more (`delete_unused`), save and load the manifest
  on every account, restore with fallback to another copy, prune; `accounts::open` turns a vault entry into a
  provider. The engine's `cloud.*` and `drive.*` commands use them (`crates/engine/src/cmd/cloud.rs`, `cmd/drive.rs`).
  `Manifest::merge` folds one copy of the manifest into another (as if both copies' ops were applied).
- `contract::check_provider`: the checks every provider passes (`tests/providers.rs`; Google Drive, OneDrive and
  Dropbox against fake servers in `tests/gdrive.rs`, `tests/onedrive.rs`, `tests/dropbox.rs`).

No UI yet. Builds for wasm32; randomness comes from the caller (`crypto::Random`; `OsRandom` on native).
