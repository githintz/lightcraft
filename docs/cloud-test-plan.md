# Cloud storage: test scenarios and edge cases

Two uses of the same storage (`crates/cloud`): **the library's backup** (`cloud.*`, Settings ▸ Cloud) and **the drive**
(`drive.*`, `lightcraft-cli drive`, the Storage Distributor dashboard). Each scenario names the test that covers it.
Status: ✅ passes · 🐞 found a bug, fixed (the test failed before the fix) · ⚠️ known limit, documented.

Test locations: `crates/cloud/tests/*` (providers, fleet), `crates/engine/src/tests_cloud.rs` (backup), 
`crates/engine/src/tests_drive.rs` and `tests_drive_edge.rs` (drive commands), `apps/lightcraft-cli/tests/drive.rs` and
`drive_http.rs` (the HTTP server), `storage-distributor/e2e/*.spec.ts` (the dashboard in Chromium against a real
`lightcraft-cli drive`).

## A. Library backup

| ID | Scenario | Test | Status |
|---|---|---|---|
| A1 | Passphrase: too short, wrong at unlock, change with a wrong old one; setup twice refused | tests_cloud `passphrases_accounts_and_failures` | ✅ |
| A2 | Back up to two folder accounts; a second backup sends nothing | tests_cloud `back_up_and_restore…` | ✅ |
| A3 | Restore on "another machine" from one account: edits, originals (inside and outside the old library) relinked | tests_cloud `back_up_and_restore…` | ✅ |
| A4 | An unplugged account during a backup: reported, the others take it | tests_cloud `passphrases_accounts_and_failures` | ✅ |
| A5 | Backup on a worker thread, progress, cancel, resume | tests_cloud `backup_on_a_worker_thread…` | ✅ |
| A6 | Restore into a folder that already holds a library refused; restore paths never leave the new folder | tests_cloud `restore_paths_stay_inside…` | ✅ |
| A7 | A damaged copy falls back to another account's copy; a failing account is skipped | cloud tests/backup.rs | ✅ |
| A8 | OpenDrive: password sign-in, rotating tokens saved, expiry + Sign In Again, restore keeps the live token | tests_cloud `opendrive_signs_in…` | ✅ |
| A9 | The same folder added as an account twice | tests_drive_edge `the_same_folder_twice_as_two_accounts` | |
| A10 | A photo deleted from the library: its entry trashed, drive files untouched | tests_drive `the_library_backup_shows_read_only…` | ✅ |
| A11 | A backup while drive uploads are queued: both sides' changes kept | tests_drive_edge `backup_and_drive_work_at_once` | |
| A12 | OpenDrive used by the drive's worker and a backup at once (one refresh token, rotated on every use) | tests_drive_edge `opendrive_shared_by_the_drive_and_a_backup` | |
| A13 | Unicode / emoji / spaces in photo paths backed up and restored | tests_drive_edge `unicode_paths_round_trip` | |

## B. The drive (commands)

