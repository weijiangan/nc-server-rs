## 8) Load validation and starvation regression test

Caching is not deferred to this step — each cache is implemented in the step that owns its data (mime map in §0, capability cache in §2, token hot cache in §3). This step validates the system under the load conditions that motivated the rewrite.

### Coding steps
1. Build a synthetic load harness: N concurrent desktop sync clients, each running a tight loop of `GET /ocs/v2.php/cloud/capabilities` → `PROPFIND /dav/files/{user}` → random PUT/GET/DELETE. N should exceed the PHP-FPM worker count that would have caused starvation on the same hardware.
2. Add cache correctness regression checks:
	- Capability payload reflects a config change within one request of the write (no stale serve).
	- Token cache eviction on explicit revocation: revoke a token, confirm the next request with that token returns 401 without a DB round trip being required.
	- Mime-type cache remains consistent after a new mimetype is inserted.
3. Add concurrency regression checks for upload mutation paths:
	- Concurrent PUT to the same path: one wins, one gets a consistent response.
	- Chunked v2 assembly race: two MOVE requests for the same upload ID.

### Verification steps
1. Under the synthetic load at N > PHP-FPM worker ceiling: Rust server p99 latency remains flat; there is no request queue growth. This is the primary success criterion from the problem statement.
2. Re-run the full Behat + Cypress suite under load to confirm no cache-induced correctness regressions.
3. Benchmark representative client workflows end-to-end:
	- login + capabilities (should be near-zero DB cost in steady state)
	- WebDAV sync/upload/download
	- files UI actions (rename/move/delete/search).

---

---

Prev: [`10-php-app-support-via-fastcgi-dispatch.md`](10-php-app-support-via-fastcgi-dispatch.md) · Up: [`README.md`](README.md) · Next: [`12-existing-tests-you-can-directly-reuse.md`](12-existing-tests-you-can-directly-reuse.md)
