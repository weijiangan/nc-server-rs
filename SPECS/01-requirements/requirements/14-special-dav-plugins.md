## 14. Special DAV Plugins

> All plugins below live under `apps/dav/lib/` in the PHP source (paths given per entry). §14.1–§14.7 are behavioural plugins. The **property** and **report** plugins that the web client depends on (`FilesPlugin`, `TagsPlugin`, `SharesPlugin`, `CommentPropertiesPlugin`, `SystemTagPlugin`, `FilesReportPlugin`) are specified with the file-tree requirements in [`06-webdav-dav.md`](06-webdav-dav.md) §6.5.1 and §6.10, since they execute inline on the Rust-native files subtree.

### 14.1 AppleQuirksPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/AppleQuirksPlugin.php`

Detect macOS DAV client user-agents (prefix `macOS/`) and fix a specific quirk: when a macOS Calendar or Contacts app sends a `{DAV:}principal-property-search` REPORT to a random principal collection **without** the `applyToPrincipalCollectionSet` flag, force-set the flag to `true`. This is not about stripping headers.

### 14.2 BlockLegacyClientPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/BlockLegacyClientPlugin.php`

Block clients outside a configured version range. Return `403 Forbidden` with an HTML body containing a link to download the supported client version.
- Below `minimum.supported.desktop.version` config (default `3.1.81`): blocked.
- Above `maximum.supported.desktop.version` config (default `99.99.99`): blocked.

### 14.3 RequestIdHeaderPlugin / UserIdHeaderPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/RequestIdHeaderPlugin.php`, `apps/dav/lib/Connector/Sabre/UserIdHeaderPlugin.php`

Add `X-Request-Id` and `X-Nextcloud-User-Id` headers to all responses for tracing.

### 14.4 CopyEtagHeaderPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/CopyEtagHeaderPlugin.php`

Mirror `ETag` value also as `OC-ETag` on every response that includes an `ETag`.

### 14.5 AnonymousOptionsPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/AnonymousOptionsPlugin.php`

Handles unauthenticated `OPTIONS` and `HEAD` requests from **Microsoft Office** user-agents (identified by `Microsoft Office` in the User-Agent string, with empty or bare-Bearer `Authorization`). Sets up a fake empty tree and returns a valid OPTIONS response so Office can probe the DAV endpoint without triggering an authentication popup. This is not a general CORS preflight handler.

### 14.6 DummyGetResponsePlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/DummyGetResponsePlugin.php`

Intercepts any `GET` request on the DAV tree (registered at priority 200). Returns HTTP 200 with plain-text body:
```
This is the WebDAV interface. It can only be accessed by WebDAV clients such as the Nextcloud desktop sync client.
```
No debug-mode condition. This prevents SabreDAV's built-in HTML directory browser from being shown to web browsers.

### 14.8 FilesDropPlugin

> **PHP source:** `apps/dav/lib/Files/Sharing/FilesDropPlugin.php`

Enforce upload-only restrictions on file-drop public shares served via `/public.php/dav`. Logic:
- Allowed methods: `PUT`, `MKCOL`, and `MOVE` (MOVE only for chunked upload assembly where the path starts with `/uploads/`).
- All other methods throw `MethodNotAllowed` (**HTTP 405**).
- Additional features: nickname header (`X-NC-Nickname`) support, automatic path rewriting to put files under offerer's subfolder, conflict resolution (deduplicating filenames), and transparent folder-creation for nested paths.

### 14.7 PropFindPreloadNotifyPlugin / PropfindCompressionPlugin

> **PHP source:** `apps/dav/lib/Connector/Sabre/PropFindPreloadNotifyPlugin.php`, `apps/dav/lib/Connector/Sabre/PropfindCompressionPlugin.php`

Optional optimisation: preload related nodes before PROPFIND depth-1 responses; compress PROPFIND response bodies with gzip if client accepts.

---

---

Prev: [`13-checksum-support.md`](13-checksum-support.md) · Up: [`README.md`](README.md) · Next: [`15-security-headers.md`](15-security-headers.md)
