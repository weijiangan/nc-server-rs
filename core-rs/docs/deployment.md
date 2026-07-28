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
