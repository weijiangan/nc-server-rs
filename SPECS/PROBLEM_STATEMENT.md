# Problem Statement: Nextcloud Core in Rust

## The Core Problem

PHP's shared-nothing, process-per-request execution model makes PHP-FPM worker exhaustion — not storage I/O or network — the binding resource constraint for a Nextcloud installation under modest load. On resource-constrained hardware this manifests as thread starvation: every worker is held for the full duration of its request, including all DB round trips, because PHP has no yield mechanism. Incoming requests queue. Clients retry. The queue grows.

The starvation arithmetic is straightforward:

```
workers_needed = avg_request_duration_ms × requests_per_second / 1000
```

Five desktop sync clients each polling at 10 req/s with an 80 ms average request duration requires 40 concurrent workers. A 2-core server running a reasonable FPM configuration has around 16–20. Every request beyond that queues, degrading latency for all clients simultaneously.

This is structural. The ceiling is not the CPU, not the disk, not the DB — it is the finite pool of blocking OS processes.

---

## Why This Cannot Be Fixed Within PHP

Three costs compound to produce each request's hold time, none of which are reducible within standard PHP:

| Cost | Mechanism | Cannot Be Fixed Because |
|---|---|---|
| **Bootstrap on every request** | Autoloader, DI container, config parse, session setup — rebuilt from zero | PHP's shared-nothing model discards heap at end of each request |
| **Blocking I/O holds the worker** | DB round trips, filesystem ops block the OS thread | PHP has no async yield; the worker sleeps on `recv()` while the OS could schedule other work |
| **No cross-worker shared state** | Auth token cache, mime map, route table, capability payload rebuilt per worker | Workers are isolated processes; APCu is per-process, not process-group shared |

### Why persistent-PHP alternatives don't fully solve this

**FrankenPHP worker mode** eliminates bootstrap cost — a real improvement — but workers still block on I/O. The concurrency ceiling moves, but its nature doesn't change: you still need one worker held per concurrent in-flight request. Under a sync storm the starvation problem recurs at the same structural point.

**Swoole / ReactPHP** give coroutines and non-blocking I/O, which does address the yield problem. The blocker is that Nextcloud's entire stack was written for synchronous PHP: static singletons, request-scoped globals, blocking DB calls throughout. Making it safe in a persistent-coroutine model requires auditing and replacing every I/O call in every code path — a larger surface than reimplementing the core + files boundary in Rust, and one that cannot be done incrementally without risking cross-request state leakage.

---

## Where the Wins Actually Come From

Not all improvements are equal. In order of impact for the starvation scenario:

**1. The concurrency ceiling is removed entirely.**
Tokio tasks yield at every `await` point — during DB queries, during file I/O, during network reads. Two OS threads can service thousands of concurrent requests because threads are never sleeping. Worker exhaustion as a concept ceases to apply.

**2. Auth token validation drops from a DB query to a hashmap lookup.**
Every single request — regardless of path — hashes the bearer token and queries `oc_authtoken`. With a process-lifetime `Arc<RwLock<HashMap>>` token cache, after the first hit this is a memory lookup. The same desktop client sends the same token on every request; steady-state cache hit rate approaches 100%.

**3. Hot state is shared across all concurrent requests, not duplicated per worker.**
A single mime-type map, capability payload, route table, and config snapshot is held in memory and accessed under read locks. APCu gives per-process caching with serialisation overhead on every access; the Rust model gives zero-copy reads under shared locks across all 10,000 concurrent tasks.

**4. Bootstrap cost disappears.**
The Nextcloud framework stack is not paid per request. This is real but secondary once the concurrency ceiling is the binding constraint — it improves per-request latency, not the starvation problem directly.

---

## Scope Boundary Rationale

The boundary follows **call frequency and latency sensitivity**:

- **Core + files** (Rust): the WebDAV sync loop is hit dozens of times per sync cycle per client. PROPFIND, GET, PUT, token validation, capability probe — these are the high-frequency paths where starvation occurs and where the async model matters.
- **Everything else** (PHP-FPM): sharing, provisioning, CalDAV/CardDAV, settings, federation — these are infrequent, human-triggered operations. A sharing API call happens once when a user shares a file, not on every sync poll. PHP's overhead is negligible at that frequency, and rewriting these apps buys nothing against the starvation problem.

