## Public status endpoint

`/status.php` returns JSON:

```json
{
  "installed": true|false,
  "maintenance": true|false,
  "needsDbUpgrade": true|false,
  "version": "x.y.z",
  "versionstring": "x.y.z",
  "edition": "",
  "productname": "Nextcloud",
  "extendedSupport": true|false
}
```

Headers: `Access-Control-Allow-Origin: *`, `Content-Type: application/json`.

---

Prev: [`08-security-and-request-validation.md`](08-security-and-request-validation.md) · Up: [`README.md`](README.md) · Next: [`10-well-known-endpoints.md`](10-well-known-endpoints.md)
