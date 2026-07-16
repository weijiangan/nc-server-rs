## Files app REST API

The `files` app exposes HTTP endpoints in addition to the DAV tree.  Mobile and web
clients use these for thumbnails, recent-file lists, and configuration.

### Thumbnail endpoint

`GET /apps/files/api/v1/thumbnail/{width}/{height}/{path}`

- `path` matches `.+` and is a URL-encoded file path relative to the user's root.
- Returns the image scaled to `{width}x{height}` pixels.
- Served by `Api#getThumbnail` in `apps/files/lib/Controller/ApiController.php`.

### Recent files

`GET /apps/files/api/v1/recent/` — returns recently modified files as JSON.

### Storage statistics

`GET /apps/files/api/v1/stats` — returns used/free space for the user's storage.

### View and display config

- `GET/PUT /apps/files/api/v1/views/{view}/{key}` — per-view display preferences.
- `GET/PUT /apps/files/api/v1/configs` — global files app settings.
- `POST /apps/files/api/v1/showhidden` — toggle hidden-file display.
- `POST /apps/files/api/v1/showgridview` / `GET /apps/files/api/v1/showgridview`.

### OCS endpoints in the files app

Prefixed at `/ocs/v2.php/apps/files/`:

- `GET /api/v1/directEditing` — list available direct-editing handlers.
- `POST /api/v1/directEditing/open` — open a file in a direct-editing session.
- `POST /api/v1/directEditing/create` — create a new file via direct editing.
- `GET /api/v1/templates` — list file templates.
- `POST /api/v1/templates/create` — create a file from a template.
- `POST /api/v1/transferownership` — initiate ownership transfer.
- `POST /api/v1/openlocaleditor` — create a token for opening a file in a local editor.
- `POST /api/v1/openlocaleditor/{token}` — validate a local editor token.

Full route list in `apps/files/appinfo/routes.php`.

---

Prev: [`10-well-known-endpoints.md`](10-well-known-endpoints.md) · Up: [`README.md`](README.md) · Next: [`12-public-sharing-endpoints.md`](12-public-sharing-endpoints.md)
