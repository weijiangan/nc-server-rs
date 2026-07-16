## 6) Files app HTTP APIs — Stretch Goal

> **Deferred:** All `/apps/files/api/v1/` REST and OCS files endpoints are forwarded to PHP-FPM via the Phase 7 catch-all as an interim measure. None are on the sync hot path that causes starvation. Implement natively in Rust after Phases 0–5 and 7 are complete.

### Coding steps
1. Implement REST endpoints from `apps/files/appinfo/routes.php`:
	- thumbnail endpoint
	- recent files
	- storage stats
	- view/config toggles
2. Implement OCS files endpoints:
	- direct editing info/open/create
	- templates list/create/path
	- transfer ownership endpoints.

### Verification steps
- Cypress: `cypress/e2e/files/*.cy.ts` (navigation, sorting, search, download, settings, recent view).
- Integration: `build/integration/files_features/*`

---

---

Prev: [`08-implement-upload-flows-must-have-for-desktop-mobile-clients.md`](08-implement-upload-flows-must-have-for-desktop-mobile-clients.md) · Up: [`README.md`](README.md) · Next: [`10-php-app-support-via-fastcgi-dispatch.md`](10-php-app-support-via-fastcgi-dispatch.md)
