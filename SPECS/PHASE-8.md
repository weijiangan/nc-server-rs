# Phase 8 — Load Validation and Starvation Regression

Goal: confirm the system sustains load that would exhaust PHP-FPM workers without degrading, and that all caches are correct under concurrent mutation.

---

### 8.1 Synthetic load harness
- [ ] Script: N concurrent clients (e.g. using `oha`, `k6`, or a custom Rust harness), each running: `GET /ocs/v2.php/cloud/capabilities` → `PROPFIND /dav/files/{user}` (depth-1, 100 files) → random `PUT`/`GET`/`DELETE`
- [ ] N configurable; default: 2× the PHP-FPM worker count that would have saturated on the same hardware
- [ ] Harness reports p50, p99, p999 latency and error rate per endpoint

**Verify:** at N = 2× PHP-FPM ceiling, p99 PROPFIND latency < 50 ms, error rate = 0%, no request queue growth observed.

### 8.2 Cache correctness — capabilities
- [ ] Write a new value to `oc_appconfig` that affects capabilities
- [ ] Immediately send `GET /ocs/v2.php/cloud/capabilities`
- [ ] Assert the response reflects the updated value (no stale serve)

**Verify:** ETag of response changes after config write; new capability field present in next request.

### 8.3 Cache correctness — token revocation
- [ ] Authenticate with a token; confirm 200
- [ ] Revoke the token (delete from `oc_authtoken`)
- [ ] Send next request with same token
- [ ] Assert `401` on the very next request — no stale cache hit

**Verify:** `401` received on request immediately following explicit revocation (not after TTL expiry).

### 8.4 Cache correctness — mime type
- [ ] Insert a new mime type into `oc_mimetypes` while server is running
- [ ] Upload a file with that MIME type
- [ ] PROPFIND should return the correct new MIME type (not "unknown")

**Verify:** new MIME type appears correctly in PROPFIND response without server restart.

### 8.5 Concurrency — concurrent PUT to same path
- [ ] Issue two concurrent `PUT` requests to the same path with different content
- [ ] One must win atomically; the file on disk must match exactly one of the two uploads
- [ ] No partial writes, no torn file, no DB state inconsistency

**Verify:** repeat 20 times; in every case the file's checksum matches one of the two PUT payloads exactly.

### 8.6 Concurrency — chunked v2 assembly race
- [ ] Start two concurrent MOVE assembly requests for the same `upload_id`
- [ ] One must succeed (`201`/`204`); the other must return `409 Conflict` or `423 Locked`
- [ ] No double-write to the target file

**Verify:** repeat 10 times; assert exactly one success and one non-2xx per pair, and the target file is consistent.

### 8.7 Full suite under load
- [ ] Run the complete Behat suite while the load harness (8.1) is active at 50% of starvation-ceiling load
- [ ] All scenarios must pass — no flakes introduced by concurrent cache state

**Verify:** Behat exits 0 with the load harness running; no scenario marked failed or skipped due to timing.

### 8.8 Benchmark baseline comparison
- [ ] Measure p99 latency for the key workflows under the Rust server
- [ ] Compare against a PHP-FPM baseline on the same hardware (or documented reference)
- [ ] Document: capabilities probe, PROPFIND (depth-1, 100 files), simple PUT, token validation (warm cache vs cold)

**Verify:** results written to `SPECS/BENCHMARKS.md`; warm-cache token validation shows near-zero DB query count vs. PHP baseline.
