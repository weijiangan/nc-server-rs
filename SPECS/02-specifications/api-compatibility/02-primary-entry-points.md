## Primary entry points

| Endpoint | Purpose | Notes |
| --- | --- | --- |
| `/index.php` | Front controller | Calls `OC::handleRequest()` (lib/base.php). |
| `/remote.php/{service}/...` | DAV and remote services | Service mapping in `remote.php`. |
| `/public.php/{service}/...` | Public DAV | Public shares and files drop. |
| `/ocs/v1.php` | OCS v1 API | XML default, 200 for most statuses. |
| `/ocs/v2.php` | OCS v2 API | XML default, status codes preserved. |
| `/ocs-provider/index.php` | OCS providers list | JSON list of available providers. |
| `/status.php` | Instance status | JSON, CORS `Access-Control-Allow-Origin: *`. |

---

Prev: [`01-scope.md`](01-scope.md) · Up: [`README.md`](README.md) · Next: [`03-request-lifecycle-and-global-behavior.md`](03-request-lifecycle-and-global-behavior.md)
