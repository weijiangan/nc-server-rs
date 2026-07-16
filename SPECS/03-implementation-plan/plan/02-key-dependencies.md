## Key dependencies

| Crate | Role |
| --- | --- |
| `axum` | HTTP server framework — chosen over `actix-web` because it is built directly on `tokio`/`hyper` with no separate runtime, giving clean integration with `sqlx` and `dav-server` |
| [`dav-server`](https://github.com/messense/dav-server-rs) | WebDAV handler (RFC4918 litmus-passing); plug in via `DavFileSystem` + `DavProp` + `DavLockSystem` traits |
| `tokio` | Async runtime — the core architectural win: tasks yield at every `.await`, so two OS threads service thousands of concurrent sync clients without worker exhaustion |
| `hyper` | Low-level HTTP types (compatible with `dav-server`) |
| `sqlx` | Async DB access (Nextcloud DB schema); persistent connection pool shared across all Tokio tasks |
| `serde` / `quick-xml` | OCS JSON/XML serialization |

`dav-server-rs` eliminates reimplementing WebDAV from scratch. We implement Nextcloud's storage and property model by satisfying its trait interfaces, and the library handles all RFC4918 methods, preconditions, partial transfers, locking, and the litmus test surface.

---

---

Prev: [`01-repository-layout.md`](01-repository-layout.md) · Up: [`README.md`](README.md) · Next: [`03-db-schema-ownership-and-migrations.md`](03-db-schema-ownership-and-migrations.md)
