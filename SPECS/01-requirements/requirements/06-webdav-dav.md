## 6. WebDAV / DAV

### 6.1 URL structure

All sub-trees are served by Nextcloud's SabreDAV server (`apps/dav`), which registers a `RootCollection` with named child trees. The Rust native handler serves only the **files** sub-tree; all other sub-trees are forwarded to PHP-FPM.

| URL | Handler | Purpose |
|---|---|---|
| `/remote.php/webdav/…` | **Rust native** | Authenticated WebDAV root (v1) — alias for `/dav/files/{userId}/` |
| `/remote.php/dav/files/{userId}/…` | **Rust native** | User file tree (DAV v2) |
| `/remote.php/dav/versions/{userId}/…` | PHP-FPM | File version history (`files_versions` app) |
| `/remote.php/dav/trashbin/{userId}/…` | PHP-FPM | Trash bin (`files_trashbin` app) |
| `/remote.php/dav/uploads/{userId}/…` | PHP-FPM | Chunked upload v2 staging area |
| `/remote.php/dav/comments/…` | PHP-FPM | File comments |
| `/remote.php/dav/calendars/…` | PHP-FPM | CalDAV |
| `/remote.php/dav/public-calendars/…` | PHP-FPM | Public CalDAV |
| `/remote.php/dav/system-calendars/…` | PHP-FPM | System CalDAV |
| `/remote.php/dav/addressbooks/…` | PHP-FPM | CardDAV |
| `/remote.php/dav/avatars/…` | PHP-FPM | User avatars |
| `/remote.php/dav/principals/…` | PHP-FPM | Principals tree (ACL/CalDAV/CardDAV) |
| `/dav/files/{userId}/…` | **Rust native** | User file tree |
| `/dav/versions/{userId}/…` | PHP-FPM | File version history |
| `/dav/trashbin/{userId}/…` | PHP-FPM | Trash bin |
| `/dav/uploads/{userId}/…` | PHP-FPM | Chunked upload v2 staging area |
| `/dav/comments/…` | PHP-FPM | File comments |
| `/dav/calendars/…` | PHP-FPM | CalDAV |
| `/dav/public-calendars/…` | PHP-FPM | Public CalDAV |
| `/dav/system-calendars/…` | PHP-FPM | System CalDAV |
| `/dav/addressbooks/…` | PHP-FPM | CardDAV |
| `/dav/avatars/…` | PHP-FPM | User avatars |
| `/dav/principals/…` | PHP-FPM | Principals tree |
| `/public.php/webdav/…` | PHP-FPM | Public share WebDAV |
| `/public.php/dav/…` | PHP-FPM | Public share DAV v2 |

### 6.2 RFC 4918 methods required

`OPTIONS`, `PROPFIND`, `PROPPATCH`, `GET`, `HEAD`, `PUT`, `DELETE`, `MKCOL`, `COPY`, `MOVE`, `LOCK`, `UNLOCK`

Additional Nextcloud methods:
- `PATCH` (checksum recalculation via `X-Recalculate-Hash` header)
- `POST` on `/dav/bulk` (bulk upload)
- `SEARCH` (file search via `SearchDAV` library — returns `207 Multi-Status`)
- `REPORT` (delta sync via RFC 6578 `sync-collection` — requires `{DAV:}sync-token` property on collections)

### 6.3 `dav-server-rs` integration

Use the `dav-server` crate (trait-based WebDAV implementation). Implement:

#### `DavFileSystem` trait

Methods: `read_dir`, `metadata`, `get_file`, `put_file`, `remove_file`, `remove_dir`, `create_dir`.

Storage backend: Nextcloud `oc_filecache` table + local/object storage.

Path resolution rules:
- `/dav/files/{userId}/{path}` → resolve `{path}` relative to the user's home storage root (storage ID `home::{userId}`, root path `files/`)
- Node IDs from `oc_filecache.fileid`
- Permissions from `oc_share` + `oc_filecache.permissions` (bitfield: READ=1, UPDATE=2, CREATE=4, DELETE=8, SHARE=16, ALL=31)

#### `DavProp` (via `DavFileSystem`)

See §6.5 for the full required property list.

#### `DavLockSystem` trait

