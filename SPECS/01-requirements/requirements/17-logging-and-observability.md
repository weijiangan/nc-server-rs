## 17. Logging and Observability

- Structured log entries for every request: method, path, status code, authenticated user, response time, request ID.
- Log at `DEBUG` level: cache hits/misses, token validation outcome.
- Log at `INFO` level: successful logins, file mutations (create/delete/move).
- Log at `WARN` level: brute-force attempts detected, quota near-limit writes.
- Log at `ERROR` level: storage errors, DB errors, unexpected panics.
- Request ID: generate a UUID per request, propagate as `X-Request-Id` header and in all log lines for that request.

---

---

Prev: [`16-caching-strategy.md`](16-caching-strategy.md) · Up: [`README.md`](README.md) · Next: [`18-configuration-file.md`](18-configuration-file.md)
