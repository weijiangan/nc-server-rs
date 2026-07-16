## Security and request validation

`SecurityMiddleware` enforces:

- Login required unless `PublicPage`.
- CSRF check except for `NoCSRFRequired` or OCS with `OCS-APIREQUEST: true` or
  `Authorization: Bearer ...`.
- Strict cookie checks unless `NoCSRFRequired` is set.
- Admin/sub-admin access for admin routes.
- IP-based admin access restrictions (`IRemoteAddress::allowsAdminActions()`).
- ExApp-required routes (`ExAppRequired` attribute) check the session `app_api` flag.

Rust reimplementation must keep the same semantics for these attributes, especially
the OCS CSRF exceptions and Bearer token bypass.

### CSRF and session cookies

Nextcloud uses a double-submit cookie + server-side token pattern:

- On first page load, a `requesttoken` is embedded in initial state (HTML or API).
- Subsequent mutating requests (non-GET, non-OCS-with-APIREQUEST) must include the
  token via the `requesttoken` form field or `OCS-APIREQUEST: true` header.
- Three cookies are set:
  - `nc_session_id` — the PHP session identifier.
  - `nc_token` — the persistent remember-me token (also triggers the SameSite check when present).
  - `nc_sameSiteCookielax` — dummy cookie with `SameSite=lax` (**lowercase** suffix in actual cookie name).
  - `nc_sameSiteCookiestrict` — dummy cookie with `SameSite=strict` (**lowercase** suffix in actual cookie name).
  - On HTTPS with path `/`, the `__Host-` prefix is prepended to the SameSite cookie names.
- The strict cookie check (`passesStrictCookieCheck`) verifies both `nc_sameSiteCookiestrict=true` and `nc_sameSiteCookielax=true` are present, providing CSRF protection for requests that carry cookies.
- Cookie check is only triggered when the request includes `session_name()` (PHP session cookie) or `nc_token`. Requests with neither cookie bypass the check entirely.
- `SameSiteCookieMiddleware` only applies to requests routed through `index.php`.
- OCS routes bypass the strict cookie check if `OCS-APIREQUEST: true` is present.
- `Authorization: Bearer ...` also bypasses CSRF checks (token-authenticated requests).

---

Prev: [`07-webdav-caldav-carddav.md`](07-webdav-caldav-carddav.md) · Up: [`README.md`](README.md) · Next: [`09-public-status-endpoint.md`](09-public-status-endpoint.md)
