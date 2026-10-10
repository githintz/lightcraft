# Distributed storage format (draft 0)

The layout LightCraft (`crates/cloud`) and the Storage Distributor web app use on a storage account, so either can
read what the other stored. **Draft:** sections marked *implemented* exist in `crates/cloud`; the others are the
agreed target and may still change before version 1. Design and rationale: [`cloud-storage.md`](cloud-storage.md).

## Accounts and the app folder

Each account has one **app folder** (a provider's app-folder scope, a bucket prefix, a WebDAV path, a local folder).
Nothing outside it is read or written. Paths below are relative to it.

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
| `vault.enc` | the encrypted vault (credentials of the other accounts, settings) |

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

## Encryption (target)

Optional per library, on by default. Each blob gets a random 256-bit file key and is encrypted with AES-256-GCM in
4 MiB plaintext segments (random reads and streaming without decrypting the whole blob):

```
header:  "LCB1" | u8 version=1 | u8 alg=1 (AES-256-GCM) | u32 segment_size (LE) | 12-byte nonce prefix
         | wrapped file key (AES-256-GCM under the vault key: 12-byte nonce | 32-byte key | 16-byte tag)
segment: ciphertext | 16-byte tag; nonce = nonce prefix (8 bytes) | u32 segment index (BE), final segment's
         index has the high bit set (truncation is detected)
```

The vault key is derived from the user's passphrase with Argon2id (parameters stored in `vault.enc`'s header). With
encryption on, blob ids are still plaintext SHA-256 (deduplication across devices); an option to name blobs by an
HMAC of the content under the vault key instead (so providers can't confirm known files) is open.

## Manifest (target)

An op log plus periodic snapshots, the same model as LightCraft's catalog journal. Each op carries a device id and a
Lamport timestamp; conflicting edits resolve last-writer-wins per field, and differing content for one file keeps both.
Entries:

```json
{"id": "…", "kind": "file", "name": "IMG_0042.CR3", "parent": "…", "size": 25165824,
 "mime": "image/x-canon-cr3", "content": "<content id>", "availability": "standard",
 "replicas": [{"account": "<account id>", "key": "<provider key>"}]}
```

`key` is the provider's own reference for the object (a path, an object key, a file id), as returned when it was
stored. The manifest is copied to every account. If every copy is lost, an index can be rebuilt by listing
`blobs/` on all accounts and reading each blob's encrypted header (target: the header also carries the name and path).
