## Routing and URL structure

### Route registration

- App routes are defined in `apps/*/appinfo/routes.php` and registered by `OC\Route\Router`.
- `RouteConfig` and `RouteParser` derive the actual URL prefix:
  - Default web routes for most apps use `/apps/{app}`.
  - Apps in the root whitelist (`cloud_federation_api`, `core`, `files_sharing`,
    `files`, `profile`, `settings`, `spreed`) can mount at `/`.
  - OCS routes are always whitelisted for root paths and use the OCS route collection.
- OCS routes are registered in a separate collection with `/ocsapp` prefix.
- `ocs/v1.php` and `ocs/v2.php` dispatch to routes by matching
  `'/ocsapp' . rawPathInfo` (see `ocs/v1.php`).

### Entry point URL prefixes

| Prefix | Notes |
| --- | --- |
| `/index.php` | May be omitted when front controller is active. |
| `/ocs/v1.php` and `/ocs/v2.php` | OCS dispatchers (map to `/ocsapp` routes). |
| `/ocs` | Router collection prefix for legacy OCS routes. |
| `/apps/{app}` | Default app web routes. |
| `/remote.php` | DAV and remote services. |
| `/public.php` | Public DAV and sharing. |

---

Prev: [`03-request-lifecycle-and-global-behavior.md`](03-request-lifecycle-and-global-behavior.md) · Up: [`README.md`](README.md) · Next: [`05-ocs-api-compatibility.md`](05-ocs-api-compatibility.md)
