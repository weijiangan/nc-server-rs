## 20. Non-Functional Requirements

| Requirement | Target |
|---|---|
| Cold start time | < 500 ms on standard server hardware |
| Request latency (PROPFIND depth-1, 100 files) | < 50 ms p99 with warm DB connection pool |
| Concurrent connections | ≥ 10,000 (Tokio async; no thread-per-request) |
| DB connection pool | Min 5, max 50 per process |
| Memory foot print (idle) | < 64 MB resident |
| Binary size | < 50 MB stripped ELF |
| Compile-time DB driver selection | Feature flags: `postgres`, `mysql`, `sqlite` |
| Zero unsafe code outside FFI boundary | `#![forbid(unsafe_code)]` except designated FFI modules |
| Graceful shutdown | Drain in-flight requests within 30 s on SIGTERM |
| Upgrade safety | Rolling deploy: new binary must accept sessions issued by old binary |

---

Prev: [`19-compatibility-test-matrix.md`](19-compatibility-test-matrix.md) · Up: [`README.md`](README.md)