Implement as a no-op (fake locking) — mirrors `FakeLockerPlugin` in SabreDAV:
- `LOCK` returns HTTP 200 with a `lockdiscovery` body containing a fake token derived from `md5(path)`, timeout 1800s.
- `UNLOCK` returns HTTP 204.
- `PROPFIND` for `{DAV:}lockdiscovery` returns an empty lock list.
- `PROPFIND` for `{DAV:}supportedlock` returns locks supported (shared + exclusive, depth infinity).
- Lock tokens in `If:` headers are always validated as valid (no actual locking state).

### 6.4 Response headers after write operations

After any successful `PUT`, `COPY`, `MOVE`, or chunked upload assembly:

| Header | Value |
|---|---|
| `OC-FileId` | `oc_filecache.fileid` as string |
| `ETag` | `"` + md5/hash + `"` |
| `OC-ETag` | same value as ETag (without quotes) |
| `X-OC-MTime: accepted` | only when `X-OC-MTime` header was honored |
| `X-OC-CTime: accepted` | only when `X-OC-CTime` header was honored |
| `OC-Checksum` | `ALGORITHM:hash` on GET responses when checksum is stored |
| `X-Accel-Buffering: no` | on all file GET responses (disables nginx buffering) |
| `Content-Disposition` | `attachment; filename*=UTF-8''...` or `attachment; filename="..."` |

### 6.5 DAV properties

#### Standard DAV properties

| Property | Notes |
|---|---|
| `{DAV:}resourcetype` | `collection` for directories, empty for files |
| `{DAV:}getcontentlength` | file size in bytes |
| `{DAV:}getcontenttype` | MIME type |
| `{DAV:}getetag` | quoted ETag string; **writable via PROPPATCH** (not protected) |
| `{DAV:}getlastmodified` | RFC 1123 date |
| `{DAV:}creationdate` | ISO 8601 atom date |
| `{DAV:}displayname` | node name; **read-only via PROPPATCH** (returns 403) |
| `{DAV:}quota-available-bytes` | free bytes for storage quota |
| `{DAV:}quota-used-bytes` | used bytes |
| `{DAV:}supportedlock` | fake lock types |
| `{DAV:}lockdiscovery` | empty (fake locker) |

#### OwnCloud namespace (`http://owncloud.org/ns` → `oc:`)

All the following are **read-only** (protected) unless noted:

| Property | Description |
|---|---|
| `{oc:}id` | Global file ID: `fileid` zero-padded to 8 chars + instance ID |
| `{oc:}fileid` | Raw numeric `oc_filecache.fileid` |
| `{oc:}permissions` | Encoded permissions string: R (read), W (write), CK (create), D (delete), S (shared), M (mounted), etc. Shares strip S and M for public links |
| `{oc:}size` | Recursive size (directories include children) |
| `{oc:}owner-id` | UID of file owner |
| `{oc:}owner-display-name` | Display name of owner (omitted or null for public links unless scope is published) |
| `{oc:}checksums` | `ALGORITHM:hash` list XML element |
| `{oc:}data-fingerprint` | Config value `data-fingerprint` |
| `{oc:}downloadURL` | Direct download URL (storage-specific) |
| `{oc:}share-permissions` (in `open-collaboration-services.org/ns`) | Integer bitmask of share permissions |
| `{oc:}share-permissions` (OCM, in `open-cloud-mesh.org/ns`) | JSON array of `read`, `write`, `share` |
| `{oc:}share-attributes` (in `http://nextcloud.org/ns`) | JSON share attributes |

#### Nextcloud namespace (`http://nextcloud.org/ns` → `nc:`)

