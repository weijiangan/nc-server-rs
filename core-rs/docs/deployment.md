# nc-server Deployment Guide

## Overview

`nc-server` is a drop-in replacement for the PHP-FPM listener on a standard
Nextcloud installation.  It takes over **all inbound HTTP traffic** (WebDAV,
OCS, files API) and forwards routes it does not handle natively to PHP-FPM over
a Unix socket.  The PHP codebase is **not modified** — it is only ever called by
the Rust proxy, never directly by clients.

```
Client → nc-server (port 80/443)
           ├── Native:  WebDAV · OCS · status.php · auth
           └── FastCGI: everything else → PHP-FPM (Unix socket, 0600)
```

---

## Step 1 — Install Nextcloud with PHP first

`nc-server` has no installer of its own.  Use the standard PHP-based installer
to create the database schema, write `config/config.php`, and set up the admin
account.

```bash
# Web installer: open https://yourhost/ with PHP-FPM/nginx still in place.
# Or CLI:
php occ maintenance:install \
  --database pgsql \
  --database-host 127.0.0.1 \
  --database-name nextcloud \
  --database-user nextcloud \
  --database-pass secret \
  --admin-user admin \
  --admin-pass changeme \
  --data-dir /var/lib/nextcloud/data
```

After install, confirm `/status.php` returns `"installed":true` against the PHP
stack before switching traffic to the Rust binary.

**Why:** `occ maintenance:install` writes `installed = true`, `instanceid`,
`secret`, and `passwordsalt` to `config.php` via `SystemConfig::setValue()`.
These values are never in `oc_appconfig` and `nc-server` reads them directly
from `config.php` at startup.

---

## Step 2 — Build nc-server

```bash
cd /path/to/nc-server/core-rs
cargo build --release --features postgres   # or --features sqlite
```

The binary is at `target/release/nc-server`.

**Packaged / systemd install:** use the PKGBUILD in
[`packaging/`](../packaging/README.md) (`cd packaging && makepkg -si`).  It
installs the binary, the PHP bootstrap shim (required at runtime — PHP-FPM
executes it on every proxied request), a systemd unit, and an env file, and
keeps binary and shim in lockstep.  Running from a source checkout needs no
installation: the shim resolves to `core-rs/php-shim/index.php` in-tree.

Shim resolution order at startup: `NC_PHP_SHIM` env var → compiled-in
packaged default (`/usr/share/nc-server/php-shim/index.php`; retarget at
build time with `NCSHIMDIR=<dir>`) → in-tree dev layout.  The chosen path is
logged at startup, and a missing shim logs a warning (proxied requests then
return 502).

---

## Step 3 — Add nc-server config keys to config.php

Append to `config/config.php` (the same file Nextcloud uses):

```php
// Unix socket path for PHP-FPM — must match listen= in the pool config below.
'fastcgi_socket' => '/run/nc-fpm.sock',
// Optional: increase for slow PHP routes (default 30 000 ms).
'fastcgi_timeout_ms' => 30000,
// Optional: PHP CLI interpreter for parsing config.php and the imagick
// startup probe (default 'php'). The NC_PHP_BINARY env var overrides it.
// 'php_binary' => 'php-legacy',
```

No other keys are needed — `nc-server` reads all standard Nextcloud config keys
(`dbtype`, `dbhost`, `secret`, `instanceid`, `datadirectory`, etc.) from the
same `config/config.php`.

---

## Step 4 — Start nc-server

```bash
# Run from the Nextcloud repo root (so config/config.php is found automatically).
cd /path/to/nc-server
./core-rs/target/release/nc-server --listen 0.0.0.0:7000

# Or specify the root explicitly:
./core-rs/target/release/nc-server \
  --root /path/to/nc-server \
  --listen 0.0.0.0:7000
```

On startup the binary:
1. Parses `config/config.php` (reads `installed`, `maintenance`, `instanceid`,
   `secret`, `dbtype`, etc.)
2. Opens the database pool and runs `sqlx migrate` (no-op on an already-migrated
   DB)
3. Loads in-memory caches (mime types, app config)
4. Opens the HTTP listener

Check `GET /status.php` — it should return `"installed":true`.

> **Maintenance mode:** `nc-server` reads `maintenance` from `config.php` at
> startup only (PHP writes it via `SystemConfig`, not `oc_appconfig`).  Toggling
> `occ maintenance:mode --on` while `nc-server` is running requires a restart to
> take effect.  See [improvements.md](../../SPECS/02-specifications/improvements.md) §I.1 for a
> planned hot-reload fix.

---

## Step 5 — Point clients at nc-server

If you were previously running nginx → PHP-FPM, update nginx to proxy to
`nc-server` instead:

