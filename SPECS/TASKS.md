# Task Breakdown by Phase

Each task has a stated verification method. A task is done when its verification passes, not when the code is written.

Each phase lives in its own file for context-size management:

| Phase | File | Status |
|---|---|---|
| 0 — Foundation: DB, Migrations, Startup Caches | [PHASE-0.md](PHASE-0.md) | ✅ Complete |
| 1 — HTTP Skeleton: Routing and Maintenance Mode | [PHASE-1.md](PHASE-1.md) | ✅ Complete |
| 2 — OCS Envelope and Core Endpoints | [PHASE-2.md](PHASE-2.md) | ✅ Complete |
| 3 — Auth Stack | [PHASE-3.md](PHASE-3.md) | ✅ Complete |
| 4 — DAV Files Tree and Properties | [PHASE-4.md](PHASE-4.md) | 🔧 In progress |
| 5 — Upload Flows | [PHASE-5.md](PHASE-5.md) | ⬜ Not started |
| 6 — Files App HTTP APIs (STRETCH) | [PHASE-6.md](PHASE-6.md) | ⬜ Not started |
| 7 — PHP-FPM FastCGI Dispatch | [PHASE-7.md](PHASE-7.md) | ⬜ Not started |
| 8 — Load Validation and Starvation Regression | [PHASE-8.md](PHASE-8.md) | ⬜ Not started |

---

## Completion Gate

All phases are done when:
1. `cargo test --all-features` exits 0
2. All Behat suites in `build/integration/` exit 0 against the Rust server
3. All Cypress suites in `cypress/e2e/files/` and `cypress/e2e/core/` pass
4. `litmus` reports zero failures
5. Phase 8.1 load test passes at N = 2× PHP-FPM ceiling with 0% error rate
6. An existing PHP Nextcloud DB is connectable with no destructive migrations applied

Goal: the binary connects to an existing Nextcloud DB or creates a fresh one, and all process-lifetime caches are populated before the first request is served.

