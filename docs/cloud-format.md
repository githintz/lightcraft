# Distributed storage format (draft 0)

The layout LightCraft (`crates/cloud`) and the Storage Distributor web app use on a storage account, so either can
read what the other stored. **Draft:** every section below is implemented in `crates/cloud`, but the format may still
change before version 1. Design and rationale: [`cloud-storage.md`](cloud-storage.md).

## Accounts and the app folder

Each account has one **app folder** (a provider's app-folder scope, a bucket prefix, a WebDAV path, a local folder).
Nothing outside it is read or written. Paths below are relative to it. On providers whose files are found by id, not by
path (Google Drive), each object is a file directly in the app folder whose name is the whole path below, `/` included.

## Object names (*implemented*)

An object name is 1–4 segments joined by `/`, at most 255 bytes; each segment is non-empty, made of `a-z 0-9 . _ -`,
and does not start with `.`. Names starting with `.` are reserved for a provider's temporary files and are never
listed. A reader validates every name it lists or is given and skips or refuses anything else, so a hostile listing
can't point outside the app folder.

| Path | Holds |
|---|---|
| `blobs/<aa>/<id>.bin` | a blob; `<id>` is its content id, `<aa>` its first two hex digits |
| `manifest/snapshot-<v>.json` | manifest snapshot after op `<v>` (encrypted: `.json.enc`) |
| `manifest/ops-<v>-<n>.json` | manifest ops `n…` after snapshot `<v>` (encrypted: `.json.enc`) |
| `vault.enc` | the vault: the data key, and the accounts' settings and credentials (see Vault) |

## Content ids (*implemented*)

A blob's id is the SHA-256 of its **plaintext** content, as 64 lower-case hex digits. A reader recomputes it after
reading (and decrypting) and treats a mismatch as damage, never as data. Identical content is stored once per account.

LightCraft's catalog `content_hash` (a fast 128-bit hash used to spot duplicates on import) is not a content id; the
manifest maps one to the other.

## Availability levels (*implemented*)

| Level | Name | Slowest account allowed | Ideal account |
|---|---|---|---|
| 5 | `instant` | fast | fast |
| 4 | `fast` | standard | fast |
| 3 | `standard` | standard | standard |
| 2 | `slow` | slow | slow |
| 1 | `archive` | archive | archive |

Speed classes, slowest first: `archive`, `slow`, `standard`, `fast`. JSON stores both as the lower-case names.

## Encryption (*implemented*, `crates/cloud/src/crypto.rs`)

Optional per library, on by default. Each blob gets a random 256-bit **file key** and is encrypted with AES-256-GCM in
segments of 4 MiB of plaintext, so it can be streamed and read in parts without decrypting all of it. The file key is
wrapped with the library's **data key**, which is kept in the vault.

```
header (80 bytes):  "LCB1" | u8 version = 1 | u8 alg = 1 (AES-256-GCM) | u16 0 | u32 LE segment size
                    | nonce prefix (8) | wrap nonce (12) | wrapped file key (32 + 16-byte tag)
segment i:          AES-256-GCM(file key, nonce = prefix ‖ u32 BE (i | 0x8000_0000 on the last segment),
                    aad = the 80-byte header) = ciphertext ‖ 16-byte tag
wrapped file key:   AES-256-GCM(data key, wrap nonce, aad = the first 20 header bytes)
```

- An empty blob has a single empty last segment (just a 16-byte tag).
- Readers accept segment sizes from 4 KiB to 16 MiB and check the plaintext size before decrypting.
- Because each segment is bound to the header and to its position, reordering, dropping or cutting off segments, or
  moving them under another blob's header, all fail the tag check.

Blob ids stay the SHA-256 of the plaintext, so the same file is deduplicated across devices. An open question is
whether to name blobs by an HMAC of the content under the data key instead, so a provider can't confirm that you
store a known file.

## Vault (*implemented*, `crates/cloud/src/vault.rs`)

`vault.enc` holds the data key and the settings and credentials of the library's accounts. It is encrypted under a key
derived from the user's passphrase:

```
"LCV1" | u8 version = 1 | u8 kdf = 1 (Argon2id v1.3) | u16 0 | u32 LE m_cost (KiB) | u32 LE t_cost | u32 LE lanes
| salt (16) | nonce (12) | AES-256-GCM(JSON, aad = the 48 bytes before it)
JSON: {"format": "lightcraft-vault", "data_key": "<64 hex>", "accounts": [{"id", "provider", "label", "config"}]}
```

- New vaults use Argon2id with 64 MiB of memory, 3 passes and 1 lane.
- Readers accept 8 MiB–1 GiB of memory, 1–16 passes and 1–8 lanes. Anything outside that is refused before the key is
  derived, so a hostile vault can't make the app allocate unbounded memory.
- A vault larger than 1 MiB is refused.
- Changing the passphrase rewrites only the vault. Blobs stay wrapped under the same data key.

## Manifest (*implemented* in memory, `crates/cloud/src/manifest.rs`; replication is phase 3)

The manifest is an op log plus periodic snapshots, the same model as LightCraft's catalog journal.

**Ops.** Each op carries a stamp `{"lamport": n, "device": "<id>"}`. Stamps are ordered by clock, then by device id.
The op kinds are `create`, `rename`, `move`, `setContent`, `setAvailability`, `setTrashed`, `addReplica`,
`removeReplica` and `delete`.

**Merging.** Each field of an entry keeps the stamp of the op that last set it: existence, kind, name, parent, content
with size, availability, trashed, and each replica separately. An op changes only the fields whose stamp is older. As
a result, merging devices' ops in any order, even with duplicates, gives the same manifest.
- Concurrent edits of the same field resolve to the later stamp.
- If two ops share a stamp, the larger value wins.
- Keeping both versions of concurrently edited content needs per-entry history and is planned with multi-device
  support (phase 5).
- When concurrent moves put folders inside each other and form a cycle, the folders in the cycle are shown at the
  top level.

**Entries.** An entry as seen through the manifest:

```json
{"id": "p1", "kind": "file", "name": "IMG_0042.CR3", "parent": "f1", "content": "<content id>",
 "size": 25165824, "availability": "standard", "trashed": false,
 "replicas": {"<account id>": "<provider key>"}}
```

`key` is the provider's own reference for the object (a path, an object key, a file id), as returned when the object
was stored.

**Snapshots** are `{"format": "lightcraft-cloud-manifest", "version": 1, "manifest": {…}}` and keep every field's
stamp, so ops written after a snapshot still merge correctly. **Op logs** are JSON lines.

**Limits a reader checks:**
- ids (entry, device, account) are 1–128 bytes of `A-Z a-z 0-9 - _ .`;
- names are 1–1024 bytes with no control characters;
- provider keys are at most 2048 bytes;
- an entry has at most 64 replicas.

The manifest is copied to every account. If every copy is lost, an index can be rebuilt by listing `blobs/` on all
accounts. The plan is for each blob header to also carry its name and path, so a rebuild recovers those too.
