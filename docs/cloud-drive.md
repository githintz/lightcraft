# The drive: one Google Drive-style view over your storage accounts

**Status:** works with the accounts cloud backup supports (folders, S3-compatible, WebDAV, Google Drive, OneDrive,
Dropbox, OpenDrive). Opt-in; nothing changes for a library that doesn't use it. Design of the storage underneath:
[`cloud-storage.md`](cloud-storage.md); on-storage format: [`cloud-format.md`](cloud-format.md).

Cloud backup spreads a library over the user's own accounts. The drive puts the same machinery behind a plain file
manager: the user drops any files in, organises them in folders, and gets them back, without seeing which account holds
what. Placement (free space, caps, speed, priority), copies on different accounts, end-to-end encryption and the
manifest on every account all work as they do for backups. The library's own backup appears next to the user's files
as a read-only **LightCraft Library** folder (Originals by folder, Smart previews, Catalog), so everything stored is in
one place.

The dashboard is the [Storage Distributor](https://github.com/githintz/storage-distributor) web app, served by
`lightcraft-cli drive`.

## Running it

```sh
# headless, on a library (made if it doesn't exist)
lightcraft-cli drive --library ~/Pictures/LightCraft --ui ../storage-distributor/dist
# or alongside the running app (it keeps the library open)
lightcraft --control 7980 &
lightcraft-cli drive --connect --ui ../storage-distributor/dist
```

Then open <http://127.0.0.1:8790/>. Without `--ui` only the API is served (for the dashboard's `npm run dev`, which
proxies `/api` to port 8790). `--port N` picks another port. The first visit sets up the vault (a passphrase) or
unlocks it; Storage ▸ Add storage connects accounts.

## Commands

Everything is a command, so the CLI, MCP and the control channel can use the drive too (`lightcraft-cli run --library
DIR drive.list`). None of them is journaled.

```text
drive.status      {}                       files, sizes by kind, space over every account, the library's backup, transfers
drive.list        {view?: folder|recent|starred|trash|search, folder?, query?}   → {items, path}
drive.mkdir       {name, parent?, reuse?}    drive.rename {id, name}    drive.move {ids, parent}
drive.star        {ids, starred?}            drive.trash {ids}          drive.restore {ids}
drive.delete      {ids}                      drive.emptyTrash {}
drive.upload      {path, parent?, name?, level?, copies?, staged?}   a file (queued) or a whole folder
drive.download    {id} → {task}              drive.file {task} → {path, name, size}
drive.transfers   {}                         drive.cancel {task}        drive.clearTransfers {}
drive.sync        {}                         read the accounts' copy (another device, a restore)
drive.wait        {timeoutMs?}               block until queued transfers are stored (scripts: before exiting)
```

- **Folders:** `root` is the top of the drive; `lib`, `lib.originals`, `lib.previews`, `lib.catalog` and
  `lib.dir.<id>` are the read-only library folders. Library files can be listed, searched and downloaded; the backup
  alone adds, replaces and trashes them.
