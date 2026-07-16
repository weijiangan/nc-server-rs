## 6. WebDAV / DAV

### 6.1 URL structure

All sub-trees are served by Nextcloud's SabreDAV server (`apps/dav`). Its `RootCollection` (`apps/dav/lib/RootCollection.php`) registers the static child trees `principals`, `files`, `calendars`, `system-calendars`, `public-calendars`, `addressbooks`, `systemtags`, `systemtags-relations`, `systemtags-current`, `comments`, `uploads`, `avatars`, and `provisioning`. Per-user app collections such as `trashbin` (`files_trashbin`) and `versions` (`files_versions`) are **not** static children — they are added at request time via `PluginManager::getAppCollections()` (`apps/dav/lib/Server.php`). The Rust native handler serves only the **files** sub-tree; all other sub-trees are forwarded to PHP-FPM.

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
- `SEARCH` (file search via the `SearchDAV` library — issued at the DAV **root**, returns `207 Multi-Status`; see §6.11)
- `REPORT` (`{http://owncloud.org/ns}filter-files` — powers the web Files app's **Favorites / Tags / Recent** views; see §6.10)

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

Implement as a no-op (fake locking) — mirrors `FakeLockerPlugin` in SabreDAV (PHP: `apps/dav/lib/Connector/Sabre/FakeLockerPlugin.php`; token = `md5($request->getPath())`, timeout `1800`):
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
| `OC-ETag` | same value as `ETag`, copied verbatim by `CopyEtagHeaderPlugin` — **includes** the surrounding quotes (not stripped) |
| `X-OC-MTime: accepted` | only when `X-OC-MTime` header was honored |
| `X-OC-CTime: accepted` | only when `X-OC-CTime` header was honored |
| `OC-Checksum` | `ALGORITHM:hash` on GET responses when checksum is stored |
| `X-Accel-Buffering: no` | on all file GET responses (disables nginx buffering) |
| `Content-Disposition` | `attachment; filename*=UTF-8''...` or `attachment; filename="..."` |

> **GET download-start cookie:** a `GET` carrying a `downloadStartSecret` query parameter (`<= 32` alphanumeric chars) sets a short-lived cookie `ocDownloadStarted={token}` (Max-Age 20, path `/`) so the web UI can detect that the download has begun (PHP `FilesPlugin::httpGet`).

> **MOVE/COPY mimetype:** when a file is moved or copied across an **extension change** (e.g. `note.txt` → `photo.jpg`), `oc_filecache.mimetype`/`mimepart` must be recomputed from the new name and updated (PHP `Files\Cache\Updater::copyOrRenameFromStorage`); otherwise `{DAV:}getcontenttype`, the icon/preview, and type filters go stale.

### 6.5 DAV properties

> **PHP source:** the core `oc:`/`nc:` properties below are provided by `apps/dav/lib/Connector/Sabre/FilesPlugin.php`. Several additional properties requested by the **web Files client** on every PROPFIND are provided by *separate* SabreDAV plugins registered on the same (Rust-native) files tree — see §6.5.1. All of these must be produced by the Rust PROPFIND handler.

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
| `{oc:}permissions` | Encoded permissions string built by `DavUtil::getDavPermissions()` (`lib/public/Files/DavUtil.php`), appended in order: `S` shared, `R` shareable (`PERMISSION_SHARE`), `M` mounted, `G` readable (`PERMISSION_READ`), `D` deletable, `NV` renameable+movable (`PERMISSION_UPDATE`), then `W` for a writable **file** (`PERMISSION_UPDATE`) or `CK` for a creatable **folder** (`PERMISSION_CREATE`). `FilesPlugin` strips `S` and `M` for public-link shares. |
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
| `{nc:}has-preview` | JSON `true`/`false` — **computed** from preview availability (`IPreview::isAvailable(mimetype)`, gated on `enable_previews`), not a constant. `true` for previewable types (images, video, PDF, office/OpenDocument, SVG, …). The web Files app reads this to decide whether to request a thumbnail. |
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

#### 6.5.1 Web-client PROPFIND properties from delegated-app plugins

The web Files app's default PROPFIND (`getDefaultPropfind()` in `@nextcloud/files/dav`, registered via `apps/files/src/init.ts`) requests the properties below. In PHP they are served by plugins that belong to apps categorised as PHP-FPM (`comments`, `systemtags`) or to `apps/dav` itself, **but they run inline during PROPFIND on the files tree** — so the Rust handler must return them. They are not performance-critical individually, but the PROPFIND that carries them is on the hot path, so Rust computes them directly rather than round-tripping per-property.

| Property | PHP plugin (source) | Data source & handling in Rust |
|---|---|---|
| `{oc:}favorite` | `apps/dav/lib/Connector/Sabre/TagsPlugin.php` | Files tagging tables `oc_vcategory` + `oc_vcategory_to_object` (§9.9), category `_$!<Favorite>!$_`. `1`/`0`. **Writable via PROPPATCH** (star/unstar from web). Rust-native. |
| `{oc:}tags` | `apps/dav/lib/Connector/Sabre/TagsPlugin.php` | Same tables; list of personal (non-favorite) tag names. Writable via PROPPATCH. Rust-native. |
| `{oc:}share-types` | `apps/dav/lib/Connector/Sabre/SharesPlugin.php` | `oc_share` (already owned by Rust, §9.6). Bitmask list of share types on the node. Protected. |
| `{nc:}sharees` | `apps/dav/lib/Connector/Sabre/SharesPlugin.php` | `oc_share`. Recipient list. Protected. |
| `{oc:}comments-href` | `apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php` | Static href `…/dav/comments/files/{fileid}`. Rust can emit without querying. |
| `{oc:}comments-count` | `apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php` | `oc_comments`. Read-only count from a direct query; `0` when the node has no comments. Always served (the plugin is registered unconditionally for logged-in users). |
| `{oc:}comments-unread` | `apps/dav/lib/Connector/Sabre/CommentPropertiesPlugin.php` | `oc_comments` + `oc_comments_read_markers`. Read-only per-user unread count. Always served for the authenticated user. |
| `{nc:}system-tags` | `apps/dav/lib/SystemTag/SystemTagPlugin.php` | `oc_systemtag` + `oc_systemtag_object_mapping` (owned by the PHP `systemtags` app). Read-only list; Rust may query read-only. All tag **management** stays on PHP-FPM. |

> **Delegation note:** `comments` and `systemtags` remain PHP-FPM apps for all writes and management UIs. Only the read-only PROPFIND enrichment above is served by Rust, because the PROPFIND request itself is Rust-native and cannot be partially delegated. The property handlers in `apps/dav` are registered unconditionally for logged-in users, so these properties are always returned (a `0`/empty value when there is no data) — they are not gated on the `comments`/`systemtags` apps being enabled.

### 6.6 PROPPATCH writable properties

| Property | Action |
|---|---|
| `{DAV:}lastmodified` | Update file mtime |
| `{DAV:}getetag` | Set custom ETag |
| `{DAV:}creationdate` | Set creation time (ISO 8601 parsed) |
| `{nc:}creation_time` | Set creation time (unix int) |
| `{nc:}metadata-{key}` | Update metadata value (permission-checked) |
| `{oc:}favorite` | Star/unstar the node (writes `oc_vcategory_to_object`; see §6.5.1, §9.9) — `TagsPlugin` |
| `{oc:}tags` | Set the node's personal tag list (`TagsPlugin`) |
| `{DAV:}displayname` | Return 403 (blocked) |

> **Custom properties:** any PROPPATCH property **not** matched above is persisted to `oc_properties` (`userid`, `propertypath`, `propertyname`, `propertyvalue`, `valuetype`) by the SabreDAV `PropertyStorage` plugin (`CustomPropertiesBackend`) and returned on subsequent PROPFIND; a PROPPATCH-remove deletes the row. These rows must be cleaned up on file `DELETE` and re-pathed on `MOVE`. (Clients such as macOS Finder store extended attributes this way.)

### 6.7 Trash bin on DELETE

DELETE on `/remote.php/dav/files/{userId}/{path}` (and equivalent `/dav/files/{userId}/{path}`) must **not** permanently delete the file. Instead, it must move the file to the trash bin, matching PHP's `files_trashbin` storage wrapper which intercepts `unlink()` calls.

> **PHP source:** `apps/files_trashbin/lib/Storage.php` (the `unlink`/`rmdir` interception on the storage wrapper) and `apps/files_trashbin/lib/Trashbin.php` (`Trashbin::move2trash()` — disk move, filecache update, `oc_files_trash` insert).

The delete is a **hard delete (no trash)** when any of these hold (PHP `Storage::doDelete()`): the `files_trashbin` app is disabled for the user; the request carries `X-NC-Skip-Trashbin: true` (the web/desktop "delete permanently" action); the target is a `.part` file; the path is outside the user's `files/` subtree; or a `MoveToTrashEvent` listener vetoes it. An encryption failure during the move also falls back to a hard delete. Rust must honour at least the app-enabled flag and the `X-NC-Skip-Trashbin` header, and restrict trashing to the `files/` subtree.

#### 6.7.1 Disk layout

The file is renamed on disk from:

```
{datadirectory}/{userId}/files/{relative_path}
```

to:

```
{datadirectory}/{userId}/files_trashbin/files/{basename}.d{timestamp}
```

The trash name uses the **basename only** (PHP `pathinfo()`); the original directory structure is not encoded in it. For directories, the entire subtree is moved as a single unit under that `.d{timestamp}`-suffixed name.

`timestamp` is the current Unix timestamp at deletion time.

If a node with the same trash name already exists, the **timestamp is incremented** (`.d{timestamp+1}`, …) until it is unique. The name stays strictly `{basename}.d{timestamp}` — trash restore recovers the timestamp by splitting the name on `.d`, so no numeric suffix may be appended.

#### 6.7.2 Filecache update

The `oc_filecache` row is **updated** (not deleted):

| Column | New value |
|---|---|
| `path` | `files_trashbin/files/{basename}.d{timestamp}` |
| `path_hash` | `MD5(new_path)` |
| `name` | Original basename with `.d{timestamp}` appended |
| `parent` | `fileid` of the `files_trashbin/files` directory (auto-created if missing) |
| `mtime` | `timestamp` (deletion time) |

The `oc_filecache_extended` row is left unchanged (still keyed by `fileid`).

#### 6.7.3 `oc_files_trash` table

`Trashbin::move2trash()` inserts **only** the columns below (schema in §9.4). The nullable `type` and `mime` columns are **not** written by PHP and are left `NULL`. `timestamp` is a `VARCHAR(12)` and is bound as a **string** (PHP `createNamedParameter` defaults to `PARAM_STR`).

| Column | Value |
|---|---|
| `id` | Original **basename** of the deleted node (PHP `pathinfo()['basename']`) — not the fileid |
| `user` | UID of the trashbin owner |
| `timestamp` | Unix timestamp of deletion, stored as a **string** |
| `location` | Original parent directory relative to `files/` (PHP `pathinfo()['dirname']`, e.g. `Documents`; `.` for root-level items) |
| `deleted_by` | UID of the user who performed the deletion (same as `user` for direct deletes; differs for share recipients) |

> **Listing vs. this row:** the web trash view (`Helper::getTrashFiles`) lists items by scanning the `files_trashbin/files` directory in the filecache and parses each item's type and deletion time from the filecache mimetype and the `.d{timestamp}` filename — **not** from this table. This row is consulted only for the original **location** and `deleted_by` (used by *restore*). Consequently, a failed/omitted `oc_files_trash` insert is invisible in the trash listing but silently breaks restore-to-original-location (PHP falls back to the root).

#### 6.7.4 Deletion from trash (permanent delete)

DELETE on `/remote.php/dav/trashbin/{userId}/{path}` is forwarded to PHP-FPM (existing route). PHP-FPM's `files_trashbin` app handles the permanent deletion from disk and removal from `oc_files_trash` + `oc_filecache`. The Rust handler does not need to implement permanent-delete logic — the trashbin DAV subtree is already routed to PHP-FPM (§6.1).

#### 6.7.5 Versioning interaction

When a file is deleted, PHP `Trashbin::retainVersions()` **moves** its versions from `files_versions/{path}.v*` into `files_trashbin/versions/…` inline, as part of the trash move, so that restoring the file also restores its versions. Because this runs on the Rust-native DELETE path, Rust must reproduce it (or delegate via a synchronous hook) when the `files_versions` app is enabled — see §6.9 for the shared design decision.

### 6.8 Cache propagation on write (parent ETag / mtime / size)

> **PHP source:** `lib/private/Files/Cache/Updater.php` (`update()`, `remove()`, `copyOrRenameFromStorage()`) → `lib/private/Files/Cache/Propagator.php` (`propagateChange()`).

Every mutating operation on the files tree — `PUT`, `DELETE`, `MOVE`, `COPY`, `MKCOL`, chunked-upload assembly, `PROPPATCH` that changes mtime — must **propagate up the parent chain to the storage root**. This is core filecache mechanics (not an app), it is **performance-critical**, and it **must be implemented natively in Rust** — it cannot be delegated, because both the web client *and* every sync client detect changes by polling the ETag of parent folders. If propagation is missing, sync silently breaks (clients never see the change).

For the affected node's every ancestor (`parent` chain in `oc_filecache`, walking up to `fileid` of the storage root):

| Column | Update |
|---|---|
| `etag` | New opaque value (PHP uses `uniqid()`, one value shared by all ancestors of a single propagation). Skipped for storages implementing `IReliableEtagStorage`. |
| `mtime` | `GREATEST(mtime, {change_time})` |
| `size` | `GREATEST(size + {sizeDifference}, -1)` — applied **only** to rows whose `size` is already computed (`> -1`); unscanned folders (`-1`) are left untouched. When the storage is an encryption wrapper, `unencrypted_size` is adjusted the same way. |

Notes:
- The change time is clamped to "now"; for a delete the `sizeDifference` is the negative of the removed node's size, and for an overwrite it is `newSize − oldSize`.
- On `MOVE`/`COPY` across folders, propagation runs on **both** the source and target parent chains, but each `propagateChange()` carries `sizeDifference = 0` (ETag/mtime only). The immediate source and target parents' sizes are corrected by a folder-size **recalculation** (`Cache::correctFolderSize()`), not a signed delta.
- A single `propagateChange()` updates all of a node's ancestors in one `UPDATE` (`path_hash IN (…)`); within a request, changes across multiple files are batched (`beginBatch()`/`commitBatch()`).

### 6.9 File versions on overwrite (write-side of `files_versions`)

> **PHP source:** `apps/files_versions/lib/AppInfo/Application.php` (registers `NodeWrittenEvent`, `NodeDeletedEvent`, `BeforeNodeRenamedEvent`/`NodeRenamedEvent`, `BeforeNodeCopiedEvent`/`NodeCopiedEvent` listeners), `apps/files_versions/lib/Storage.php`, `apps/files_versions/lib/Listener/FileEventsListener.php`, `apps/files_versions/lib/Listener/VersionStorageMoveListener.php`.

This is the same class of gap as trash-on-DELETE (§6.7): `files_versions` is **not** a self-contained app. It is a storage-wrapper + event-listener that fires when a file is **overwritten** (`PUT` to an existing path — Rust-native), and when files are **moved/copied** (so versions follow the file). The read-side (browsing/restoring versions via `/dav/versions/…`) is correctly delegated to PHP-FPM (§6.1), but the write-side runs on the Rust files endpoint.

Required behaviour on `PUT` overwrite of an existing file (when the `files_versions` app is enabled):
- Before writing new content, copy the **previous** content to `{datadirectory}/{userId}/files_versions/{relative_path}.v{timestamp}` where `timestamp` is the old file's mtime. The full relative path is preserved (unlike trash, which flattens to the basename).
- No version is created for `.part` files, directories, or **empty (0-byte)** files (PHP `Storage::store()`), and a `CreateVersionEvent` listener may veto creation.
- Create/update the corresponding `oc_filecache` rows under `files_versions/` (auto-creating the `files_versions` folder if missing), so the versions PROPFIND (PHP-FPM) can enumerate them.
- On `MOVE`/`COPY` of a file, move/copy its `files_versions/{path}.v*` siblings alongside it.

> **Delegation decision required:** because the copy-on-overwrite must occur *inline, before* the Rust `PUT` completes, it cannot be a fire-and-forget delegation to PHP-FPM. Two viable designs: (a) Rust performs the version copy natively (mirrors trashbin), or (b) Rust exposes a synchronous internal hook the PHP shim implements. Option (a) is preferred for consistency with §6.7 and to keep the write path off PHP-FPM. This is flagged as an open design point, not yet finalised.

### 6.10 `filter-files` REPORT (web Favorites / Tags / Recent views)

> **PHP source:** `apps/dav/lib/Connector/Sabre/FilesReportPlugin.php`.

The web Files app's **Favorites**, **Tags**, and **Recent** sidebar views are not separate endpoints — they issue a `REPORT` with body `{http://owncloud.org/ns}filter-files` against `/dav/files/{userId}/` (a Rust-native collection). The Rust handler must implement it:

- Request body contains a `{oc:}filter-rules` block; supported rules:
  - `{oc:}favorite` — **presence** of this rule restricts results to the user's favorited nodes (from `oc_vcategory`, §9.9); the rule's value is not interpreted.
  - `{oc:}systemtag` (tag id, repeatable) — restrict to nodes carrying the system tag(s).
  - `{oc:}circle` (circle id) — restrict to nodes shared with a circle (delegated data; may return empty if circles are disabled).
- Optional `{DAV:}limit` with `{DAV:}nresults` (page size) and `{nc:}firstresult` (offset) for paging.
- A `{DAV:}prop` block lists the properties to return per match (same property set as §6.5 / §6.5.1).
- An **empty** `filter-rules` block must be rejected with `400 Bad Request` (matches PHP — an empty filter would scan all files).
- Filtering by a non-existent tag returns `412 Precondition Failed`.
- Response is `207 Multi-Status` with one `<d:response>` per matching node, scoped to the report target's subtree.

### 6.11 `SEARCH` method (DAV basic search)

> **PHP source:** the `SearchDAV` library's `SearchPlugin` (registered in `apps/dav/lib/Server.php`), with the Nextcloud backend `apps/dav/lib/Files/FileSearchBackend.php` defining the searchable schema and translating the query.

The web client and some third-party/desktop clients issue an HTTP `SEARCH` with a `{DAV:}searchrequest` → `{DAV:}basicsearch` body. The rules the Rust handler must honour:

- **Endpoint:** `SEARCH` is served at the DAV **arbiter root** (`/remote.php/dav/` and `/dav/`), **not** at `/dav/files/{userId}` — `getArbiterPath()` returns `''`. Each `{DAV:}scope` in the body carries a `path` relative to the arbiter (e.g. `/files/{userId}/Photos`) that must resolve to a **directory**, otherwise the request is rejected.
- **Queryable properties** (usable in the `{DAV:}where` clause): `{DAV:}displayname`, `{DAV:}getcontenttype`, `{DAV:}getlastmodified`, `{DAV:}creationdate`, `{nc:}upload_time`, `{oc:}size`, `{oc:}favorite`, `{oc:}fileid`, `{oc:}owner-id`, and dynamic `{nc:}metadata-{key}` for **indexed** metadata keys.
- **Select-only properties** (returnable in results but not searchable): `{DAV:}resourcetype`, `{DAV:}getcontentlength`, `{oc:}checksums`, `{oc:}permissions`, `{DAV:}getetag`, `{oc:}owner-display-name`, `{oc:}data-fingerprint`, `{nc:}has-preview`, `{oc:}id`.
- **Operators:** `eq`, `lt`, `lte`, `gt`, `gte`, `like` (contains), plus boolean `and`/`or`/`not`. A query with more than **100** operators (`OPERATOR_LIMIT`) is rejected with `400`.
- **`{oc:}owner-id`** may only be filtered to the **current user** (which limits the search to the home storage); any other value is rejected.
- **Paging / sort:** `{DAV:}limit` with a maximum result count (`0` = unlimited) and a first-result offset; `orderby` is supported, including on metadata keys.
- **Response:** `207 Multi-Status`, one `<d:response>` per match (href `/files/{userId}/{relativePath}`) carrying the requested property set.

---

---

Prev: [`05-ocs-api.md`](05-ocs-api.md) · Up: [`README.md`](README.md) · Next: [`07-upload-flows.md`](07-upload-flows.md)
