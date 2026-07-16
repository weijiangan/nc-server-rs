## App compatibility considerations

### PHP apps (traditional)

Existing apps are PHP and rely on:

- OCP interfaces (service container, request, user/session, files, config).
- OCS routing and response envelopes.
- App loading lifecycle (`OC_App::loadApps`, appinfo/routes).

To run existing PHP apps without rewriting:

1. Provide a PHP execution environment with compatible OCP API bindings, or
2. Implement an AppAPI bridge (external apps) and migrate apps to it.

### AppAPI (external apps)

The `app_api` stubs show a pattern where external apps are called with signed
headers and validated via `AppAPIService::validateExAppRequestToNC`. If AppAPI
compatibility is desired, the Rust core should expose the same auth header
validation and `ExAppRequired` semantics (`SecurityMiddleware`).

---

Prev: [`12-public-sharing-endpoints.md`](12-public-sharing-endpoints.md) · Up: [`README.md`](README.md) · Next: [`14-configuration-values-influencing-api-behavior.md`](14-configuration-values-influencing-api-behavior.md)
