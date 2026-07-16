## 1) Stand up the HTTP server skeleton

### Coding steps
1. Create a Rust HTTP server exposing the same entry points:
	- `/index.php`
	- `/remote.php/{service}/...`
	- `/public.php/{service}/...`
	- `/ocs/v1.php/...`
	- `/ocs/v2.php/...`
	- `/status.php`
	- `/heartbeat`
	- `GET /ocs-provider/index.php` — JSON list of available OCS providers
	- `GET /.well-known/{service}` — webfinger, nodeinfo; DAV PROPFIND redirects for caldav/carddav (all PHP-FPM, but Rust must route them)
	- Login flow routes: `GET|POST /login/flow`, `GET /login/flow/grant`, `POST /login/v2/poll`, `GET /login/v2/flow/{token}`, `GET /login/v2/grant`, `POST /login/v2/apptoken` — all PHP-FPM
2. Add request/response tracing middleware (method, path, status, key headers, response time).
3. Return correct maintenance-mode behavior (503 + `X-Nextcloud-Maintenance-Mode: 1` + `Retry-After: 120`) and `/status.php` JSON from config/DB at startup.

### Verification steps
- `build/integration/features/maintenance-mode.feature`
- `build/integration/features/ocs-v1.feature`
- `build/integration/routing_features/apps-and-routes.feature`

---

---

Prev: [`03-db-schema-ownership-and-migrations.md`](03-db-schema-ownership-and-migrations.md) · Up: [`README.md`](README.md) · Next: [`05-implement-ocs-envelope-auth-behavior.md`](05-implement-ocs-envelope-auth-behavior.md)
