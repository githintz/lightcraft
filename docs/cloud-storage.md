# Distributed cloud storage (proposal)

**Status:** proposal; phase 1 (core crate `crates/cloud`) done, phase 2 (network providers) in progress. Opt-in; nothing changes for a library
that doesn't turn it on. The local library stays the source of truth.

LightCraft is local-first: no LightCraft account, no LightCraft server, no telemetry. This proposal keeps all three.
It lets a library spread its originals, smart previews and catalog over **the user's own storage accounts**, as many as
they like, from any providers: a second disk, a NAS, an S3-compatible bucket, a WebDAV server, consumer cloud drives.
LightCraft decides which account holds each file from the free space left, the caps the user sets and how quickly the
file must come back. The design comes from the Storage Distributor web app (a Drive-style app over many accounts, in
TypeScript), whose placement engine, availability levels and storage layout are reused here. The two share one
on-storage format ([`cloud-format.md`](cloud-format.md)), so either app can read what the other stored.

## Why it fits LightCraft

| Storage Distributor | LightCraft today |
|---|---|
| Index = op log + snapshots | The catalog is an op log + snapshots (`crates/catalog/src/journal.rs`) behind a byte-store trait (`crates/catalog/src/store.rs`) |
| Blobs named by content hash | Photos carry a `content_hash`; the web build stores originals by hash |
| Availability levels, Instant → Archive | Smart previews (~1 MB proxies) edit and export with the original offline: proxies and catalog on fast accounts, originals on slow, cheap ones |
| Zero backend, the user's own accounts, end-to-end encryption | No account, no server, no telemetry |
| Browser CORS limits (tiers, relay) | The desktop app isn't a browser: no CORS, no relay needed |
| Resumable, verified transfers | `crates/fetch`: pure-Rust HTTPS (rustls + RustCrypto), resume, timeouts, SHA-256 checks |

It also opens a path to the tablet companion (#74): a library in the user's accounts can be opened by the web build on a
tablet.

## Ground rules

- **Opt-in and local-first.** Off by default. Turning it off leaves a normal local library. No LightCraft-run service is
  ever in the data path.
- **Pure Rust, no `unsafe`.** Provider clients are written against their public REST APIs; no vendor SDKs with C
  dependencies. Credentials live in a passphrase-encrypted vault (Argon2id + AES-256-GCM), not an OS keychain, which
  would need FFI.
- **Never crash.** Every network response, listing and stored name is hostile input: sizes are capped before reading,
  names validated ([`ObjectName`](../crates/cloud/src/blob.rs)), content checked against its SHA-256 id. Calls block and
  run on worker threads; the UI never waits on the network.
- **Everything is a command.** `cloud.*` commands (accounts, connect, caps, backup, restore, availability, status) for
  the UI, CLI, control channel and MCP alike.
- **No provider artwork.** Providers are shown by name only; logos are trademarks (see `CLAUDE.md` → Assets).
- **Out:** providers with no official API or terms that forbid this use (TeraBox), and MEGA until its own crypto has a
  clean-room description.

## Architecture

```
 ui-egui / mcp (L5)   Settings ▸ Cloud, Info ▸ "Stored on", cloud.* commands
        │
 engine (L4)          backup / restore / offload jobs on worker threads, account registry, vault
        │
 catalog (L3)         journal: a remote-replicated Store for catalog.snap / catalog.log (phase 3+)
        │
 cloud (L1)           Availability, placement, Provider trait, ContentId / ObjectName, contract checks
        │                 FolderProvider, MemProvider; later S3, WebDAV, Google Drive, OneDrive, Dropbox
 fetch (L0)           HTTPS transport (rustls, RustCrypto), resume, timeouts
```

`lightcraft-cloud` sits at L1 so the catalog and engine can use it, and builds for wasm32 so the web app can too (with
the browser's `fetch` as transport there).

## Placement

Port of the Storage Distributor engine (`crates/cloud/src/placement.rs`, with its tests). An account qualifies when it
accepts uploads, has room, allows the file's size and is at least the level's minimum speed. Qualifying accounts are
ranked by free share left, fit to the level's ideal speed, reliability, user priority, minus a penalty for being faster
than needed (keeps fast space for files that need it). Copies go to different providers. Free space is the smaller of
the provider's quota left and the user's cap minus what the library stored (90 % of the quota when no cap is set; an
account of unknown size gets nothing until the user sets a cap).

What each kind of file asks for by default:

| Data | Level | Copies |
|---|---|---|
| Catalog snapshot + log | Instant | every account (it's small; losing one account never loses the index) |
| Smart previews, thumbnails | Instant | 1 |
| Originals | Standard (per album / photo setting) | 1, recommended 2 |
| Rejected / archived photos | Archive | 1 |

## Phases

| Phase | Scope | Done when |
|---|---|---|
| **0. Format spec** | [`cloud-format.md`](cloud-format.md): blob naming, encryption, manifest; shared with Storage Distributor | Both projects reference one spec |
| **1. Core crate** (done) | `crates/cloud`: availability, placement (+ ported tests), `Provider` trait, content-addressed blobs, folder + memory providers, contract checks, segmented AES-256-GCM blob encryption, the Argon2id passphrase vault, the manifest (per-field last-writer-wins merge, snapshots, op logs) | Contract checks pass for every provider; no UI |
| **2. First network providers** | S3-compatible (B2, R2, MinIO for tests; SigV4 by hand), WebDAV (Nextcloud); then Google Drive, OneDrive, Dropbox with PKCE + loopback redirect (RFC 8252) | Contract checks pass against local test servers (MinIO, a WebDAV server) |
| **3. Backup (one-way)** | Push originals, catalog snapshots and smart previews; restore a whole library on another machine. `cloud.connect`, `cloud.accounts`, `cloud.backup`, `cloud.restore`; Settings ▸ Cloud; Info ▸ "Stored on" | A library restored from the accounts alone matches the original |
| **4. Offload originals** | Availability per album / photo; "Free up local space"; an offloaded original is fetched on demand while its smart preview is shown; ties into Missing Photos | Edit and export a photo whose original is only in the cloud |
| **5. Several devices** | First one writer at a time (a remote lease next to `catalog.lock`: "checked out on MacBook"); later per-op device id + Lamport clock and per-op merge rules | Two machines take turns on one library; later edit at once |
| **6. Web and tablet** | The web build opens the synced library (browser `fetch` transport; only CORS-friendly providers) | A tablet browses and edits a desktop library |
| **7. Storage Distributor reads it** | The web app shows LightCraft's blobs in its drive view; optionally runs this crate as wasm | One implementation of the format |

## Risks

- **Multi-device merge** is the hard part; phase 5 starts with a single-writer lease to avoid it.
- **Raw decodes need the whole original**; smart previews cover editing meanwhile, and offloading is per album.
- **Provider clients are upkeep**, and each needs an OAuth app registration (client ids are public; bring-your-own is
  supported).
- **A lost passphrase loses encrypted data**: a recovery kit at setup, clear warnings.

## Open decisions (maintainer)

- Whether upstream accepts an opt-in cloud feature at all (the README says "no cloud"; `LR-CLOUD-SYNC` in
  `docs/parity.md` is out of scope today).
- Storage Distributor needs an open-source licence compatible with MIT OR Apache-2.0 before more of its code is ported.