```nginx
location / {
    proxy_pass         http://127.0.0.1:7000;
    proxy_set_header   Host              $host;
    proxy_set_header   X-Real-IP         $remote_addr;
    proxy_set_header   X-Forwarded-For   $proxy_add_x_forwarded_for;
    proxy_set_header   X-Forwarded-Proto $scheme;
    proxy_read_timeout 120s;
}
```

`nc-server` handles TLS termination upstream (nginx/caddy) the same way a
standard PHP-FPM setup does.

### Static-file deny rules (canonical layer)

`nc-server` serves static assets itself from a strict whitelist (`/core/`,
`/dist/`, `/themes/`, `/apps/`, plus `robots.txt` and `index.html`) and 404s
everything else before touching the filesystem (Phase 18.1 / F1).  The
canonical webserver layer should enforce the same deny set — defense in depth
for anything that ever bypasses or precedes `nc-server` (another vhost, a
misrouted location, a future static mode):

```nginx
# Deny what the whitelist denies: data dir, dotfiles, PHP's bundled 3rdparty.
location ~ ^/(data|3rdparty)/          { deny all; return 404; }
location ~ /\.                         { deny all; return 404; }
location ~ \.php$                      { deny all; return 404; }  # nc-server proxies PHP itself
```

These mirror the paths the F1 property pins in tests and the parity gate
(`/data/.ocdata`, `/.htaccess`, `/3rdparty/*` → 404 on both the Rust SUT and
the pure-PHP oracle).  When `nc-server` is the only server, the whitelist is
sufficient; keep the nginx rules only if other vhosts share the webroot.

---

## Target-profile tuning (2-core, HDD, localhost Postgres)

Tuning for the deployment this build is designed around: 2 physical cores,
an HDD data disk, and Postgres on the same box.  Every value below is
measured against that profile; do not transplant SSD-era settings into it.

### PostgreSQL (`postgresql.conf`)

```ini
# HDD-correct planner cost — random page access is ~4x the sequential cost
# on a platter.  This is the single most important line for this profile;
# keep SSD configurations (random_page_cost = 1.1) out of shared configs.
random_page_cost = 4.0

# Size to real RAM, not the box's name.  This is what the planner believes
# is available for index scans + the query working set.
effective_cache_size = 4GB        # 50-70% of physical RAM

# Modest: on low-RAM the OS page cache does the real work (see the page-cache
# discipline note below) — Postgres only needs to buffer what it actively
# writes and re-reads.  128MB is a sane floor, 512MB the ceiling here.
shared_buffers = 256MB

# Group commit: batches concurrent commits into one fsync.  Safe on HDD
# (unlike synchronous_commit = off, which trades durability for speed).
commit_delay = 100000             # 0.1 ms, nanoseconds
commit_siblings = 5

# Autovacuum must not compete with interactive requests on 2 cores: throttle
# its cost and keep it off the busiest hours is not enough — cap the workers.
autovacuum_max_workers = 3
autovacuum_vacuum_cost_delay = 50ms
autovacuum_vacuum_cost_limit = 400

# Aligned with the nc-server pool floor (see below): 8 backends on a 2-core
# box, not the 100-connection default — each backend is a process competing
# for the same cores and the same platter.
max_connections = 40
```

### nc-server (`config.php`)

```php
// The connection pool clamps to 4-8 backends on a 2-core box (floor 4,
// ceiling 8).  Nothing to configure — just don't raise PG's
// max_connections to match the old 100-backend expectations.
//
// Preview generation is CPU- and read-heavy; on 2 cores it must not starve
// request handling.  The generator is semaphore-gated; cap it at 1 job
// (the default is hardware concurrency = 2 here).
'preview_concurrency_new' => 1,
```

### Page-cache discipline

This profile runs on RAM where a single large download can evict
Postgres's working set — the very cache that keeps index reads off the
platter.  nc-server drops the pages of any file ≥32 MiB once it has been
fully streamed (`posix_fadvise(DONTNEED)`), and issues `WILLNEED` +
`SEQUENTIAL` when a GET opens a file so the platter seek overlaps the
metadata query.  The rules to respect:

- Give the OS page cache room: do **not** let `shared_buffers` +
  `effective_cache_size` exceed ~70% of RAM, and do not run other
  page-cache-hungry services (build agents, bulk transfers) on the same box.
- Do not drop caches in a cron job; the kernel's own eviction under
  `random_page_cost = 4.0` is the discipline the tunings assume.

### Compression

Probed on the current server: nc-server sends no `Content-Encoding` today
(no middleware compression; the only deflate is inside ZIP folder-download
archives, unchanged).  PHP's own HTML/JS routes gzip only if the reverse
proxy is configured to.

- **LAN-local clients: no compression.**  Gzip buys nothing at 1 Gbit
  loopback/lan speeds on a CPU-starved box — it would cost more than it
  saves.