The PHP-FPM apps run as a compute backend — not a second Nextcloud instance. Rust handles auth once and injects the authenticated identity via FastCGI parameters. PHP apps trust the injected identity and skip re-authentication.

---

## Scope

### In Scope — native Rust

- HTTP server and all entry-point routes (`/status.php`, `/ocs/…`, `/remote.php/…`, `/public.php/…`, `/dav/…`, `/apps/files/api/…`)
- Authentication and session management (Basic, Bearer/token, CSRF, brute-force throttling, 2FA enforcement gate)
- OCS API envelope and the three core OCS endpoint groups (capabilities, config, identity)
- Full WebDAV/DAV stack for user file trees and public shares, backed by `dav-server-rs`
- All upload flows: simple PUT, chunked v1, chunked v2, bulk
- Quota enforcement, filename validation, checksum upload/download/recalculation
- Database ownership: schema migrations for all core + files tables
- In-process caching layer (route map, capabilities, token hot cache, mime map, config values)

### Out of Scope — delegated to PHP-FPM

- All Nextcloud apps other than `files`: sharing, CalDAV/CardDAV, provisioning, settings, federation, comments, system tags, OAuth2, LDAP, etc.
- Web UI HTML rendering
- Two-factor authentication challenge pages (Rust enforces the 2FA status gate; PHP-FPM renders the challenge)

---

## Requirements

### Hard requirements (imposed externally)

| Requirement | Rationale |
|---|---|
| Full wire-protocol compatibility | Existing clients must work with zero reconfiguration |
| DB schema additive-only migrations | Existing PHP Nextcloud installs must continue to function alongside the Rust server |
| Support PostgreSQL, MySQL/MariaDB, SQLite | Same databases the PHP stack supports |

### Design decisions (chosen, not imposed)

| Decision | Rationale |
|---|---|
| Auth performed once in Rust; identity injected to PHP-FPM | Avoids duplicating auth logic; PHP apps trust the FastCGI-injected `userId` |
| `dav-server-rs` for RFC 4918 mechanics | Avoids reimplementing WebDAV from scratch; implement Nextcloud's storage model via its trait interfaces |
| `config/config.php` read for existing installs | Zero migration friction for existing Nextcloud deployments |

---

## Risks

**DAV client compatibility.** SabreDAV has absorbed a decade of quirks from Finder, Windows WebDAV, OneNote, iOS, and various WebDAV mounts. `dav-server-rs` is younger. The failure modes are not the obvious RFC methods — they are the client-specific edge cases in `If:` header handling, exact `207` body format, lock token semantics, and principal discovery. The existing litmus and Behat suites catch protocol compliance; they do not exhaustively cover every client quirk.

**PHP-FPM auth handoff surface.** Injecting a trusted `userId` via FastCGI and having PHP apps accept it unconditionally is a security-relevant trust boundary. The shim must not be reachable except via the Rust server; the FastCGI socket must not be exposed directly.

**Nextcloud DB schema evolution.** The PHP codebase continues to evolve. Additive-only migrations are safe, but if upstream PHP Nextcloud adds a column that changes query semantics for an existing Rust handler, both sides must be kept in sync.

---

## Success Criteria

1. All existing Behat/Gherkin integration suites in `build/integration/` pass against the Rust server with no scenario modifications.
2. All Cypress E2E suites in `cypress/e2e/files/` and `cypress/e2e/core/` pass.
3. A litmus WebDAV compliance test against `/remote.php/webdav` reports zero failures.
4. A desktop sync client completes a full initial sync, incremental sync, upload, download, move, and delete cycle with no errors.
5. Under a load that saturates PHP-FPM (worker exhaustion), the Rust server continues to serve requests without queueing.
6. An existing PHP Nextcloud installation migrates to the Rust server by pointing the web server at the Rust binary — no DB changes, no client reconfiguration required.
