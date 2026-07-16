## Request lifecycle and global behavior

- Initialization and config loading happens in `lib/base.php` (`OC::handleRequest()`).
- `OC::checkInstalled()` redirects to `/index.php` when not installed.
- `OC::checkMaintenanceMode()` sends HTTP 503, header `X-Nextcloud-Maintenance-Mode: 1`,
  and `Retry-After: 120` for most routes.
- `OC::$WEBROOT` is derived from `SCRIPT_NAME`/`REQUEST_URI` and influences URL generation.
- `apps_paths` config affects app lookup and autoloading; missing apps folder is fatal.
- `htaccess.IgnoreFrontController` or `front_controller_active=true` removes `/index.php`
  from generated URLs, but clients still need to accept `/index.php` paths.
- `index.php` emits JSON for API clients when `Accept` does not include `html`
  (see login and brute-force error handling in `index.php`).

---

Prev: [`02-primary-entry-points.md`](02-primary-entry-points.md) · Up: [`README.md`](README.md) · Next: [`04-routing-and-url-structure.md`](04-routing-and-url-structure.md)
