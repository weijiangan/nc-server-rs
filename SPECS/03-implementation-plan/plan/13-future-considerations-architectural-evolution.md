## Future Considerations: Architectural Evolution

While the primary goal of this implementation is 1-to-1 parity with the PHP behavior via the `oc_filecache` database, future iterations should consider the "OCIS model" of metadata management to address long-term RDBMS bottlenecks.

### Recommendation: Metadata Write-Through Strategy
Do **not** move the source of truth away from the database yet (it would break compatibility with all 300+ PHP apps). Instead, implement a **Shadow Metadata Cache**:

1.  **Primary Authority:** Maintain `oc_filecache` as the source of truth for all write operations to ensure PHP compatibility.
2.  **Accelerator:** Store specialized metadata (ETags, checksums, permissions) in a high-performance sidecar format (e.g., Extended Attributes or a local Key-Value store like sled/RocksDB) within the Rust layer.
3.  **Read Path Optimization:** In `DavFileSystem`, prioritize the Shadow Cache for `PROPFIND` Depth-1 operations. Only fallback to SQL if the cache is stale or missing.

This provides the horizontal scalability benefits of ownCloud OCIS without the "Dual Source of Truth" corruption risks or the need for a total data migration.

---

Prev: [`12-existing-tests-you-can-directly-reuse.md`](12-existing-tests-you-can-directly-reuse.md) · Up: [`README.md`](README.md) · Next: [`14-native-preview-thumbnail-fast-path.md`](14-native-preview-thumbnail-fast-path.md)
