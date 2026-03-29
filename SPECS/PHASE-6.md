# Phase 6 — Files App HTTP APIs — STRETCH GOAL

> **Deferred:** All `/apps/files/api/v1/` and OCS files endpoints are called by the web browser SPA and mobile apps, but none are on the sync hot path that causes starvation. In the interim these routes are forwarded to PHP-FPM via the Phase 7 catch-all, so functionality is preserved. Implement natively in Rust once Phases 0–5 and 7 are complete and verified.

Goal: the files app REST and OCS endpoints used by the web UI and extended clients work correctly.

---

### 6.1 REST endpoints under `/apps/files/api/v1/`
- [ ] `GET /thumbnail/{x}/{y}/{file+}` — return thumbnail (generate or fetch from cache)
- [ ] `POST /files/{path+}` — update file tags (REQ §8.1)
- [ ] `GET /recent/` — return recently modified files list
- [ ] `GET /stats` — return `{used, free, total}` for authenticated user's storage
- [ ] `GET /views` / `PUT /views/{view}/{key}` / `PUT /views` — view config read/write
- [ ] `GET /configs` / `PUT /config/{key}` — user config read/write
- [ ] `POST /showhidden`, `POST /cropimagepreviews`, `POST /showgridview`, `GET /showgridview` — UI preference toggles

**Verify:** Cypress `cypress/e2e/files/*.cy.ts` — navigation, settings, recent view tests pass.

### 6.2 OCS files endpoints
- [ ] `GET /directEditing` — available editors info
- [ ] `GET /directEditing/templates/{editorId}/{creatorId}` — templates for editor
- [ ] `POST /directEditing/open` / `POST /directEditing/create` — open/create via direct editor
- [ ] `GET /templates` / `GET /templates/fields/{fileId}` / `POST /templates/create` / `POST /templates/path` — template management
- [ ] `POST /transferownership` / `POST /transferownership/{id}` / `DELETE /transferownership/{id}` — ownership transfer
- [ ] `POST /openlocaleditor` / `POST /openlocaleditor/{token}` — local editor token

**Verify:** `build/integration/files_features/transfer-ownership.feature`. Cypress `cypress/e2e/files/*.cy.ts` — direct editing and template scenarios if present.
