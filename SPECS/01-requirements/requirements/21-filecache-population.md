# 21. `oc_filecache` Population and Self-Repair Lifecycle

> **PHP source basis:** all citations verified against the workspace PHP tree (commit `1a0ccac9…`) and the live dev-docker PostgreSQL on 2026-08-03.

`oc_filecache` (+ `oc_filecache_extended`) is the authoritative file-metadata store of the server. It is **not** a rebuild-on-demand cache: every request handler — WebDAV, the files app, shares, trash, versions, previews, search, quota — reads it, and the ETags it carries **are** the change-detection protocol for sync clients.

PHP keeps the table current through **four mechanisms**: one primary mechanism that runs **inline with every write**, and three secondary **self-repair** mechanisms that fix drift later, on read or on schedule. All four write to the same table. Any implementation that serves a files subtree natively while PHP continues to serve the rest of the product must leave the table exactly where mechanism 21.1 leaves it, because the repair mechanisms (21.3–21.5) only fire on **PHP** reads, fire **late**, and reconstruct only a subset of what the write path wrote (§21.6).

---

## 21.1 Primary mechanism — inline cache update on every write

Every storage mutation that goes through the `View` layer updates the cache **synchronously, before the operation returns**, via four protected helpers (`lib/private/Files/View.php`):

- `writeUpdate()` (`View.php:281`) → `Updater::update()`
- `removeUpdate()` (`View.php:290`) → `Updater::remove()`
- `renameUpdate()` (`View.php:296`) → `Updater::renameFromStorage()`
- `copyUpdate()` (`View.php:302`) → `Updater::copyFromStorage()`

All four are gated on `$this->updaterEnabled` (`View.php:273-279`, `disableCacheUpdate()`/`enableCacheUpdate()` — enabled by default).

Call sites in `View`:

| Operation | Call site | Notes |
|---|---|---|
| `file_put_contents` (`View.php:620`) | `writeUpdate` at `View.php:662` | |
| `rename` (`View.php:732`) | `writeUpdate` at `:839` (source storage, cross-storage case), `renameUpdate` at `:842` | |
| `copy` (`View.php:942`) | `copyUpdate` at `:1000` | |
| `fopen` (`View.php:1036`) | `writeUpdate` at `:1070` | fires when a writable stream is closed |
| `basicOperation` (`View.php:1204`) — `unlink`/`rmdir`/`mkdir`/`touch` | `:1249`, `:1254`, `:1257` | see below |

`basicOperation`'s dispatch rules (`View.php:1248-1258`):
- `delete` hooks → `removeUpdate()`.
- `write` hooks (except `fopen`/`touch`) → `writeUpdate($storage, $internalPath, null, $isCreateOperation ? $sizeDifference : null)` where `$isCreateOperation = ($operation === 'mkdir') || ($operation === 'file_put_contents' && in_array('create', $hooks))` and `$sizeDifference = ($operation === 'mkdir') ? 0 : $result` (bytes written). For **non-create** writes the size difference is left `null` and derived by the scanner (§21.1.1).
- `touch` hooks → `writeUpdate($storage, $internalPath, $extraParam, 0)` (`$extraParam` = the requested mtime).

### 21.1.1 `Updater::update()` — the anatomy of a write-path cache update

`lib/private/Files/Cache/Updater.php:68-94`. In order:

1. **Skip** when the updater is disabled or `Scanner::isPartialFile($path)` — `.part` files never touch the cache until assembled.
2. `$time = time()` if not given.
3. `$data = $this->scanner->scan($path, Scanner::SCAN_SHALLOW, -1, false)` — the node's own row is rebuilt **from storage metadata** (`Cache::put`, §21.2). The scan returns `oldSize`/`size`; if both are present, `$sizeDifference = size − oldSize`.
4. Encryption carve-out: if the scanned data is `encrypted`, `$sizeDifference = null` (the encryption module touches the cache itself).
5. If `$sizeDifference` is still `null` → `$this->cache->correctFolderSize($path, $data)` (full size recalculation fallback).
6. `$this->correctParentStorageMtime($path)` (`Updater.php:225-238`): reads the **parent directory's disk mtime** (`storage->filemtime(dirname($path))`) and writes it to the parent row's `storage_mtime`. A `DeadlockException` here is deliberately swallowed (logged at `info` — "with failures concurrent updates, someone else would have already done it … should at most only trigger an extra rescan").
7. `$this->propagator->propagateChange($path, $time, $sizeDifference ?? 0)` — the ancestor-chain ETag/mtime/size propagation, specified in §6.8.

### 21.1.2 `Updater::remove()` (`Updater.php:96-120`)