| Property | Description |
|---|---|
| `{nc:}has-preview` | JSON `true`/`false` |
| `{nc:}mount-type` | Mount point type string (`local`, `shared`, `external`, etc.) |
| `{nc:}is-mount-root` | `"true"` if node's internal path is empty (shared root) |
| `{nc:}is-federated` | `"true"` if mount is a federated external share |
| `{nc:}metadata_etag` | ETag of associated metadata. **⚠ Known PHP bug:** `METADATA_ETAG_PROPERTYNAME` is defined in `FilesPlugin` but no `$propFind->handle()` call exists, so this property is never returned by PROPFIND in the reference PHP implementation. Implement it correctly in Rust (read from `oc_filecache_extended.metadata_etag`). |
| `{nc:}upload_time` | Unix timestamp of upload |
| `{nc:}creation_time` | Unix timestamp of creation; **writable via PROPPATCH** |
| `{nc:}note` | Share note from associated share |
| `{nc:}hide-download` | `"true"` if share has hide-download set |
| `{nc:}contained-folder-count` | Count of direct child directories |
| `{nc:}contained-file-count` | Count of direct child files |
| `{nc:}metadata-{key}` | Per-file metadata values from `oc_files_metadata`; writable based on `EDIT_REQ_*` permission level |
| `{nc:}hidden` | `"true"` if file is a live photo MOV companion |
| `{nc:}download-url-expiration` | Unix timestamp when the `{oc:}downloadURL` signed URL expires; absent if no direct download URL is configured. Protected (read-only). |
| `{DAV:}creationdate` | Also writable via PROPPATCH (mapped to `creation_time`) |
| `{DAV:}lastmodified` | Writable via PROPPATCH (updates mtime) |

### 6.6 PROPPATCH writable properties

| Property | Action |
|---|---|
| `{DAV:}lastmodified` | Update file mtime |
| `{DAV:}getetag` | Set custom ETag |
| `{DAV:}creationdate` | Set creation time (ISO 8601 parsed) |
| `{nc:}creation_time` | Set creation time (unix int) |
| `{nc:}metadata-{key}` | Update metadata value (permission-checked) |
| `{DAV:}displayname` | Return 403 (blocked) |

### 6.7 Trash bin on DELETE

DELETE on `/remote.php/dav/files/{userId}/{path}` (and equivalent `/dav/files/{userId}/{path}`) must **not** permanently delete the file. Instead, it must move the file to the trash bin, matching PHP's `TrashbinPlugin` which intercepts `unlink()` calls via the `files_trashbin` storage wrapper.

#### 6.7.1 Disk layout

The file is renamed on disk from:

```
{datadirectory}/{userId}/files/{relative_path}
```

to:

```
{datadirectory}/{userId}/files_trashbin/files/{relative_path}.d{timestamp}
```

For directories, the entire subtree is moved under the same `.d{timestamp}`-suffixed path.

`timestamp` is the current Unix timestamp at deletion time.

If a file with the same trash path already exists, append an incrementing suffix: `.d{timestamp}_1`, `.d{timestamp}_2`, etc.

#### 6.7.2 Filecache update

The `oc_filecache` row is **updated** (not deleted):

| Column | New value |
|---|---|
| `path` | `files_trashbin/files/{relative_path}.d{timestamp}` |
| `path_hash` | `MD5(new_path)` |
| `name` | Original basename with `.d{timestamp}` appended |
| `parent` | `fileid` of the `files_trashbin/files` directory (auto-created if missing) |
| `mtime` | `timestamp` (deletion time) |

The `oc_filecache_extended` row is left unchanged (still keyed by `fileid`).

#### 6.7.3 `oc_files_trash` table

One row is inserted per deleted file/directory:

| Column | Value |
|---|---|
| `auto_id` | auto-increment |
| `id` | `fileid` from `oc_filecache` (the deleted node's fileid) |
| `user` | UID of the deleting user |
| `timestamp` | Unix timestamp of deletion |
| `location` | Original `files/{relative_path}` (the path before deletion) |
| `type` | `'file'` or `'folder'` |
| `deleted_by` | UID of the user who performed the deletion (same as `user` for direct deletes; differs for share recipients) |

#### 6.7.4 Deletion from trash (permanent delete)

DELETE on `/remote.php/dav/trashbin/{userId}/{path}` is forwarded to PHP-FPM (existing route). PHP-FPM's `files_trashbin` app handles the permanent deletion from disk and removal from `oc_files_trash` + `oc_filecache`. The Rust handler does not need to implement permanent-delete logic — the trashbin DAV subtree is already routed to PHP-FPM (§6.1).

#### 6.7.5 Versioning interaction

When a file is deleted and moved to trash, any existing versions in `files_versions/` are preserved as-is by the `files_versions` app (PHP-FPM). The Rust handler does not interact with versions during trash moves.

---

---

Prev: [`05-ocs-api.md`](05-ocs-api.md) · Up: [`README.md`](README.md) · Next: [`07-upload-flows.md`](07-upload-flows.md)
