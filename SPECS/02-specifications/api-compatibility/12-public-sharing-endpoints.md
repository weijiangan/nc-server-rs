## Public sharing endpoints

The `files_sharing` app registers several public-facing routes (no authentication needed):

| Route | Purpose |
| --- | --- |
| `GET /s/{token}` | Render public share page. |
| `GET /s/{token}/authenticate/{redirect}` | Show password prompt. |
| `POST /s/{token}/authenticate/{redirect}` | Submit share password. |
| `GET /s/{token}/download/{filename}` | Direct download of shared file. |
| `GET /s/{token}/preview` | Preview image for a public share. |
| `GET /publicpreview/{token}` | Alternative preview URL. |
| `POST /shareinfo` | Returns share metadata for a given token. |

Public DAV for share tokens is at `/public.php/dav` (served by `publicremote.php`).
Files-drop shares (upload-only) are enforced by `FilesDropPlugin` on that endpoint.



To be fully API compatible, you must implement routes from the shipped core apps
listed below. Each app’s `appinfo/routes.php` is the authoritative route list.

| App | Key API surfaces | Route file |
| --- | --- | --- |
| `provisioning_api` | Users, groups, apps, app config, user prefs | `apps/provisioning_api/appinfo/routes.php` |
| `files_sharing` | OCS share API, sharees, remote shares, public shares | `apps/files_sharing/appinfo/routes.php` |
| `files` | Files app APIs, thumbnails, recent, templates, direct editing | `apps/files/appinfo/routes.php` |
| `dav` | DAV public routes, direct endpoints, upcoming events | `apps/dav/appinfo/routes.php` |
| `oauth2` | OAuth2 tokens and authorize redirect | `apps/oauth2/appinfo/routes.php` |
| `federation` | Shared-secret endpoints | `apps/federation/appinfo/routes.php` |
| `cloud_federation_api` | OCM share requests | `apps/cloud_federation_api/appinfo/routes.php` |
| `federatedfilesharing` | Federated share OCS endpoints | `apps/federatedfilesharing/appinfo/routes.php` |
| `comments` | Notifications view | `apps/comments/appinfo/routes.php` |
| `systemtags` | Tag usage | `apps/systemtags/appinfo/routes.php` |
| `files_versions` | Previews, download/rollback scripts | `apps/files_versions/appinfo/routes.php` |
| `files_trashbin` | Previews | `apps/files_trashbin/appinfo/routes.php` |

For complete API coverage, parse every `apps/*/appinfo/routes.php` and add support
for the declared routes and controller behaviors. Many routes depend on response
schemas defined in each app’s `ResponseDefinitions` class (e.g. `files_sharing`).

---

Prev: [`11-files-app-rest-api.md`](11-files-app-rest-api.md) · Up: [`README.md`](README.md) · Next: [`13-app-compatibility-considerations.md`](13-app-compatibility-considerations.md)