`Cache::remove($path)` (§21.2.4) → `correctParentStorageMtime($path)` → `propagateChange($path, time(), −$entry->getSize())` when the entry existed; otherwise `propagateChange($path, time())` plus `correctFolderSize($parent)`.

### 21.1.3 `Updater::renameFromStorage()` / `copyFromStorage()`

Both delegate to `copyOrRenameFromStorage()` (`Updater.php`), which:
- skips `.part` sources/targets;
- runs the operation closure (below);
- on an **extension change** (source and target file extensions differ, not a directory, not a `.d{timestamp}` trash name) recomputes `mimetype` from the new name and updates it (`$this->cache->update($fileId, ['mimetype' => …])`) — see §6.4;
- runs `correctFolderSize` on **both** source and target parents;
- runs `correctParentStorageMtime` on both, plus `updateStorageMTimeOnly($target)` (sets the target row's `storage_mtime` from disk with the `mtime = null` "do not overwrite mtime" magic, §21.2.3);
- runs `propagateChange($source, $time)` and `propagateChange($target, $time)` — both with `sizeDifference = 0` (ETag/mtime only); folder sizes are corrected by recalculation, not by signed delta (§6.8).

**Rename closure** (`Updater::renameFromStorage`, `Updater.php:122-135`): if a cache entry already exists at the target it is **removed first** ("Remove existing cache entry to no reuse the fileId"), then the source row is **moved**: `Cache::move($source, $target)` (same storage) or `Cache::moveFromCache($sourceCache, $source, $target)` (cross-storage). `Cache::moveFromCache` (`Cache.php:716`) re-points the **same row and the same `fileid`** to the new path/parent/name — this is how file identity survives renames — and dispatches `CacheEntryInsertedEvent` for the target afterwards (`Cache.php:842-843`).

**Copy closure** (`Updater::copyFromStorage`, `Updater.php:138-153`): ensures the target's parent is in the cache (scans it shallow if missing), then `Cache::copyFromCache($sourceCache, $sourceInfo, $target)` (`Cache.php:1223`): copies the source entry's data through `Cache::put` at the target path — the target gets a **new `fileid`** — resets permissions (`PERMISSION_ALL` for directories, `PERMISSION_ALL − PERMISSION_CREATE` for files), and recurses into folder contents.

### 21.1.4 The DAV `PUT` path bypasses `View` — and must do the same work explicitly

`apps/dav/lib/Connector/Sabre/File.php::put()`:

1. Content is written to a **part file** and assembled with `$storage->moveFromStorage($partStorage, $internalPartPath, $internalPath)` (storage level, no `View`).
2. Because `View` was bypassed, the handler calls the updater explicitly — `$storage->getUpdater()->update($internalPath)` (`File.php:337`, PHP comment: *"since we skipped the view we need to scan and emit the hooks ourselves"*).
3. `X-OC-MTime` header → `$this->fileView->touch($this->path, $mtime)` + `X-OC-MTime: accepted` response header (`File.php:346-352`).
4. `putFileInfo(['upload_time' => time()])` — **always**; plus `'creation_time' => $ctime` **only when the client sent `X-OC-CTime`** (`File.php:354-366`), answering `X-OC-CTime: accepted`.
5. `OC-Checksum` header → `putFileInfo(['checksum' => $checksum])` (`File.php:634`); a previously stored checksum is reset to `''` when the header is absent on an overwrite.

`View::putFileInfo()` (`View.php:1687-1710`) shallow-scans the path if it is not in the cache yet, then calls `Cache::put($internalPath, $data)` (§21.2.1).

**A DAV `GET` also writes:** `File::get()` compares the cached size with the filesystem size when opening the download and, on mismatch, logs *"fixing cached size of file id=…"* and runs `$this->getFileInfo()->getStorage()->getUpdater()->update($internalPath)` (`File.php:483-496`). This is a read path that writes — it self-heals size drift at download time.

### 21.1.5 Node-API creation path

`OC\Files\Node\Folder::newFile()` (`lib/private/Files/Node/Folder.php:163`) — used by apps creating files through the Node API — writes the content (`file_put_contents` or `touch`) and then stamps `putFileInfo($fullPath, ['creation_time' => time()])` (`Folder.php:181`), between the `preWrite/preCreate` and `postWrite/postCreate` hooks.

---

## 21.2 What the cache writes actually do (`Cache::put` / `insert` / `update` / `remove`)

`lib/private/Files/Cache/Cache.php`.

### 21.2.1 `put()` (`Cache.php:254-267`)
- Paths under `files_versions/` have `creation_time` **stripped** before writing — PHP comment: *"do not carry over creation_time to file versions, as each new version would otherwise create a filecache_extended entry with the same creation_time as the original file"*.
- Existing entry (`getId($file) > -1`) → `update($id, $data)`; otherwise `insert($file, $data)`.

### 21.2.2 `insert()` (`Cache.php:278-360`)
- Merges any saved **partial** data for the path (see below).
- Requires `size`, `mtime`, `mimetype`; if any is missing the data is parked as "partial" and `-1` is returned — completed on a later `insert`.
- Derives `path`, `path_hash`, `parent`, `name`; throws if the parent folder is not in the filecache.
- `normalizeData()` → `[$values, $extensionValues]`; INSERTs into `filecache`, then INSERTs into `filecache_extended` **only when `$extensionValues` is non-empty**.
- Dispatches `CacheEntryInsertedEvent` — both untyped (`CacheInsertEvent::class`) and typed (`Cache.php:331-333`). App listeners react to this event (part of the write-event cascade).

### 21.2.3 `update()` (`Cache.php:363-430`)
- The `filecache` UPDATE is **conditional**: it only runs for columns whose value actually differs or is currently `NULL` — no-op writes are skipped at the SQL level.
- Extended table: tries an INSERT of `$extensionValues`; on unique-constraint violation falls back to the same conditional UPDATE.
- `normalizeData()` (`Cache.php:446-487`) splits fields: main fields `path, parent, name, mimetype, size, mtime, storage_mtime, encrypted, etag, permissions, checksum, storage, unencrypted_size`; extension fields `metadata_etag, creation_time, upload_time`. Details:
  - `path` also sets `path_hash = md5(path)`; `mimetype` also resolves `mimepart`.
  - `storage_mtime` is **copied to `mtime`** unless an explicit `mtime` is present; passing `mtime = null` is the documented magic for *"do not copy storage_mtime to mtime"* / *"do not overwrite mtime"* (`Cache.php:453-456`, also used by `Updater::updateStorageMTimeOnly`).
  - The extension array is passed through `array_filter` — **falsy extension values (missing, `null`, `0`) are dropped**, so a partial extended write touches only the supplied columns.

### 21.2.4 `remove()` (`Cache.php:556+`)
Deletes the `filecache` row **and** its `filecache_extended` row (`Cache.php:567`); recursive folder removal walks depth-first deleting extended rows as it goes (`Cache.php:612`).

### 21.2.5 `oc_filecache_extended` defaults — live-verified behavior

The creating migration (`core/Migrations/Version17000Date20190514105811.php`) defines both `creation_time` and `upload_time` as `BIGINT NOT NULL DEFAULT 0`.

Consequence, verified against the live dev-docker DB (2026-08-03): a **bare DAV PUT of a fresh file without `X-OC-CTime`** leaves

```
creation_time = 0          -- column default; the extended write only carried upload_time
upload_time   = <request time>
```

`creation_time` becomes non-zero only via an explicit `X-OC-CTime` header on PUT (§21.1.4), the Node `newFile` path (§21.1.5), or a PROPPATCH of `{DAV:}creationdate` (§6.6). The two columns are independent — e.g. install-time skeleton copies show `creation_time != 0, upload_time = 0`.

---

## 21.3 Secondary mechanism — lazy scan on PHP read

`View::getCacheEntry()` (`View.php:1396-1420`), called from `getFileInfo()` (`View.php:1433`, call at `:1445`) and `getDirectoryContent()` (`View.php:1503`, call at `:1527`):

- If the path has **no cache row**, or its row has `size = -1` (unscanned): check `file_exists` on storage, then `Scanner::scan($internalPath, Scanner::SCAN_SHALLOW)` and reload.
- This path does **no propagation** — it materializes the node's own row only.

## 21.4 Secondary mechanism — Watcher repair on PHP read

Same function (`View.php:1396-1420`), the `elseif` branch: if the row exists and is scanned, and `Watcher::needsUpdate()` says the storage changed behind PHP's back:

1. Take a `LOCK_SHARED` on the file.
2. `$watcher->update($internalPath, $data)` — `Watcher.php:88-104`: shallow-scan (folder) or `scanFile` (file), `cleanFolder` if the row used to be a directory, then `correctFolderSize($path)`.
3. `$storage->getPropagator()->propagateChange($internalPath, time())` — **with `sizeDifference = 0`** and the current time, not the change time.
4. Reload and unlock. If the file is locked (`LockedException`), the stale cache entry is used silently.

`Watcher::needsUpdate()` (`lib/private/Files/Cache/Watcher.php:110-122`): default policy is `CHECK_ONCE` per request; returns true when the cached `storage_mtime` is `null` or `$this->storage->hasUpdated($path, $cachedData['storage_mtime'])` — i.e. the **disk mtime is newer than the cached `storage_mtime`**.

**Invariant:** the write path keeps `storage_mtime` in lockstep with the disk (§21.1.1 step 6, `updateStorageMTimeOnly`, the `storage_mtime → mtime` copy in `normalizeData`) precisely so this repair fires only for **out-of-band** changes. A write path that skips the `storage_mtime` update makes PHP's next read of the parent treat PHP's/Rust's own write as external drift and fire a spurious repair — an ETag bump at an arbitrary later moment, with `sizeDifference = 0` and `time = now`.

## 21.5 Secondary mechanisms — background scan and `occ files:scan`

- **`OCA\Files\BackgroundJob\ScanFiles`** (`apps/files/lib/BackgroundJob/ScanFiles.php`, registered in `apps/files/appinfo/info.xml:31`): a `TimedJob` running every **600 s** (`setInterval(60 * 10)`), processing up to `USERS_PER_SESSION = 500` users per run, disabled by the `files_no_background_scan` system config. It selects **only users whose storages have rows with `size = -1 AND parent > -1`** — i.e. unscanned folders — and runs `\OC\Files\Utils\Scanner::backgroundScan('')` for them. Its purpose is completing **partial scans**; it does not revisit scanned nodes, re-derive ETags, or touch `oc_filecache_extended`.
- **`occ files:scan`** (`apps/files/lib/Command/Scan.php`): manual scan via the same `\OC\Files\Utils\Scanner` (`:113`), recursive by default.
- `\OC\Files\Utils\Scanner` (`lib/private/Files/Utils/Scanner.php`): `backgroundScan($dir)` (`:121`) and `scan($dir, $recursive, $mountFilter)` (`:166`) walk **all mounts** of the user, unlike the per-storage `Scanner`.
- Scan-depth constants (`lib/public/Files/Cache/IScanner.php:19-29`): `SCAN_RECURSIVE = true`, `SCAN_SHALLOW = false`, `SCAN_RECURSIVE_INCOMPLETE = 2`.

## 21.6 What the repair paths can and cannot reconstruct

Because the secondary mechanisms (§21.3–21.5) rebuild state **from the disk**, only disk-derived facts are reconstructible:

**Reconstructible** (by a shallow scan of the node itself): `path`/`path_hash`/`name`/`parent`, `size`, `mtime`, `storage_mtime`, `mimetype`/`mimepart`, `permissions`.

**Not reconstructible by any repair path:**
- `upload_time` / `creation_time` — request-time facts injected by the handler (§21.1.4, §21.1.5); absent from storage metadata, defaulted to `0`.
- **Fileid continuity** — a repair scan of a missing row allocates a fresh `fileid` from the sequence; only the write path's `Cache::move`/`moveFromCache` preserves the row (and thus the identity clients, shares, and trash rely on) across renames (§21.1.3).
- **Propagation semantics** — timing, the shared single `uniqid()` ETag per propagation, and the signed size delta exist only on the write path (§6.8); Watcher repair propagates late, on PHP reads, with `sizeDifference = 0`.
- **Event artifacts** — the `CacheEntryInsertedEvent` dispatch on insert/move (`Cache.php:331-333`, `:842-843`) and whatever listeners do with it, `oc_files_versions` rows (§6.9), `oc_files_trash` rows (§6.7.3), preview-generation queueing.
- Repair paths only run when **PHP reads** the path. A path served exclusively by another handler is never repaired by them.

## 21.7 Requirement

Any implementation serving a files subtree natively must reproduce the **primary mechanism (§21.1) inline with each mutation** — including the `oc_filecache_extended` semantics of §21.1.4/§21.2.5 and the ancestor propagation of §6.8. The secondary mechanisms (§21.3–21.5) are PHP's self-healing for drift; they are not an alternative to the inline writes, they fire only on PHP reads, and they cannot reconstruct most of what the write path wrote (§21.6). The state left by native writes must make the PHP repair paths **no-ops**: no missing rows, no `size = -1` leftovers on scanned folders, and `storage_mtime` kept in lockstep with the disk so the Watcher (§21.4) never fires spuriously on natively-written paths.

Related: §6.8 (cache propagation on write), §6.7.2 (filecache update on trash move), §6.9 (version rows under `files_versions/`), §9.4 (`oc_filecache` / `oc_filecache_extended` schema).

---

Prev: [`20-non-functional-requirements.md`](20-non-functional-requirements.md) · Up: [`README.md`](README.md)
