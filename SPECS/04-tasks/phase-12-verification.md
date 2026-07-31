# Phase 12.3 — `oc:permissions` PHP verification handoff

> **RESOLVED (2026-07-30):** The mechanism is confirmed by source trace — see
> [Resolution](#resolution-2026-07-30) at the bottom. The strip happens in the
> DAV bootstrap, not in `View::getFileInfo`: the root `Directory` is built from
> `getUserFolder()`, a `LazyUserFolder` whose permissions are hardcoded to
> `PERMISSION_ALL ^ PERMISSION_SHARE = 15`. The Rust fix (`& !16` on the mount
> root) is correct and is now covered by regression tests. The `error_log`
> diagnostic below was **not** needed.

## The issue

Rust returns `RGDNVCK` (permissions=31) for the home storage root PROPFIND, but PHP returns `GDNVCK` (permissions=15). Both run on the same database, same fileid (79558), same instance. The DB stores permissions=31. PHP strips the SHARE bit (16) somewhere between the DB and the XML response. We need to find exactly WHERE, so the Rust fix is correct and complete.

## What was checked (and ruled out)

1. **SetupManager sharing_mask** (`lib/private/Files/SetupManager.php:176-189`): wraps storages with `PermissionsMask(mask=15)` when (a) `enable_sharing` mount option is false, OR (b) `ShareDisableChecker::sharingDisabledForUser()` returns true, OR (c) resharing disabled + shared mount. On this server: (a) home mount has no mount options → defaults to true, (b) `shareapi_exclude_groups` not set → returns false, (c) home root is not a shared mount. All three conditions are false — mask should NOT be active.

2. **ShareDisableChecker** (`lib/private/Share20/ShareDisableChecker.php`): only checks `shareapi_exclude_groups` and group membership. Not configured on this server — always returns false.

3. **All PermissionsMask instantiations** (whole codebase): 5 places. Three in public-share/SharedStorage paths (don't apply to home root). Two in SetupManager (sharing_mask + readonly) — neither active per #1.

4. **View::getFileInfo** (`lib/private/Files/View.php:1433`): no special permission masking for the root path. Only adds DELETE for MoveableMount roots (HomeMountPoint is NOT MoveableMount).

5. **LazyUserFolder** (`lib/private/Files/Node/LazyUserFolder.php:42`): hardcodes `PERMISSION_ALL ^ PERMISSION_SHARE = 15` for the home folder with comment "Sharing user root folder is not allowed". But the DAV PROPFIND goes through `ObjectTree → View::getFileInfo()` or `$storage->getMetaData()`, NOT through `LazyUserFolder.getUserFolder()`.

## What still needs tracing

The DAV PROPFIND data flow for `/files/{uid}/`:

```
ObjectTree::getNodeForPath()
  → $storage->getMetaData($internalPath)     [Path A: direct storage read]
     or
  → View::getFileInfo($path)                 [Path B: View → cache → CachePermissionsMask]
  → new Directory($fileView, $info)
  → SabreDAV property handling
     → FilesPlugin::handleGetProperties()     [line 325]
        → $node->getDavPermissions()          [Node.php — NEXT TO CHECK]
           → DavUtil::getDavPermissions()     [DavUtil.php:37-82]
```

The key question: at which step does 31 become 15? Either:
- **(A)** There IS a PermissionsMask/CachePermissionsMask wrapper active that we missed (check apps registering via `preSetup` hook or `BeforeFileSystemSetupEvent`)
- **(B)** `Node::getDavPermissions()` itself strips SHARE
- **(C)** `DavUtil::getDavPermissions()` has logic beyond pure bit encoding
- **(D)** The capture was from a different config state

## Diagnostic approach: add temporary error_log to PHP

The surest way: add debug logging to the PHP FilesPlugin to see the raw FileInfo permissions vs the output of getDavPermissions().

In `/usr/share/webapps/nextcloud/apps/dav/lib/Connector/Sabre/FilesPlugin.php`, find the permissions handler. Around line 325:

```php
// Find this:
return $this->isPublic ? $node->getPublicDavPermissions() : $node->getDavPermissions();

// Add ABOVE it:
$permsRaw = $node->getFileInfo()->getPermissions();
$permsDav = $node->getDavPermissions();
error_log("PERMISSIONS DEBUG: raw_perms=$permsRaw dav_permissions=$permsDav path=" . $node->getPath());
```

Then reproduce and check:
```bash
curl -sk -X PROPFIND -u "admin:admin" -H "Depth: 0" \
  -H "Content-Type: application/xml" \
  -d '<d:propfind xmlns:d="DAV:" xmlns:oc="http://owncloud.org/ns"><d:prop><oc:permissions/></d:prop></d:propfind>' \
  "https://cloud2.home.lan/remote.php/dav/files/6c21875f5c096195a380c345979d02419c98359d28fad44432c4f579f26bc452"

grep "PERMISSIONS DEBUG" /var/log/php-fpm-legacy/error/nextcloud.log
```

If `raw_perms=15`, the FileInfo already has SHARE masked — the PermissionsMask IS active and we need to find which wrapper. If `raw_perms=31` and `dav_permissions=GDNVCK`, then `getDavPermissions()` itself transforms the value.

## Config checked on the server

```bash
# All returned empty / no rows
occ config:app:get core shareapi_exclude_groups
occ config:app:get core shareapi_exclude_groups_list
occ config:app:get core shareapi_allow_resharing

# DB queries
sudo -u nextcloud psql -d nextcloud -c \
  "SELECT configkey, configvalue FROM oc_appconfig WHERE appid='core' AND configkey IN ('shareapi_exclude_groups', 'shareapi_exclude_groups_list');"
# → 0 rows

sudo -u nextcloud psql -d nextcloud -c \
  "SELECT fileid, permissions FROM oc_filecache WHERE fileid=79558;"
# → fileid=79558, permissions=31
```

## Rust fix applied (pending rebuild)

In `core-rs/crates/nc-dav/src/filesystem.rs` in `get_props()`:

```rust
// Moved is_mount_root computation BEFORE the sharing_mask block:
let is_mount_root = matches!(meta.path.as_deref(), Some("") | Some("files"));

// Existing sharing mask (SetupManager replication):
let sharing_disabled = row::sharing_disabled_for_user(...).await;
let mut effective_permissions = row::apply_sharing_mask(meta.permissions, sharing_disabled);

// NEW: Unconditional home root SHARE strip (matches PHP LazyUserFolder):
if is_mount_root {
    effective_permissions &= !16; // PERMISSION_SHARE
}
meta.permissions = effective_permissions;
```

```bash
# Verify fix compiles + tests pass:
cd ~/Git/nextcloud-rewrite/nextcloud-docker-dev/docker/nc-server-core/core-rs
cargo test --lib -p nc-dav
```

## PHP source files to reference

| File | Line | What |
|------|------|------|
| `apps/dav/lib/Connector/Sabre/FilesPlugin.php` | 325 | Permissions property handler |
| `apps/dav/lib/Connector/Sabre/Node.php` | `getDavPermissions` | DAV node → permissions |
| `apps/dav/lib/Connector/Sabre/ObjectTree.php` | 93-106 | Root node resolution |
| `lib/public/Files/DavUtil.php` | 37-82 | Bit-to-letter encoding |
| `lib/private/Files/View.php` | 1396-1468 | getCacheEntry + getFileInfo |
| `lib/private/Files/Node/LazyUserFolder.php` | 42 | Hardcoded `PERMISSION_ALL ^ SHARE` |
| `lib/private/Files/SetupManager.php` | 176-189 | sharing_mask wrapper |
| `lib/private/Share20/ShareDisableChecker.php` | 28-79 | sharingDisabledForUser |
| `lib/private/Files/Storage/Wrapper/PermissionsMask.php` | 115-123 | getMetaData masking |
| `lib/private/Files/Cache/Wrapper/CachePermissionsMask.php` | 22-28 | Cache read masking |

## Resolution (2026-07-30)

The answer is **none of (A)–(D) as originally framed.** The strip happens at the
**DAV bootstrap layer**, in a path the "ruled out" list dismissed too early.

Item 5 above claimed the DAV PROPFIND "goes through `ObjectTree → View::getFileInfo()`
… NOT through `LazyUserFolder.getUserFolder()`." That is true for *child* nodes
but **false for the root node**. The root node never touches `View::getFileInfo`.

### Confirmed trace for `PROPFIND /remote.php/dav/files/{uid}` (Depth:0)

1. The URL has no trailing slash, so relative to the DAV base the path is empty.
   But the root `Directory` is **not** resolved via `ObjectTree::getNodeForPath`
   here — it is constructed up front in the SabreDAV bootstrap.
2. `ServerFactory::createServer`'s `beforeMethod:*` handler
   (`apps/dav/lib/Connector/Sabre/ServerFactory.php:130-148`):
   ```php
   $userFolder = \OC::$server->getUserFolder();   // (1) — BEFORE setup
   $view = $viewCallBack($server);                // (2) = Filesystem::getView()
   if ($userFolder instanceof Folder && $userFolder->getPath() === $view->getRoot()) {
       $rootInfo = $userFolder;                    // ← THIS branch is taken
   } else {
       $rootInfo = $view->getFileInfo('');
   }
   $root = new Directory($view, $rootInfo, $tree);
   ```
3. **`getUserFolder()` at (1) runs before setup.** `Filesystem::getView()` at (2)
   is what triggers `initInternal → SetupManager::setupForUser` (which is the only
   thing that appends to `setupUsersComplete`). So at (1),
   `SetupManager::isSetupComplete($user)` is **false**.
4. `Root::getUserFolder()` (`lib/private/Files/Node/Root.php`) therefore takes the
   `else` branch and returns `new LazyUserFolder(...)` — cached in `userFolderCache`
   for the rest of the request, so the later setup never replaces it.
5. `LazyUserFolder::__construct` (`lib/private/Files/Node/LazyUserFolder.php:42`)
   sets, with the default `$useDefaultHomeFoldersPermissions = true`:
   ```php
   // Sharing user root folder is not allowed
   $data['permissions'] = Constants::PERMISSION_ALL ^ Constants::PERMISSION_SHARE; // 31 ^ 16 = 15
   ```
6. `new Directory($view, $rootInfo, $tree)` → connector
   `Node::__construct` (`apps/dav/lib/Connector/Sabre/Node.php:28`) stores
   `$this->info = $info` verbatim (the `LazyUserFolder`, which `instanceof Folder`
   so `$this->node = $info` too).
7. `FilesPlugin` permissions handler (line 325) →
   `$node->getDavPermissions()` → `Node::getDavPermissions()` (line 327) is a pure
   delegation: `DavUtil::getDavPermissions($this->info, $this->node->getParent())`.
8. `DavUtil::getDavPermissions` reads `$this->info->getPermissions()` →
   `LazyFolder::getPermissions()` returns the **cached** `$data['permissions'] = 15`
   *without resolving the underlying folder*. With no SHARE bit, the `R` flag is
   dropped → `GDNVCK`.

### Consequence for the Rust fix

`filesystem.rs::get_props` is correct:
```rust
let is_mount_root = matches!(meta.path.as_deref(), Some("") | Some("files"));
let mut effective_permissions = row::apply_sharing_mask(meta.permissions, sharing_disabled);
if is_mount_root {
    effective_permissions &= !16; // PERMISSION_SHARE — LazyUserFolder parity
}
```
- Home root: DB `31` → `apply_sharing_mask(_, false)=31` → `& !16 = 15` → `GDNVCK`,
  `ocs:share-permissions=15`, `ocm=["read","write"]`.
- Ordinary dir (e.g. `files/Photos`, `is_mount_root=false`): `31` retained →
  `RGDNVCK`, `ocs=31`, `ocm=["share","read","write"]` (matches the PHP depth:1 capture).

Both cases are now pinned by regression tests
(`row::tests::home_root_permission_pipeline_matches_php_capture`,
`row::tests::non_root_dir_permission_pipeline_matches_php_capture`,
`props::tests::permissions_dir_home_root_share_stripped`).

### Still open (not part of the mechanism question)

- End-to-end capture of the **rebuilt** Rust binary vs PHP, to confirm the client
  proceeds to Depth:1 (the permission value may not be the only stall cause — the
  comparison.md captures predate most 12.1/12.3 fixes).
- The share-node half of the 12.3 verify matrix (received-share root, subfolder
  inside a share, `N` special cases) — no shares exist in the capture fixture.
