## Well-known endpoints

`core/Controller/WellKnownController.php` handles:

- `GET /.well-known/{service}` with header `X-NEXTCLOUD-WELL-KNOWN: 1`.
- Returns `404` with `{"message":"{service} not supported"}` when unknown.
- Setup checks expect:
  - `/\.well-known/webfinger` -> 200/400/404
  - `/\.well-known/nodeinfo` -> 200/404
  - `PROPFIND /.well-known/caldav` -> 207
  - `PROPFIND /.well-known/carddav` -> 207

These should map to DAV roots for CalDAV/CardDAV.

---

Prev: [`09-public-status-endpoint.md`](09-public-status-endpoint.md) · Up: [`README.md`](README.md) · Next: [`11-files-app-rest-api.md`](11-files-app-rest-api.md)
