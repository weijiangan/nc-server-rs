## 2. HTTP Entry Points

### 2.1 Routes Rust must serve natively

| URL pattern | Handler |
|---|---|
| `GET /status.php` | Status JSON |
| `GET /heartbeat` | 200 OK |
| `GET /index.php` (and clean URL equivalents) | PHP-FPM fallback (or page not found for API-only mode) |
| `/ocs/v1.php/…` | OCS v1 |
| `/ocs/v2.php/…` | OCS v2 |
| `GET /ocs-provider/index.php` | OCS provider discovery (JSON list of available providers) |
| `GET /core/preview` | Preview image by `fileId` (`PreviewController::getPreviewByFileId`) — native fast-path, Phase 11 |
| `GET /core/preview.png` | Preview image by path (`PreviewController::getPreview`) — native fast-path, Phase 11 |
| `/remote.php/{service}/…` | DAV service dispatch |
| `/public.php/{service}/…` | Public DAV dispatch |
| `/apps/files/…` | Files app (mix: REST native + PHP-FPM) |
| `GET /.well-known/{service}` | Well-known endpoints (webfinger, nodeinfo; DAV well-known redirects to CalDAV/CardDAV) |
| `GET /login/flow`, `POST /login/flow`, `GET /login/flow/grant` | Login flow v1 (app password generation) — PHP-FPM |
| `POST /login/v2/poll`, `GET /login/v2/flow/{token}`, `GET /login/v2/grant`, `POST /login/v2/apptoken` | Login flow v2 (token-based) — PHP-FPM |

### 2.2 `remote.php` service map

```
webdav  → dav/files/{userId}      (authenticated WebDAV v1 root)
files   → dav/files/{userId}      (alias)
dav     → DAV v2 tree              (authenticated, full path resolution)
caldav  → PHP-FPM (dav app)
calendar→ PHP-FPM (dav app)
carddav → PHP-FPM (dav app)
contacts→ PHP-FPM (dav app)
direct  → PHP-FPM (dav app)
```

### 2.3 `public.php` service map

```
webdav  → public WebDAV (public-share auth flow)
dav     → public DAV v2 (public-share auth flow)
```

### 2.4 Required response headers on every DAV endpoint

```
Content-Security-Policy: default-src 'none';
```

### 2.5 Maintenance mode

When `maintenance = true` in config, all non-`/status.php` endpoints must return:
- HTTP 503
- `X-Nextcloud-Maintenance-Mode: 1`
- `Retry-After: 120`
- Body as OCS error envelope (for OCS endpoints) or plain text (for DAV)

When schema needs upgrade (`Util::needUpgrade`), DAV endpoints return HTTP 503 immediately.

---

---

Prev: [`01-scope.md`](01-scope.md) · Up: [`README.md`](README.md) · Next: [`03-status-php.md`](03-status-php.md)
