# The drive: one Google Drive-style view over your storage accounts

**Status:** works with the accounts cloud backup supports (folders, S3-compatible, WebDAV, Google Drive, OneDrive,
Dropbox). Opt-in; nothing changes for a library that doesn't use it. Design of the storage underneath:
[`cloud-storage.md`](cloud-storage.md); on-storage format: [`cloud-format.md`](cloud-format.md).

Cloud backup spreads a library over the user's own accounts. The drive puts the same machinery behind a plain file
manager: the user drops any files in, organises them in folders, and gets them back, without seeing which account holds
what. Placement (free space, caps, speed, priority), copies on different providers, end-to-end encryption and the
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
- **Upload speed** (`level`: instant … archive) and **copies** (1–8, on different providers) are per upload; the
  dashboard's Settings pick its defaults.
- **Scripts:** transfers run on a worker thread, so a `lightcraft-cli run` that uploads ends with `drive.wait` (or passes
  `wait=true` to `drive.upload`); otherwise the process can exit with uploads still queued (nothing is half-stored:
  an entry is recorded only once its file is on an account).

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
| `POST /api/call {command, params}` | runs a `drive.*` or `cloud.*` command (not `cloud.restore` / `cloud.finishRestore`) → `{ok, result \| error}` |
| `PUT /api/upload?name=&parent=&level=&copies=` (body: the file) | writes it to `cloud/inbox/`, then `drive.upload {staged: true}`, which removes it once stored |
| `GET /api/file/<task>[?inline=1]` | a finished download (Range requests for video); shown inline only for images, PDF, audio, video and plain text, always under `Content-Security-Policy: sandbox` |
| `GET /api/health` | `{ok, app, version}` |
| anything else | the dashboard's files from `--ui` (unknown routes get `index.html`) |

Safety: it listens on 127.0.0.1 only; a request whose `Host` isn't `127.0.0.1:<port>`, `localhost:<port>` or
`[::1]:<port>` is refused (DNS rebinding); API calls need an `X-LightCraft-Drive` header (a page on another site can't
send it without a CORS preflight, which is never granted); a download navigation from another site
(`Sec-Fetch-Site: cross-site`) is refused. Request heads are capped at 16 KiB, JSON bodies at 1 MiB, uploads at 4 GiB.
The passphrase crosses only the loopback connection.

## Testing

`cargo test -p lightcraft-engine tests_drive` (folders, uploads, unique names, moves, stars, the trash, deleting stored
copies, downloads, the library folder next to drive files, a backup not trashing them, the drive read back from the
accounts alone) and `cargo test -p lightcraft-cli --test drive` (the HTTP API against a real server process, including
the requests it must refuse).

## Next

- Stream large files instead of reading them whole (segmented encryption already allows it), lifting the 4 GiB limit.
- Byte progress for uploads to the accounts (today: per file).
- Thumbnails of photos and the library's smart previews in the grid.
- Move a file to another speed (re-placement) and change its number of copies after upload.
