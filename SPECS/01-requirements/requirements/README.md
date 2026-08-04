# Requirements (REQ) — Sectioned

Split from the original `REQ.md` (Requirements: Nextcloud Core + Files in Rust (API-Compatible)). Each section is its own file for focused reading.

## Overview

This document captures the detailed requirements for reimplementing Nextcloud's **core** and **files** subsystems in Rust such that all existing clients (desktop sync, iOS, Android, WebDAV mounts, web UI) continue to work unchanged. PHP apps remain supported by forwarding requests to a PHP-FPM backend.

### ⚠ Requirement Gap: Cross-cutting concerns on the Rust-native files subtree

The original scope boundaries were drawn along **app/endpoint lines** — e.g. "the `files_trashbin` app and its `/dav/trashbin/…` subtree are PHP-FPM" (§1, §10). This is correct for *self-contained* apps, but it misses a whole class of functionality that is **not** self-contained: features implemented in PHP as **storage-wrappers**, **event listeners**, or **SabreDAV property/report plugins** that execute *inline* on the very requests the Rust server handles natively — `PROPFIND`, `PUT`, `DELETE`, `MOVE`, `COPY`, and `REPORT` on `/dav/files/{userId}/…`.

Because these run on the Rust-native path, they **cannot be proxied to PHP-FPM after the fact**: by the time the delegated app's own endpoint is reached, the Rust handler has already served (or must have already served) the request. The trash-bin-on-DELETE case (the first one found) is the archetype; tracing the same cross-cutting dataflow surfaces several more, many of which are exercised by the **web Files client** on every folder view.

| Cross-cutting concern | Triggered on (Rust-native) | Why it can't be a pure PHP-FPM delegation | Corrected req |
|---|---|---|---|
| **Trash bin** (move-to-trash) | `DELETE /dav/files/…` | Storage-wrapper intercepts `unlink()`; the move happens on the files endpoint, not `/dav/trashbin/…` | §6.7, §9.7 |
| **Cache propagation** (parent `etag`/`mtime`/`size`) | `PUT`/`DELETE`/`MOVE`/`COPY`/`MKCOL` | Core `Updater`→`Propagator`; **every** client (web + sync) detects changes by polling parent-folder ETags. Perf-critical, must be Rust. | §6.8 |
| **File versions** (copy-on-overwrite) | `PUT` (overwrite) + `MOVE`/`COPY` | `files_versions` storage-wrapper/listeners preserve prior content *before* the write completes | §6.9 |
| **Favorites & tags** (`{oc:}favorite`, `{oc:}tags`) | `PROPFIND`/`PROPPATCH` on files tree | `TagsPlugin` runs during PROPFIND; web client shows/sets stars inline | §6.5, §9.9 |
| **Share badges** (`{oc:}share-types`, `{nc:}sharees`) | `PROPFIND` on files tree | `SharesPlugin`; data lives in `oc_share` (Rust already owns) | §6.5 |
| **Comments count** (`{oc:}comments-*`) | `PROPFIND` on files tree | `CommentPropertiesPlugin`; web client shows unread-comment badges | §6.5 |
| **System tags** (`{nc:}system-tags`) | `PROPFIND` on files tree | `SystemTagPlugin`; collaborative tags shown in web sidebar | §6.5 |
| **Filter REPORT** (`{oc:}filter-files`) | `REPORT` on `/dav/files/…` | `FilesReportPlugin` powers the web **Favorites / Tags / Recent** views | §6.10 |

Guiding rule for the corrections (per the project's latency-driven scope boundary): a concern is only pulled into Rust when it is triggered **inline on a Rust-native request**. Where the underlying data is already owned by Rust (`oc_share`, filecache, favorites tags), Rust computes it directly. Where a concern is *not* performance-sensitive and its data is owned by a PHP-FPM app (comments, system tags), Rust may satisfy the inline property by a read-only query (or a scoped call-back), while all **write** operations for that app stay delegated to PHP-FPM.

See §6.5, §6.7–§6.10, §9.7 and §9.9 for the corrected requirements.

---

## Sections

- [`01-scope.md`](01-scope.md) — 1. Scope
- [`02-http-entry-points.md`](02-http-entry-points.md) — 2. HTTP Entry Points
- [`03-status-php.md`](03-status-php.md) — 3. /status.php
- [`04-authentication.md`](04-authentication.md) — 4. Authentication
- [`05-ocs-api.md`](05-ocs-api.md) — 5. OCS API
- [`06-webdav-dav.md`](06-webdav-dav.md) — 6. WebDAV / DAV
- [`07-upload-flows.md`](07-upload-flows.md) — 7. Upload Flows
- [`08-files-app-rest-endpoints.md`](08-files-app-rest-endpoints.md) — 8. Files App REST Endpoints
- [`09-database-schema.md`](09-database-schema.md) — 9. Database Schema
- [`10-php-fpm-integration.md`](10-php-fpm-integration.md) — 10. PHP-FPM Integration
- [`11-quota-enforcement.md`](11-quota-enforcement.md) — 11. Quota Enforcement
- [`12-filename-validation.md`](12-filename-validation.md) — 12. Filename Validation
- [`13-checksum-support.md`](13-checksum-support.md) — 13. Checksum Support
- [`14-special-dav-plugins.md`](14-special-dav-plugins.md) — 14. Special DAV Plugins
- [`15-security-headers.md`](15-security-headers.md) — 15. Security Headers
- [`16-caching-strategy.md`](16-caching-strategy.md) — 16. Caching Strategy
- [`17-logging-and-observability.md`](17-logging-and-observability.md) — 17. Logging and Observability
- [`18-configuration-file.md`](18-configuration-file.md) — 18. Configuration File
- [`19-compatibility-test-matrix.md`](19-compatibility-test-matrix.md) — 19. Compatibility Test Matrix
- [`20-non-functional-requirements.md`](20-non-functional-requirements.md) — 20. Non-Functional Requirements
- [`21-filecache-population.md`](21-filecache-population.md) — 21. `oc_filecache` Population and Self-Repair Lifecycle

Back: [`../README.md`](../README.md)