| ID | Scenario | Test | Status |
|---|---|---|---|
| B1 | Locked / not set up / no library: every command says so, none crashes | tests_drive_edge `gates` | |
| B2 | Folder names: empty, spaces only, `/` `\`, `.` `..`, control characters, 300 bytes, emoji, RTL, case-only clashes → `name (1)` | tests_drive_edge `names` | |
| B3 | mkdir into a trashed folder, into the library folder, into a file, into a missing id: refused | tests_drive_edge `names` | |
| B4 | Upload an empty file, the same content twice (stored once), the same name twice | tests_drive_edge `uploads` | |
| B5 | Upload with no account, with no room (caps), copies > accounts | tests_drive_edge `uploads_that_cannot_be_stored` | |
| B6 | Upload a folder tree with hidden files, empty folders, a symbolic link and a file over the size limit | tests_drive_edge `folder_uploads` | |
| B7 | The upload's folder trashed or deleted while it is queued; the source file deleted before the worker reads it | tests_drive_edge `uploads_whose_folder_or_file_goes` | |
| B8 | Cancel a queued upload; clear finished transfers | tests_drive_edge `uploads` | |
| B9 | Rename to a sibling's name, rename a library file, rename a missing id | tests_drive_edge `rename_move_star` | |
| B10 | Move into itself or a descendant, into the trash, into the library; several items with clashing names | tests_drive_edge `rename_move_star` | |
| B11 | Trash a folder: its contents hidden from folders, search, recent, starred; status counts | tests_drive_edge `trash_and_restore` | |
| B12 | Restore a child whose folder is trashed (→ top); restore a folder and its child together (both back in place, any order) | tests_drive_edge `trash_and_restore` | |
| B13 | Delete forever a file whose content another file shares: the other keeps its stored copy | tests_drive_edge `deleting_shared_content` | |
| B14 | Empty trash with nested trashed items | tests_drive_edge `trash_and_restore` | |
| B15 | Download: content identical; a trashed file; a file deleted before the worker gets to it; a library original | tests_drive_edge `downloads` | |
| B16 | Search: case-insensitive, trashed excluded, library originals included, empty query | tests_drive_edge `views` | |
| B17 | Recent / starred / trash views and breadcrumbs in nested folders and library folders | tests_drive_edge `views` | |
| B18 | Restart: everything comes back from the library; the drive comes back from the accounts alone | tests_drive `the_drive_comes_back…` | ✅ |
| B19 | Two devices on the same accounts: changes on each merged by `drive.sync` | tests_drive_edge `two_devices` | |
| B20 | Corrupt `drive.json`, corrupt `manifest.enc`, a missing outbox: errors or defaults, never a crash | tests_drive_edge `damaged_files` | |
| B21 | An account fails during uploads: the task fails with its message, the next upload uses the other account | tests_drive_edge `uploads_that_cannot_be_stored` | |

## C. The HTTP server (`lightcraft-cli drive`)

| ID | Scenario | Test | Status |
|---|---|---|---|
| C1 | Host checks (DNS rebinding), the API header, cross-site downloads | cli tests/drive.rs | ✅ |
| C2 | Malformed requests: no request line, oversized head (431), > 100 headers, bad percent escapes, a JSON body > 1 MiB, no Content-Length, not JSON, no `command` | drive_http `malformed_requests` | |
| C3 | Commands the dashboard may not run (`cloud.restore`, `library.*`, `photo.*`, `app.*`) | cli tests/drive.rs | ✅ |
| C4 | Uploads: cut off mid-body, `name` missing or hostile (`../x`, `a/b`), when locked, empty body, two at once | drive_http `uploads` | |
| C5 | Downloads: ranges (start, middle, suffix, past the end, multiple, malformed), HEAD, names with quotes and Unicode in Content-Disposition, HTML shown as text, PDF inline | drive_http `downloads` | |
| C6 | Static files: traversal, dot files, the SPA fallback, `/` → index.html, unknown asset → 404 | cli tests/drive.rs, drive_http | |
| C7 | A blocking request (`wait`, `drive.wait`) doesn't freeze the server for the dashboard | drive_http `waits_dont_block_the_server` | |
| C8 | `drive.upload` of an arbitrary local path through the API (only the inbox may be handed over) | drive_http `uploads` | |
| C9 | Many connections at once (> 64): refused with 503, the server keeps serving | drive_http `many_connections` | |

## D. The dashboard (Chromium, against a real server)

| ID | Scenario | Spec | Status |
|---|---|---|---|
| D1 | First visit: set up the passphrase (too short, mismatch), then unlock after a restart; wrong passphrase | e2e/gate.spec.ts | |
| D2 | Add a folder storage location; its space shown; rename, size limit, pause, remove | e2e/storage.spec.ts | |
| D3 | Upload files (input) and a folder; progress panel; names clash → "(1)" | e2e/drive.spec.ts | |
| D4 | New folder, open it, breadcrumbs back | e2e/drive.spec.ts | |
| D5 | Rename, move (dialog), star, trash, restore, delete forever, empty trash | e2e/drive.spec.ts | |
| D6 | Search; Recent; Starred | e2e/drive.spec.ts | |
| D7 | Preview an image, a text file, a PDF; arrow keys; download a file (bytes identical) | e2e/drive.spec.ts | |
| D8 | The LightCraft Library folder: read-only (no rename / trash / upload) | e2e/drive.spec.ts | |
| D9 | Home: totals, space, recent files | e2e/home.spec.ts | |
| D10 | Settings persist across reloads (theme, density, list/grid) | e2e/settings.spec.ts | |
| D11 | The server goes away and comes back: the can't-reach screen, then recovery | e2e/gate.spec.ts | |
| D12 | Names that are HTML (`<img onerror>`) shown as text | e2e/drive.spec.ts | |
| D13 | Phone width (390 px): no horizontal scroll, menus reachable | e2e/drive.spec.ts | |
| D14 | Lock from the dashboard | e2e/gate.spec.ts | |
