## Login flows and OAuth2

### Login flow v1 (app password)

Routes in `core/Controller/ClientFlowLoginController.php`:

- `GET /login/flow` and `GET /login/flow/grant` render login flow pages.
- `POST /login/flow` exchanges state token for an app password.
- Requires `OCS-APIREQUEST: true` header or valid OAuth client identifier for access.
- State token stored in session (`client.flow.state.token`) and compared via `hash_equals`.

### Login flow v2 (token-based)

Routes in `core/Controller/ClientFlowLoginV2Controller.php`:

- `POST /login/v2/poll` returns JSON credentials when flow completes.
- `GET /login/v2/flow/{token}` and `GET /login/v2/flow` manage the flow state.
- `GET /login/v2/grant` and `POST /login/v2/apptoken` finalize the flow.
- Uses session keys `client.flow.v2.login.token` and `client.flow.v2.state.token`.

### OAuth2 token endpoint

Routes in `apps/oauth2/appinfo/routes.php`:

- `POST /apps/oauth2/api/v1/token` returns bearer tokens used by OCS and DAV.
- `GET /apps/oauth2/authorize` initiates OAuth2 auth picker flow.

Bearer tokens must be accepted by OCS (CSRF bypass) and DAV (BearerAuth).

---

Prev: [`05-ocs-api-compatibility.md`](05-ocs-api-compatibility.md) · Up: [`README.md`](README.md) · Next: [`07-webdav-caldav-carddav.md`](07-webdav-caldav-carddav.md)
