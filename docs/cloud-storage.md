# Distributed cloud storage (proposal)

**Status:** proposal; phase 1 (core crate `crates/cloud`) done; phase 2 (network providers) done: S3-compatible, WebDAV, Google Drive, OneDrive and Dropbox; phase 3 (backup and
restore) done: `cloud.*` commands (CLI, control channel, MCP) and Settings ▸ Cloud. Opt-in; nothing changes for a library
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
| **2. Network providers** (done) | S3-compatible (`crates/cloud/src/s3.rs`: B2, R2, Wasabi, MinIO…; SigV4 written from the spec, checked against AWS's published examples and botocore), WebDAV (`crates/cloud/src/webdav.rs`: Nextcloud, ownCloud, NAS; basic auth only over https or to this computer), Google Drive (`crates/cloud/src/gdrive.rs`), OneDrive (`crates/cloud/src/onedrive.rs`), Dropbox (`crates/cloud/src/dropbox.rs`), see below. Sign-in for consumer drives: `crates/cloud/src/oauth.rs`, the authorization code flow with PKCE (RFC 7636) and a loopback redirect (RFC 8252), no LightCraft server. Later: S3 multipart upload above 5 GiB | Contract checks pass against local test servers (moto for S3, WsgiDAV with and without auth; fakes of Drive, Graph and Dropbox in `tests/`) |
| **3. Backup (one-way)** (engine done) | Push originals, catalog snapshots and smart previews; restore a whole library on another machine. `crates/cloud/src/backup.rs` (the fleet: placement, encrypted blobs, the manifest on every account, restore, prune), `crates/cloud/src/accounts.rs` (a vault's accounts opened by kind), `crates/engine/src/cloud.rs` + `crates/engine/src/cmd/cloud.rs` (`cloud.*` commands, worker-thread jobs), `crates/ui-egui/src/panels/cloud.rs` (Settings ▸ Cloud). Next: Info ▸ "Stored on"; S3, WebDAV and restore in the UI | A library restored from the accounts alone matches the original (`crates/engine/src/tests_cloud.rs`) |
| **4. Offload originals** | Availability per album / photo; "Free up local space"; an offloaded original is fetched on demand while its smart preview is shown; ties into Missing Photos | Edit and export a photo whose original is only in the cloud |
| **5. Several devices** | First one writer at a time (a remote lease next to `catalog.lock`: "checked out on MacBook"); later per-op device id + Lamport clock and per-op merge rules | Two machines take turns on one library; later edit at once |
| **6. Web and tablet** | The web build opens the synced library (browser `fetch` transport; only CORS-friendly providers) | A tablet browses and edits a desktop library |
| **7. Storage Distributor reads it** | The web app shows LightCraft's blobs in its drive view; optionally runs this crate as wasm | One implementation of the format |

### Google Drive

- **Scope `drive.file`:** LightCraft sees only the files it created, never the rest of the user's Drive. It is also a
  scope Google doesn't classify as restricted, so a published client id needs no security assessment.
- **Layout:** one app folder in My Drive (`LightCraft` by default); each object is a file in it named by its full object
  name (`blobs/ab/<id>.bin`), since Drive allows `/` in names. The `RemoteRef` is the Drive file id. Drive doesn't keep
  names unique, so when two devices store the same name at once, `put` and `list` both use the oldest file.
- **Uploads:** one multipart request up to 5 MiB; above that a resumable session in chunks (8 MiB by default). After a
  dropped connection or a 5xx answer, it asks Google how many bytes it holds (`Content-Range: bytes */N`) and goes on
  from there. A session address that points at any other server than the API is refused.
- **Errors:** an expired access token is refreshed once; 429, 403 `rateLimitExceeded` and 5xx answers are retried with
  backoff (5 times); `storageQuotaExceeded` is "the storage account is full"; a revoked refresh token asks the user to
  sign in again. Ids, names, sizes and page tokens from Google are validated before use.
- **Quota:** `about.storageQuota`, the whole account (Drive, Gmail and Photos share it); no limit for accounts without
  one. Refreshing it (for placement) is the engine's job in phase 3; a few minutes' staleness is fine.
- **Client id:** each build needs a Google Cloud OAuth client of type *Desktop app* (public, per RFC 8252). Users can
  bring their own. Google's token endpoint also wants that client's secret (not a secret for desktop clients, RFC
  8252 §8.5): Settings ▸ Cloud asks for it with the client id, and the vault keeps it with the account. While the
  OAuth consent screen is in "Testing", add your Google account as a test user (and refresh tokens expire after 7 days).

### OneDrive

- **Scope `Files.ReadWrite.AppFolder`:** only the app's folder (`Apps/<app name>`, Graph's `special/approot`), personal
  and work/school accounts (the `common` endpoint). A public client: no secret. Microsoft rotates refresh tokens; the
  provider keeps the newest (`refresh_token()`) for the vault.
- **Layout:** files addressed by path (`blobs/ab/<id>.bin`), folders made as needed; the `RemoteRef` is the path.
- **Uploads:** one request up to 4 MiB; above that an upload session in chunks (multiples of 320 KiB, 10 MiB by
  default). After a failure it asks the session for `nextExpectedRanges` and goes on. The session address and download
  redirects are pre-authenticated addresses and are sent no credentials; they must be https. A listing's next page must
  come from Graph itself.
- **Errors:** 429 and 5xx retried with `Retry-After`; `quotaLimitReached` / 507 is "the storage account is full".
- **Registration** (Microsoft Entra admin center ▸ App registrations): supported account types "Accounts in any
  organizational directory and personal Microsoft accounts" (the `common` endpoint); API permissions: Microsoft Graph
  delegated `Files.ReadWrite.AppFolder` and `offline_access`; no secret. Redirect URI `http://127.0.0.1` of type
  `InstalledClient` (sign-in sends `http://127.0.0.1:<port>`; Microsoft ignores the port of loopback redirect URIs).
  The portal's form doesn't take an `http://127.0.0.1` address, so add it in the app's **Manifest**:
  `"replyUrlsWithType": [{"url": "http://127.0.0.1", "type": "InstalledClient"}]` (Microsoft's "Redirect URI (reply
  URL) best practices and limitations" page). Not yet tried against a real registration.

### Dropbox

- **App folder access:** the app is registered with "App folder" access, so it sees only `Apps/<app name>`. Scopes:
  `files.content.write`, `files.content.read`, `files.metadata.read`, `account_info.read`. A public client with PKCE.
- **Redirect:** Dropbox matches redirect URIs exactly, so sign-in listens on a fixed loopback port (53682 by default,
  `OAuthClient::redirect_port`), which the app registration lists as `http://127.0.0.1:53682`.
- **Layout:** files addressed by path under an optional subfolder of the app folder (`root`); the `RemoteRef` is the
  object name.
- **Uploads:** one request up to 8 MiB; above that an upload session appended in 8 MiB chunks. A chunk whose answer was
  lost is sent again, and Dropbox's `incorrect_offset` says how much it really holds.
- **Errors:** 429 and 5xx retried with `Retry-After`; `insufficient_space` is "the storage account is full";
  `not_found` on delete is not an error.

### Signing in by hand

`cargo run -p lightcraft-cloud --example cloud-signin -- gdrive|onedrive|dropbox <client id> [<secret>]` opens the
sign-in and prints a refresh token (for the live tests below).

The 9Drive web app (Zenhosta, Apache-2.0; a server-hosted Express + MySQL gateway over several Google Drive and S3
accounts) served as a reference for which Drive calls a multi-account gateway needs. No code was taken from it: this is
written from Google's public REST documentation.

## Using it (phase 3)

Everything is a command, so the CLI, the control channel and MCP drive it; **Settings ▸ Cloud** calls the same ones:
set up or unlock with the passphrase, add a folder (the folder picker) or sign in to Google Drive / OneDrive / Dropbox
in the browser (with the client id of an app registration), a size limit per account, copies of originals, smart
previews on or off, Back Up Now with progress and Stop, Check Accounts, Lock. S3 and WebDAV accounts and restoring a
library are commands only for now. None of them is journaled (they carry passphrases and credentials).

```text
cloud.setup        {passphrase}                         a new vault (data key, no accounts) in <library>/cloud/
cloud.unlock       {passphrase}   cloud.lock {}          cloud.changePassphrase {old, new}
cloud.addAccount   {provider: folder|s3|webdav, config, label, cap?, speed?, priority?}
cloud.signIn       {provider: gdrive|onedrive|dropbox, clientId}  → {url}; cloud.signInStatus adds the account
cloud.setAccount   {id, label?, cap?, speed?, priority?, acceptsUploads?}    cloud.removeAccount {id}
cloud.configure    {originalCopies?: 1–8, smartPreviews?, clientIds?}
cloud.backup       {wait?}        cloud.progress {}      cloud.cancel {}      cloud.refresh {wait?}
cloud.restore      {dir, provider, config, passphrase, wait?}   cloud.finishRestore {}
cloud.status       {}
```

- **In the library:** `cloud/vault.enc` (sealed), `cloud/settings.json` (caps, speed, priority, copies, client ids,
  device id; nothing secret), `cloud/manifest.enc` (this device's copy of the manifest).
- **What a backup sends:** the catalog files (`catalog.snap`, `catalog.log`, `presets.json`, `prefs.json`, `view.json`),
  a `library.info` record (the library's folder) and the cloud settings, to every account; each library photo's
  original (virtual copies share theirs), placed by free space, speed and priority, `originalCopies` copies on different
  providers when there are enough of them; smart previews (one copy). Encrypted under the vault's data key; files
  already stored with the same content are skipped, so a second backup of an unchanged library sends nothing. Photos
  that left the library are marked trashed in the manifest (their blobs stay until a prune). Whole files are read into
  memory, so a file over 4 GiB is skipped and reported. Every account also gets the vault and an `account.json` naming
  its id in the vault.
- **Accounts without a known size** (a folder, some WebDAV servers) get originals only once the user sets a cap:
  placement never guesses free space.
- **An account that fails** (an unplugged disk, a revoked sign-in) is reported in `cloud.status` and skipped; the
  others still take the backup. A missing folder is never created except when the account is added.
- **Restore** needs one account (its current credentials) and the passphrase: the vault on it opens every other
  account, the newest manifest is read, and the library is written into a new folder: the catalog files,
  originals that were inside the old library at the same place, the others under `Restored Originals/<their old path>`,
  smart previews. Nothing is written outside that folder. `cloud.finishRestore` (automatic with `wait`) opens it and
  relinks the originals with Find Missing Photos. A damaged copy is skipped for another account's copy.
- **Rotated refresh tokens** (Microsoft) are saved back into the vault after each job.
- **Limits now:** one device writes at a time (phase 5 adds a lease); the passphrase is kept in memory while unlocked
  (to reseal the vault when accounts change); quotas are read at the start of each job, not watched.

## Testing

`cargo test -p lightcraft-cloud` and `cargo test -p lightcraft-engine tests_cloud` run everything offline: the provider contract against the memory and folder
providers and fakes of Google Drive, Microsoft Graph and Dropbox (with dropped chunks, lost answers, rate limits,
expired and rotated tokens, hostile addresses and the sign-in redirect), SigV4 against fixed vectors, the HTTP client against a local echo server. `tests/live.rs` runs the same
contract against real servers, only when one is configured:

```sh
LIGHTCRAFT_TEST_S3=http://127.0.0.1:5055 \
LIGHTCRAFT_TEST_WEBDAV=http://127.0.0.1:8081/ \
  cargo test -p lightcraft-cloud --test live
```

`LIGHTCRAFT_TEST_S3_KEY` / `_SECRET` / `_REGION` and `LIGHTCRAFT_TEST_WEBDAV_USER` / `_PASSWORD` set credentials.
`LIGHTCRAFT_TEST_GDRIVE_CLIENT_ID` / `_CLIENT_SECRET` / `_REFRESH_TOKEN` run it against a real Google account (in a new
folder in My Drive per run); `LIGHTCRAFT_TEST_ONEDRIVE_CLIENT_ID` / `_REFRESH_TOKEN` against OneDrive (the app folder
must be empty); `LIGHTCRAFT_TEST_DROPBOX_CLIENT_ID` / `_REFRESH_TOKEN` against Dropbox (a new subfolder per run). Any
S3-compatible server works (MinIO, Garage, moto: `moto_server -p 5055`); any WebDAV server (WsgiDAV:
`wsgidav --port 8081 --root <dir> --auth anonymous`). Each run writes to a fresh app folder.

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
