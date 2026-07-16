# Requirements (REQ) — Sectioned

Split from the original `REQ.md` (Requirements: Nextcloud Core + Files in Rust (API-Compatible)). Each section is its own file for focused reading.

## Overview

This document captures the detailed requirements for reimplementing Nextcloud's **core** and **files** subsystems in Rust such that all existing clients (desktop sync, iOS, Android, WebDAV mounts, web UI) continue to work unchanged. PHP apps remain supported by forwarding requests to a PHP-FPM backend.

### ⚠ Requirement Gap: Trash Bin on DELETE

The `files_trashbin` app was categorised as "out of scope / delegated to PHP-FPM" (§1, §10.5) and its DAV endpoint (`/remote.php/dav/trashbin/…`) was routed to PHP-FPM (§6.1). This correctly covers the *read-side* (listing and restoring deleted files), but **misses the write-side**: the trash bin is not a self-contained app — it is a storage-wrapper that intercepts every `unlink()` call across the filesystem. The act of moving a file to trash happens during `DELETE /remote.php/dav/files/{userId}/…` — the Rust-native handler — not on the trashbin endpoint. By the time a request reaches `/dav/trashbin/…`, the file is already expected to be in the trash.

This gap was introduced because the scope boundaries were drawn along app/endpoint lines (the trashbin *app* and its *DAV subtree* are PHP-FPM), without tracing the cross-cutting dataflow: the write path runs through the files endpoint, not the trashbin endpoint. The `oc_files_trash` table and the `files_trashbin/files/` storage layout were also absent from the database schema (§9).

See §6.7 and §9.7 for the corrected requirements.

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

Back: [`../README.md`](../README.md)
