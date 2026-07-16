## OCS API compatibility

### Routing and format

- OCS endpoints are dispatched by `ocs/v1.php` and `ocs/v2.php`.
- Format defaults to XML (`format` query param or `Accept` header).
- `OCS-APIREQUEST: true` is required for CSRF bypass on OCS routes (see Security section).
- Content types:
  - XML: `text/xml; charset=UTF-8` (from `OCS\ApiHelper`).
  - JSON: `application/json; charset=utf-8`.

### Response envelope

OCS responses are wrapped as:

```json
{
  "ocs": {
    "meta": {
      "status": "ok|failure",
      "statuscode": 100|...,
      "message": "OK|error message",
      "totalitems": "...",
      "itemsperpage": "..."
    },
    "data": { ... }
  }
}
```

XML uses the same structure (`<ocs><meta>...</meta><data>...</data></ocs>`).

### Status code mapping

- OCS v1 (`OCS\V1Response`):
  - HTTP 200 for most responses; HTTP 401 for `RESPOND_UNAUTHORISED`.
  - OCS status `100` means OK; otherwise the OCS status equals the response status.
- OCS v2 (`OCS\V2Response`):
  - HTTP status equals the response status with mappings:
    - `RESPOND_UNAUTHORISED` -> 401
    - `RESPOND_NOT_FOUND` -> 404
    - `RESPOND_SERVER_ERROR` or `RESPOND_UNKNOWN_ERROR` -> 500
    - status <200 or >600 -> 400

### OCS format detection

- `format` query parameter takes precedence: `?format=json` or `?format=xml`.
- If absent, the `Accept` header is inspected; `application/json` selects JSON.
- Default is XML when neither is specified.
- `ocs/v1.php` vs `ocs/v2.php` is determined by the script name (see `OCS\ApiHelper::isV2`).

### Core OCS endpoints

These are served by `core/Controller/OCSController.php`:

- `GET /ocs/v1.php/config` and `/ocs/v2.php/config`
  - Returns `{ version: "1.7", website: "Nextcloud", host, contact, ssl }`.
- `GET /ocs/v1.php/cloud/capabilities` and `/ocs/v2.php/cloud/capabilities`
  - Returns `version` fields and `capabilities`.
  - Two separate capability providers contribute to `core`:
    - `OC\OCS\CoreCapabilities` (`lib/private/OCS/CoreCapabilities.php`):
      `core.pollinterval` (default 60), `core.webdav-root` (default `remote.php/webdav`),
      `core.reference-api: true`, `core.reference-regex`, `core.mod-rewrite-working`.
    - `OC\Core\AppInfo\Capabilities` (`core/AppInfo/Capabilities.php`):
      per-user fields `core.user.language`, `core.user.locale`, `core.user.timezone`
      and `core.can-create-app-token` (only when a user is authenticated).
  - Unauthenticated requests receive only `IPublicCapability` results.
  - The ETag of the capabilities response is set to `md5(json_encode($result))`.
- `POST /ocs/v1.php/person/check` and `/ocs/v2.php/person/check`
  - Uses `login`/`password`, returns status 200 with person data, or OCS status 101/102.
  - Protected by brute-force throttling (`@BruteForceProtection(action: 'login')`).
- `GET /ocs/v1.php/identityproof/key/{cloudId}` and v2 equivalent
  - Returns public key or 404.

### OCS auth headers

When responding with `RESPOND_UNAUTHORISED`, `OCS\ApiHelper` sets:

- `WWW-Authenticate: Basic realm="Authorisation Required"` (or DummyBasic for XHR).
- When `X-Requested-With: XMLHttpRequest` is set, value becomes `DummyBasic realm="Authorisation Required"`.

### Rate limiting (brute force)

- When `MaxDelayReached` is thrown, both `index.php` and `ocs/v1.php` return HTTP 429.
- `index.php`: if `Accept` does not include `html`, returns JSON `{"message": "..."}` with 429.
- `ocs/v1.php` / `ocs/v2.php`: returns OCS envelope with `Http::STATUS_TOO_MANY_REQUESTS`.
- The throttler adds a `Retry-After` header indicating when the client may retry.

---

Prev: [`04-routing-and-url-structure.md`](04-routing-and-url-structure.md) · Up: [`README.md`](README.md) · Next: [`06-login-flows-and-oauth2.md`](06-login-flows-and-oauth2.md)
