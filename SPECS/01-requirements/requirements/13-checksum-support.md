## 13. Checksum Support

### 13.1 On PUT upload

Client may send `OC-Checksum: {ALGORITHM}:{hash}` (MD5, SHA1, SHA256, Adler32 supported).

Server must:
1. Compute the same hash of the received data.
2. If mismatch: return `400 Bad Request`.
3. If match: store the checksum in `oc_filecache.checksum`.

### 13.2 On GET download

Server must include `OC-Checksum: {stored_checksum}` in the response headers if a checksum is stored.

### 13.3 Checksum recalculation (PATCH)

```
PATCH /dav/files/{userId}/{path}
X-Recalculate-Hash: {algorithm}
```

Server recomputes the hash, stores it, and responds:
```
HTTP 204 No Content
OC-Checksum: {ALGORITHM}:{new_hash}
```

---

---

Prev: [`12-filename-validation.md`](12-filename-validation.md) · Up: [`README.md`](README.md) · Next: [`14-special-dav-plugins.md`](14-special-dav-plugins.md)
