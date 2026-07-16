## 5. OCS API

### 5.1 Envelope format

Both XML and JSON are supported. Format selection:
1. `?format=xml` or `?format=json` query parameter (takes precedence)
2. `Accept: application/json` header
3. Default: XML

#### XML envelope (`Content-Type: text/xml; charset=UTF-8`)

```xml
<?xml version="1.0"?>
<ocs>
  <meta>
    <status>ok</status>
    <statuscode>100</statuscode>
    <message>OK</message>
    <totalitems></totalitems>
    <itemsperpage></itemsperpage>
  </meta>
  <data>…</data>
</ocs>
```

#### JSON envelope (`Content-Type: application/json; charset=utf-8`)

```json
{"ocs":{"meta":{"status":"ok","statuscode":100,"message":"OK"},"data":{…}}}
```

### 5.2 OCS v1 HTTP status mapping

| Condition | HTTP status |
|---|---|
| Any success | 200 |
| Unauthorised (OCS status 997) | 401 |
| Maintenance mode | 503 (exception override) |

OCS `statuscode` 100 = success; `status` field is `"ok"` when statuscode = 100, otherwise `"failure"`.

`totalitems` and `itemsperpage` are included as empty strings in v1 (not absent — existing clients depend on this).

In OCS **v2**, `totalitems` and `itemsperpage` are **omitted** from the meta block unless the handler explicitly sets them. Do not emit the keys as empty strings in v2.

### 5.3 OCS v2 HTTP status mapping

Maps OCS status codes directly to HTTP status codes:
- 200–299 → as-is
- 997 → 401
- 998 → 404
- 999 or unknown → 500
- Outside 200-600 → 400

### 5.4 Unauthorised OCS response headers

Browser requests (`X-Requested-With: XMLHttpRequest`):
```
WWW-Authenticate: DummyBasic realm="Authorisation Required"
```
Other requests:
```
WWW-Authenticate: Basic realm="Authorisation Required"
```

### 5.5 `OCS-APIREQUEST` header

When `OCS-APIREQUEST: true` is present, CSRF token verification is bypassed. This is used by all desktop/mobile clients.

### 5.6 Core OCS endpoints

All mounted at `/ocs/v1.php/` and `/ocs/v2.php/` with the same path suffix.

#### `GET /cloud/capabilities`

Returns merged capabilities from all registered providers. The Rust server must natively return capabilities for:

**core** (from `OC\OCS\CoreCapabilities`):
```json
{
  "core": {
    "pollinterval": 60,
    "webdav-root": "remote.php/webdav",
    "reference-api": true,
    "reference-regex": "…",
    "mod-rewrite-working": true
  }
}
```

**dav** (from `OCA\DAV\Capabilities`):
```json
{
  "dav": {
    "chunking": "1.0",
    "public_shares_chunking": true,
    "search_supports_creation_time": true,
    "search_supports_upload_time": true,
    "bulkupload": "1.0"
  }
}
```

**files** (from `OCA\Files\Capabilities`):
```json
{
  "files": {
    "bigfilechunking": true,
    "blacklisted_files": [],
    "forbidden_filenames": [],
    "forbidden_filename_basenames": [],
    "forbidden_filename_characters": [],
    "forbidden_filename_extensions": [],
    "chunked_upload": {
      "max_size": 10737418240,
      "max_parallel_count": 5
    },
    "file_conversions": []
  }
}
```

Capabilities registered by PHP apps (e.g. `files_sharing`, `provisioning_api`) must be merged in. Rust collects them from PHP-FPM at startup or on capability-invalidating config changes.

**Authentication state matters:** When the request is authenticated, return the full capability set from `getCapabilities()`. When unauthenticated, return only `IPublicCapability` results via `getCapabilities(true)`. The ETag of the response is `md5(json_encode($result))`.

#### `GET /ocs/v1.php/config`

```xml
<ocs>
  <meta>…</meta>
  <data>
    <version>1.7</version>
    <website>Nextcloud</website>
    <host>example.com</host>
    <contact>admin@example.com</contact>
    <ssl>false</ssl>
  </data>
</ocs>
```

#### `POST /person/check`

Login credential validation endpoint (used by ownCloud-compatible federation). Validates `login` + `password` against user database.

#### `GET /identityproof/key/{cloudId}`

Returns the server's public signing key for the given user's cloud ID.

---

---

Prev: [`04-authentication.md`](04-authentication.md) · Up: [`README.md`](README.md) · Next: [`06-webdav-dav.md`](06-webdav-dav.md)
