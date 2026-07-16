## 12. Filename Validation

Before any write (PUT, MKCOL, MOVE, COPY target), validate against:
- `forbidden_filenames` list (exact matches, case-insensitive)
- `forbidden_filename_basenames` (name without extension, case-insensitive)
- `forbidden_filename_characters` (reject names containing any of these characters)
- `forbidden_filename_extensions` (file extension matches, case-insensitive)

Lists are configurable via `oc_appconfig` for `core` app, with defaults matching `.htaccess`, `web.config`, etc.

On violation: `400 Bad Request`. PHP throws `OCP\Files\InvalidPathException`, which the DAV connector (`apps/dav/lib/Connector/Sabre/Directory.php`) re-throws as `Connector\Sabre\Exception\InvalidPath`, whose `getHTTPCode()` returns **`400`** (`Exception/InvalidPath.php`). The DAV error body carries an `<o:reason>` element.

---

---

Prev: [`11-quota-enforcement.md`](11-quota-enforcement.md) · Up: [`README.md`](README.md) · Next: [`13-checksum-support.md`](13-checksum-support.md)
