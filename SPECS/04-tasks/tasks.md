# Task Breakdown by Phase

> LLM navigation: for smaller task-focused docs, start at [`README.md`](README.md).

Each task has a stated verification method. A task is done when its verification passes, not when the code is written.

Each phase lives in its own file for context-size management:

| Phase | File | Status |
|---|---|---|
| 0 — Foundation: DB, Migrations, Startup Caches | [phase-0.md](phase-0.md) | ✅ Complete |
| 1 — HTTP Skeleton: Routing and Maintenance Mode | [phase-1.md](phase-1.md) | ✅ Complete |
| 2 — OCS Envelope and Core Endpoints | [phase-2.md](phase-2.md) | ✅ Complete |
| 3 — Auth Stack | [phase-3.md](phase-3.md) | ✅ Complete |
| 4 — DAV Files Tree and Properties | [phase-4.md](phase-4.md) | 🔧 In progress |
| 5 — Upload Flows | [phase-5.md](phase-5.md) | ⬜ Not started |
| 6 — Files App HTTP APIs (STRETCH) | [phase-6.md](phase-6.md) | ⬜ Not started |
| 7 — PHP-FPM FastCGI Dispatch | [phase-7.md](phase-7.md) | ⬜ Not started |
| 8 — Load Validation and Starvation Regression | [phase-8.md](phase-8.md) | ⬜ Not started |
| 9 — Cross-Cutting Filesystem Concerns (Requirement-Gap Remediation) | [phase-9.md](phase-9.md) | ⬜ Not started |
| 10 — PHP-Parity Discrepancy Remediation | [phase-10.md](phase-10.md) | ⬜ Not started |
| 11 — Native Preview / Thumbnail Fast Path | [phase-11.md](phase-11.md) | ⬜ Not started |
| 16 — Differential Integration-Test Harness | [phase-16.md](phase-16.md) | ⬜ Not started |
| 18 — Performance improvements (flamegraph rounds 1-4) | [phase-18.md](phase-18.md) | ✅ Complete |
| 21 — DAV read-path round-trip reduction, Tier 1 | [phase-21.md](phase-21.md) | 🔧 In progress |

| Deferred improvements (nice-to-have) | [../02-specifications/improvements.md](../02-specifications/improvements.md) | ⬜ |

---

## Completion Gate

All phases are done when:
1. `cargo test --all-features` exits 0
2. All Behat suites in `build/integration/` exit 0 against the Rust server
3. All Cypress suites in `cypress/e2e/files/` and `cypress/e2e/core/` pass
4. `litmus` reports zero failures
5. Phase 8.1 load test passes at N = 2× PHP-FPM ceiling with 0% error rate
6. An existing PHP Nextcloud DB is connectable with no destructive migrations applied
7. Phase 9 cross-cutting concerns (trash-on-DELETE, ETag propagation, versions, favorites/tags, share/comment/system-tag props, `filter-files` REPORT) are implemented — required for the Cypress web-client suites in (3) to pass

Goal: the binary connects to an existing Nextcloud DB or creates a fresh one, and all process-lifetime caches are populated before the first request is served.
