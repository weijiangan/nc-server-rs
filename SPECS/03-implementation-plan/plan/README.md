# Implementation Plan — Sectioned

Split from the original `IMPL_PLAN.md` (Rust Nextcloud Core+Files Implementation Plan). Each section is its own file for focused reading.

## Overview

## Sections

- [`01-repository-layout.md`](01-repository-layout.md) — Repository layout
- [`02-key-dependencies.md`](02-key-dependencies.md) — Key dependencies
- [`03-db-schema-ownership-and-migrations.md`](03-db-schema-ownership-and-migrations.md) — 0) DB schema ownership and migrations
- [`04-stand-up-the-http-server-skeleton.md`](04-stand-up-the-http-server-skeleton.md) — 1) Stand up the HTTP server skeleton
- [`05-implement-ocs-envelope-auth-behavior.md`](05-implement-ocs-envelope-auth-behavior.md) — 2) Implement OCS envelope + auth behavior
- [`06-implement-dav-service-routing-auth-stack.md`](06-implement-dav-service-routing-auth-stack.md) — 3) Implement DAV service routing + auth stack
- [`07-implement-dav-files-tree-properties.md`](07-implement-dav-files-tree-properties.md) — 4) Implement DAV files tree + properties
- [`08-implement-upload-flows-must-have-for-desktop-mobile-clients.md`](08-implement-upload-flows-must-have-for-desktop-mobile-clients.md) — 5) Implement upload flows (must-have for desktop/mobile clients)
- [`09-files-app-http-apis-stretch-goal.md`](09-files-app-http-apis-stretch-goal.md) — 6) Files app HTTP APIs — Stretch Goal
- [`10-php-app-support-via-fastcgi-dispatch.md`](10-php-app-support-via-fastcgi-dispatch.md) — 7) PHP app support via FastCGI dispatch
- [`11-load-validation-and-starvation-regression-test.md`](11-load-validation-and-starvation-regression-test.md) — 8) Load validation and starvation regression test
- [`12-existing-tests-you-can-directly-reuse.md`](12-existing-tests-you-can-directly-reuse.md) — Existing tests you can directly reuse
- [`13-future-considerations-architectural-evolution.md`](13-future-considerations-architectural-evolution.md) — Future Considerations: Architectural Evolution
- [`14-native-preview-thumbnail-fast-path.md`](14-native-preview-thumbnail-fast-path.md) — 9) Native preview / thumbnail fast path
- [`15-edge-security-hardening.md`](15-edge-security-hardening.md) — 10) Edge security hardening (PHP-FPM forwarding path)
- [`16-differential-integration-test-harness.md`](16-differential-integration-test-harness.md) — Differential integration-test harness (Rust core vs. PHP reference)
- [`17-caldav-carddav-build-vs-adopt.md`](17-caldav-carddav-build-vs-adopt.md) — CalDAV/CardDAV: build-vs-adopt exploration
- [`18-performance-measurement-and-profiling.md`](18-performance-measurement-and-profiling.md) — Performance measurement and profiling (benchmark harness + Rust profiling)
- [`19-performance-improvements.md`](19-performance-improvements.md) — Performance improvements from the Phase 17 flamegraphs
- [`20-performance-budget-gate.md`](20-performance-budget-gate.md) — Performance budget gate (query-count regression guard)
- [`21-propfind-round-trip-reduction.md`](21-propfind-round-trip-reduction.md) — DAV read-path round-trip reduction
- [`22-deployment-profile-tuning.md`](22-deployment-profile-tuning.md) — Deployment-profile tuning (2-core, HDD, localhost Postgres)
- [`23-write-path-batch-subtree-operations.md`](23-write-path-batch-subtree-operations.md) — Write-path batch: subtree-level operations (MOVE / DELETE-to-trash)

Back: [`../README.md`](../README.md)