- **Remote clients:** enable compression on the reverse proxy for
  HTML/JS/CSS only — never on DAV GET streams (user files are already
  large/compressed; on-the-fly compression of a video file wastes the same
  2 cores).  Use **zstd level 1** (or gzip level 1 — not 6): the goal is
  cheap text savings, not maximal ratio.

---

## PHP-FPM configuration

### Unix socket (hard requirement)

The Rust server connects to PHP-FPM over a **Unix socket only**. TCP mode
(`127.0.0.1:9000`, `0.0.0.0:9000`, etc.) is **not supported** and must not be
used. The `fastcgi_socket` key in `config/config.php` must be an absolute file
path, not a host:port string.

Example PHP-FPM pool configuration (`/etc/php/8.x/fpm/pool.d/nextcloud.conf`):

```ini
[nextcloud]
user  = nextcloud
group = nextcloud

listen       = /run/nc-fpm.sock
listen.owner = nextcloud
listen.group = nextcloud
listen.mode  = 0600

pm                   = dynamic
pm.max_children      = 20
pm.start_servers     = 4
pm.min_spare_servers = 2
pm.max_spare_servers = 6
```

Corresponding `config/config.php` entry:

```php
'fastcgi_socket' => '/run/nc-fpm.sock',
'fastcgi_timeout_ms' => 30000,
```

### Socket file permissions (hard requirement)

The socket file **must** be owned by the same OS user that runs `nc-server`,
with mode `0600`.

```
-rw------- 1 nextcloud nextcloud 0 ... /run/nc-fpm.sock
```

Correct `listen.owner`, `listen.group`, and `listen.mode` values in the FPM
pool config (see above) produce this automatically.

**Why this matters:** the PHP bootstrap shim (`core-rs/php-shim/index.php`)
validates the `HTTP_X_NC_PROXIED` FastCGI parameter injected by the Rust proxy
on every proxied request.  This distinguishes legitimate Rust-proxied requests
(both authenticated and unauthenticated) from direct socket connections.  Any
OS process that can connect to the socket can inject `HTTP_X_NC_PROXIED=1`
and bypass the shim's guard, so `0600` ownership is the real security
boundary.

A mode of `0660` (group-readable) is potentially acceptable only if the group
contains exactly one principal (the `nc-server` process user), but `0600` is
strongly preferred.

---

## FastCGI trust boundary

### Never expose the socket to the network or untrusted processes

The FastCGI socket path configured in `fastcgi_socket` **must never be**:

- bound to any network-accessible TCP address
- placed in a directory writable or listable by untrusted local users
- readable by any OS process other than `nc-server` (i.e. do not use `0666`)

Binding PHP-FPM to a TCP address (even `127.0.0.1`) exposes the unauthenticated
FastCGI protocol to any local process and, in container or shared-hosting
environments, potentially to remote processes.

### Defence-in-depth layers

Two independent guards protect the trust boundary. **Both must be in place.**

| Layer | Mechanism | Protects against |
|---|---|---|
| 1 | `0600` Unix socket permissions | Any process connecting directly to the socket |
| 2 | `reject_unauthenticated_shim_request()` in the PHP shim | Socket permission misconfiguration; belt-and-suspenders |

Layer 1 is the primary control. Layer 2 is a last line of defence: if socket
permissions are accidentally widened (e.g. during an OS upgrade that recreates
`/run`), the shim still rejects requests that lack the Rust-injected
`HTTP_X_NC_PROXIED` marker.

### What the Rust proxy guarantees

- Client-supplied `X-NC-User`, `X-NC-Session-Token`, `X-NC-Is-Admin`, and
  `X-NC-Proxied` headers are **stripped** before being forwarded as FastCGI
  `HTTP_*` params, so a malicious HTTP client cannot inject these values.
- `HTTP_X_NC_PROXIED=1` is injected by the proxy on **every** proxied request,
  authenticated or not.  The PHP shim validates this marker to distinguish
  Rust-proxied requests from direct socket connections (§7.3 / §7.8).
- `HTTP_X_NC_USER`, `HTTP_X_NC_SESSION_TOKEN`, and `HTTP_X_NC_IS_ADMIN` are
  only injected by the proxy after the request has passed Rust-side
  authentication (brute-force check, token validation, 2FA gate).
- Unauthenticated requests (login flows, `.well-known` redirects, public pages,
  the IPublicCapability capabilities probe) do **not** receive any
  `HTTP_X_NC_USER` identity param but still carry `HTTP_X_NC_PROXIED=1`.  PHP
  handles authentication naturally for these routes.

---

## Process user and file ownership

Run `nc-server` as a dedicated non-root system user (e.g. `nextcloud`). This
user must have:

- Read access to `config/config.php`
- Read+write access to the Nextcloud `datadirectory`
- (Local storage only) Ownership of files under `datadirectory`
- Ownership of the PHP-FPM Unix socket (`listen.owner = nextcloud`)

Do not run `nc-server` as `root` or as the same user as any network-facing
service other than PHP-FPM.
