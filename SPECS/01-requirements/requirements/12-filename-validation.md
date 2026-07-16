## 12. Filename Validation

Before any write (PUT, MKCOL, MOVE, COPY target), validate against:
- `forbidden_filenames` list (exact matches, case-insensitive)
- `forbidden_filename_basenames` (name without extension, case-insensitive)
- `forbidden_filename_characters` (reject names containing any of these characters)
- `forbidden_filename_extensions` (file extension matches, case-insensitive)

Lists are configurable via `oc_appconfig` for `core` app, with defaults matching `.htaccess`, `web.config`, etc.

On violation: `422 Unprocessable Entity` (SabreDAV `InvalidPath` exception → HTTP 400/422).

---

---

Prev: [`11-quota-enforcement.md`](11-quota-enforcement.md) · Up: [`README.md`](README.md) · Next: [`13-checksum-support.md`](13-checksum-support.md)
