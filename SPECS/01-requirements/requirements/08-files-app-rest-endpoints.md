## 8. Files App REST Endpoints

All mounted under `/apps/files/` (via OC routing, `index.php/apps/files/…` or clean URL).

### 8.1 REST (non-OCS) endpoints

| Method | URL | Description |
|---|---|---|
| `GET` | `/apps/files/` | Files app SPA index page (PHP-FPM) |
| `GET` | `/apps/files/f/{fileid}` | Show file by ID (PHP-FPM redirect) |
| `GET` | `/apps/files/api/v1/thumbnail/{x}/{y}/{file+}` | Generate/fetch preview thumbnail |
| `POST` | `/apps/files/api/v1/files/{path+}` | Update file tags |
| `GET` | `/apps/files/api/v1/recent/` | Recent files list |
| `GET` | `/apps/files/api/v1/stats` | Storage stats (used/free/total) |
| `PUT` | `/apps/files/api/v1/views/{view}/{key}` | Set view config value |
| `PUT` | `/apps/files/api/v1/views` | Set multiple view config values |
| `GET` | `/apps/files/api/v1/views` | Get all view configs |
| `PUT` | `/apps/files/api/v1/config/{key}` | Set user config value |
| `GET` | `/apps/files/api/v1/configs` | Get all user config values |
| `POST` | `/apps/files/api/v1/showhidden` | Toggle show-hidden-files |
| `POST` | `/apps/files/api/v1/cropimagepreviews` | Toggle crop image previews |
| `POST` | `/apps/files/api/v1/showgridview` | Set grid view |
| `GET` | `/apps/files/api/v1/showgridview` | Get grid view setting |
| `GET` | `/apps/files/directEditing/{token}` | Direct editing token view (PHP-FPM) |
| `GET` | `/apps/files/preview-service-worker.js` | Service worker JS |
| `GET` | `/apps/files/{view}` | View-specific SPA entry (PHP-FPM) |
| `GET` | `/apps/files/{view}/{fileid}` | View+fileid SPA entry (PHP-FPM) |

### 8.2 OCS endpoints (mounted under `/ocs/…/apps/files/api/v1/`)

| Method | URL suffix | Description |
|---|---|---|
| `GET` | `/directEditing` | Direct editing info (available editors) |
| `GET` | `/directEditing/templates/{editorId}/{creatorId}` | Templates for editor |
| `POST` | `/directEditing/open` | Open file in direct editor |
| `POST` | `/directEditing/create` | Create file via direct editor |
| `GET` | `/templates` | List file templates |
| `GET` | `/templates/fields/{fileId}` | List template fields for file |
| `POST` | `/templates/create` | Create file from template |
| `POST` | `/templates/path` | Set templates folder path |
| `POST` | `/transferownership` | Initiate ownership transfer |
| `POST` | `/transferownership/{id}` | Accept transfer |
| `DELETE` | `/transferownership/{id}` | Reject transfer |
| `POST` | `/openlocaleditor` | Create open-in-local-editor token |
| `POST` | `/openlocaleditor/{token}` | Validate open-in-local-editor token |

---

---

Prev: [`07-upload-flows.md`](07-upload-flows.md) · Up: [`README.md`](README.md) · Next: [`09-database-schema.md`](09-database-schema.md)
