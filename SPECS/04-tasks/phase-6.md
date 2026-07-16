# Phase 6 — Files App HTTP APIs — STRETCH GOAL

> **Decision — delegate to PHP-FPM (do not implement natively):** all `/apps/files/api/v1/` and OCS files endpoints are web-SPA / mobile routes; **none are on the sync hot path** and none are inline enrichment of a Rust-native request, so they are fully proxyable. They are served by the Phase 7 PHP-FPM catch-all, so functionality is preserved. Reimplementing them in Rust would duplicate working PHP with **no latency benefit**, so this phase stays delegated. Revisit **only** if removing PHP-FPM entirely becomes a goal (see the plan's "future architectural evolution"). *(Note: the `{nc:}metadata-{key}` PROPFIND/PROPPATCH property, previously pointed here from §4.9/§4.10, is **not** part of this phase — it is inline on the Rust-native DAV path and is now tracked in-place in Phase 4.)*

Goal: the files app REST and OCS endpoints used by the web UI and extended clients work correctly.

---

### 6.1 REST endpoints under `/apps/files/api/v1/`
- [ ] `GET /thumbnail/{x}/{y}/{file+}` — return thumbnail (generate or fetch from cache). *Native fast-path serving/generation is designed in [`phase-11.md`](phase-11.md); until then this route is served by the Phase 7 PHP-FPM catch-all.*
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
