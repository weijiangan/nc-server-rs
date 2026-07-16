## 3. `/status.php`

Returns `Content-Type: application/json`, `Access-Control-Allow-Origin: *`, and:

```json
{
  "installed": true,
  "maintenance": false,
  "needsDbUpgrade": false,
  "version": "30.0.2.1",
  "versionstring": "30.0.2",
  "edition": "",
  "productname": "Nextcloud",
  "extendedSupport": false
}
```

All values read from `config/config.php` (system config) or DB (`oc_appconfig`). **`installed` and `maintenance` come from `config.php` via `SystemConfig::getValue()` — not from `oc_appconfig`.** Version strings (`oc_version`, `versionstring`) come from `oc_appconfig` under `core`.

---

---

Prev: [`02-http-entry-points.md`](02-http-entry-points.md) · Up: [`README.md`](README.md) · Next: [`04-authentication.md`](04-authentication.md)
