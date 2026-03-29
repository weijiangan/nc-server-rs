# nc-server Deployment Guide

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