- **Names** are cleaned (no `/`, `\`, control characters; 255 bytes) and kept unique per folder, Drive-style
  (`report (1).pdf`).
- **The trash:** a trashed folder hides what's in it; restoring puts an item back in its folder, or at the top when
  that folder is gone. Deleting forever removes the items at once and their stored copies in the background; a copy
  another item still uses (identical content is stored once) stays.
- **Transfers** run on one worker thread: uploads (read whole and encrypted in memory, so up to 4 GiB a file in this
  version), downloads (decrypted into `cloud/outbox/`, kept an hour), deleting stored copies, and reading the accounts'
  manifest. Changes to names, folders, stars and the trash are saved in the library at once and on the accounts when
  the worker's queue is empty (`drive.transfers` → `unsynced`).
- **Upload speed** (`level`: instant … archive) and **copies** (1–8) are per upload; the dashboard's Settings pick its
  defaults. Copies go to different accounts, one per provider first; when there are fewer providers than copies, the
  rest go to other accounts of the same provider (two disks are two places even if both are folders). Content already
  stored for another item (the same file uploaded twice, a photo the library backup holds) is shared, not sent again.
- **Uploads that can't be stored** fail with the reason (no account accepts files, no room under the size limits, an
  account that can't be reached); a folder upload reports the files it skipped (links, special files, files over the
  size limit) and queues the rest. A name that can't be a file name (`..`, only spaces) is refused.
- **Scripts:** transfers run on a worker thread, so a `lightcraft-cli run` that uploads ends with `drive.wait` (or passes
  `wait=true` to `drive.upload`); otherwise the process can exit with uploads still queued (nothing is half-stored:
  an entry is recorded only once its file is on an account).

## The vault on the accounts, and more than one computer

Like a backup, the drive puts the sealed vault (`vault.enc`, the data key behind the passphrase) and each account's
`account.json` on every account, so the drive opens from any one account and the passphrase. `account.json` also
carries every account's settings (size limit, speed, preference, paused).

- **Another computer** opens the same drive with `cloud.join {provider, config, passphrase}` (the dashboard's **Use
  that drive here** on its first screen): one account's folder or sign-in and the passphrase. It takes the vault and the
  accounts' settings from that account, then lists and changes the same drive. Each computer saves its own manifest
  snapshot (`snapshot-<clock>-<device>.json.enc`); reading the accounts merges the newest few, and a computer deletes
  another's snapshot only once it has merged it, so changes made on both survive. `drive.sync` reads the accounts again.
- **A joined library doesn't back up there:** the accounts already hold the other library's backup, and a backup of this
  library would mark those photos as removed. Back a joined library up to other accounts.
- **Storage that already holds another drive or backup** (a vault the passphrase doesn't open) can't be added as an
  account; a folder can't be added twice, inside or around another account's folder, or inside the library.
- **OpenDrive** replaces its sign-in token on every refresh, and the old one stops working; every user of one account
  in a process (the drive's worker, a backup) refreshes through one shared token source, and the vault keeps the newest.

## How it shares the storage with backups

- Drive items are manifest entries with ids `drive.<16 hex>`; the backup's are `catalog.*`, `original.*`, `smart.*`,
  `library.info` and `cloud.settings`. A backup trashes only its own entries when their files leave the library, never
  drive items (`Fleet::trash_except` takes the backup's scope).
- Dates and stars, which the manifest doesn't record, are in `cloud/drive.json`, also stored on every account as the
  entry `meta.drive`.
- The session keeps one manifest; the drive's worker and a running backup each work on a copy and merge it back
  (`Manifest::merge`, the per-field last-writer-wins merge of the manifest's ops), so neither loses the other's
  changes. A backup started while uploads run merges their results too.

## The HTTP bridge (`lightcraft-cli drive`)

Pure `std`, in `apps/lightcraft-cli/src/drive_server.rs`. One session thread runs the commands; each connection gets a
thread (64 at most).

| Request | Does |
|---|---|
| `POST /api/call {command, params}` | runs a `drive.*` or `cloud.*` command → `{ok, result \| error}`; not `cloud.restore` / `cloud.finishRestore` (another library), `drive.upload` (a path on this computer: files come through `/api/upload`) or `drive.wait`, and `wait` params are dropped (they would hold the one session thread) |
| `PUT /api/upload?name=&parent=&level=&copies=` (body: the file) | writes it to `cloud/inbox/`, then `drive.upload {staged: true}`, which removes it once stored |
| `GET` / `HEAD /api/file/<task>[?inline=1]` | a finished download (one `bytes=` Range for video; other units or several ranges get the whole file); shown inline only for images, PDF, audio, video and plain text, always under `Content-Security-Policy: sandbox` (a PDF under `default-src 'none'`) |
| `GET /api/health` | `{ok, app, version}` |
| anything else | the dashboard's files from `--ui` (unknown routes get `index.html`) |

Safety: it listens on 127.0.0.1 only; a request whose `Host` isn't `127.0.0.1:<port>`, `localhost:<port>` or
`[::1]:<port>` is refused (DNS rebinding); API calls need an `X-LightCraft-Drive` header (a page on another site can't
send it without a CORS preflight, which is never granted); a download navigation from another site
(`Sec-Fetch-Site: cross-site`) is refused. Request heads are capped at 16 KiB, JSON bodies at 1 MiB, uploads at 4 GiB.
The passphrase crosses only the loopback connection.

## Testing

The scenarios and edge cases, with the test that covers each, are listed in [`cloud-test-plan.md`](cloud-test-plan.md).
`cargo test -p lightcraft-engine tests_drive` (the commands: folders, uploads, unique names, moves, stars, the trash,
deleting stored copies, downloads, the library folder, two computers, joined libraries, damaged files), `cargo test -p
lightcraft-engine tests_cloud` (backups, OpenDrive with the drive at once), `cargo test -p lightcraft-cli --test drive
--test drive_http` (the HTTP API against a real server process: what it must refuse, malformed requests, uploads,
ranges, static files, many connections). The dashboard's own end-to-end tests (`npm run e2e` in Storage Distributor)
drive it in Chromium against a real `lightcraft-cli drive`.

## Next

- Stream large files instead of reading them whole (segmented encryption already allows it), lifting the 4 GiB limit.
- Byte progress for uploads to the accounts (today: per file).
- Thumbnails of photos and the library's smart previews in the grid.
- Move a file to another speed (re-placement) and change its number of copies after upload.
