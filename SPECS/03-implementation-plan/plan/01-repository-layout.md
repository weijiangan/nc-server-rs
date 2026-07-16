## Repository layout

The Rust implementation lives in `core-rs/` at the root of the `nc-server` repo, alongside the existing PHP codebase. The PHP files are unchanged. The two coexist in the same repository and share nothing at the source level — the integration point is the running system (FastCGI socket, shared DB, shared `datadirectory`).

```
nc-server/
├── apps/                   # PHP apps (unchanged)
├── lib/                    # PHP core library (unchanged)
├── config/                 # config.php (read by both PHP and Rust)
├── core-rs/                # Rust implementation
│   ├── Cargo.toml          # workspace root
│   ├── crates/
│   │   ├── nc-server/      # binary crate — HTTP server, main entry point
│   │   ├── nc-db/          # DB pool, migrations, startup caches
│   │   ├── nc-auth/        # auth stack, token cache, brute-force
│   │   ├── nc-dav/         # DavFileSystem, DavProp, DavLockSystem impls
│   │   ├── nc-ocs/         # OCS envelope, capabilities, core endpoints
│   │   ├── nc-files/       # files app REST + OCS endpoints, upload flows
│   │   └── nc-fastcgi/     # FastCGI client, route registry, PHP-FPM dispatch
│   ├── migrations/         # sqlx versioned SQL migration files
│   └── tests/              # integration test harness (wraps Behat/Cypress)
├── build/integration/      # existing Behat suites (reused as-is)
├── cypress/                # existing Cypress suites (reused as-is)
└── SPECS/                  # this document and related specs
```

### Coexistence rules
- `core-rs/` is a standalone Cargo workspace. Running `cargo build` inside it has no effect on the PHP codebase.
- The Rust binary reads `../config/config.php` relative to its working directory when run from the repo root, or an explicit `--config` path flag.
- `.gitignore` at the repo root gains `core-rs/target/` to exclude Rust build artefacts.
- No PHP files are modified. The PHP codebase continues to work independently (for reference, testing, and PHP-FPM dispatch).

---

---

Up: [`README.md`](README.md) · Next: [`02-key-dependencies.md`](02-key-dependencies.md)
