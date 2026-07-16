## 11. Quota Enforcement

Quota checks happen before write operations (PUT, MKCOL, COPY, MOVE, chunk assembly):

1. Resolve the free space for the target path by querying the storage's `free_space()`.
2. Compare against the larger of `Content-Length`, `X-Expected-Entity-Length`, and `OC-Total-Length` headers.
3. If free space < required space → `507 Insufficient Storage` response.
4. For MKCOL: check 4096 bytes as a proxy for directory creation cost.
5. Storage abstraction sentinel values — when `free_space()` returns **any negative value**, the quota check is skipped and the write is allowed. Sentinels are:
   - `SPACE_NOT_COMPUTED = -1`: file size not yet scanned
   - `SPACE_UNKNOWN = -2`: free space is not determinable (e.g. external storage)
   - `SPACE_UNLIMITED = -3`: no quota / unlimited storage
   - PHP `QuotaPlugin.checkQuota()` treats all negative `free_space()` as "allow" — this mirrors that behavior.
6. DAV property mapping: `{DAV:}quota-available-bytes` reports `-3` when quota is unlimited (confirmed by integration tests). Internal `SPACE_UNKNOWN (-2)` maps to `-3` in the DAV response.

---

---

Prev: [`10-php-fpm-integration.md`](10-php-fpm-integration.md) · Up: [`README.md`](README.md) · Next: [`12-filename-validation.md`](12-filename-validation.md)
