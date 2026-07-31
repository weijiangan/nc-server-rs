# Phase 12 handover — updated 2026-07-31

Status: **capabilities, basic auth, home-root permissions, and the proxied-OCS 500s are all fixed and verified byte-equivalent to PHP** using a local A/B harness (same DB). **The iOS app still does not fire `PROPFIND Depth:1` / show files against Rust**, while a fresh PHP account on the *same device, same `admin` user, same database* does (`Depth:0 → Depth:1 → files`). Current investigation + next steps below. This supersedes the 2026-07-30 version of this doc.

---

## 1. The goal

Make the Nextcloud iOS client (34.0.0) list files against the Rust server. Symptom: the app authenticates, does a root `PROPFIND` `Depth:0`, and **never escalates to the `Depth:1` listing**, so the files view is empty. Against PHP (same device/account/DB) the same account works and does fire `Depth:1`.

## 2. What is FIXED and verified (do not redo)

All verified against the **local dev PHP** (see §6) on the **same database** — not against production.

- **Capabilities (the original "gate").** `nc-ocs/src/handlers.rs::ocs_capabilities` read `request.extensions().get::<Option<AuthInfo>>()`, but the auth middleware inserts `AuthInfo` (not `Option<AuthInfo>`), so `is_authenticated` was always false and every authenticated client got the *public* subset. Fixed to `get::<AuthInfo>().is_some()`. Authenticated capabilities are now **byte-for-byte identical to PHP** (13 keys, `files_sharing` present, `version.major=34`). The internal PHP-app capability fetch already returned the full set once authenticated correctly.
- **Version ("Bug A") — resolved, no longer a bug.** `version.major` is `34` (was `0`). `status.php` and capabilities version now come from `config.php` (`34.0.0.1` → string `34.0.0`). The only residual is the cosmetic ` dev` channel suffix (PHP `34.0.0 dev` vs Rust `34.0.0`), see §9.
- **Basic auth (argon2).** `oc_users.password` is `3|$argon2id$…`; Rust only did `bcrypt::verify`, so `admin:admin` → 401. Added `nc-auth::hasher` (PHP-`Hasher`-compatible: argon2id / argon2i / bcrypt / legacy SHA-1), wired `passwordsalt` through `nc-db::config`, off-loaded the CPU-heavy verify. Plain-password login works; app-token login was already fine.
- **Home-root permissions.** Reverted the unconditional `& !16` SHARE-bit strip in `nc-dav/src/filesystem.rs::get_props` (it matched only a stale cold-start capture). Verified PHP returns `RGDNVCK`/`31`/`["share","read","write"]` for the home root in steady state; Rust now matches. Corrected the regression tests that asserted the wrong home-root=15 behavior (`row.rs`, `props.rs`); the pure encoding tests remain valid.
- **Proxied-OCS `$userId` injection (the 500s).** `user_status`, `dashboard/widgets`, etc. returned HTTP 500 (`Argument #1 ($userId) must be of type string, null given`). Root cause: the php-shim used `IUserSession::setVolatileActiveUser()`, which sets the in-memory active user (so `getUser()`/`isLoggedIn()` worked) but does **not** write `user_id` to the session. The AppFramework injects controller `$userId` from `ISession::get('user_id')` (`AppFramework\DependencyInjection\DIContainer` `userId` service). Fixed: the shim now calls `IUserSession::setUser()` (what a real login does). All proxied OCS startup endpoints now match PHP (`user_status`/`dashboard`/`notifications`/`recommendations`/`push`/`cloud/users` → same status codes, no 500s), and Rust emits the identical session cookies PHP does.

## 3. The current open bug

A **fresh** Rust account: the app does `PROPFIND Depth:0` on `/remote.php/dav/files/admin` and **never does `Depth:1`**; files don't appear. A **fresh** PHP account on the same device/`admin`/DB does:
```
…/status.php            200
…/ocs/v2.php/cloud/users/admin   200
…/remote.php/dav/files/admin     PROPFIND 207  (Depth:0, ~2.9kb)
…/ocs/v1.php/cloud/capabilities  200
…/ocs/v2.php/apps/files/api/v1/directEditing  200
…/remote.php/dav/files/admin     PROPFIND 207  (Depth:1, ~3.2kb)  ← files load
```

**Verified equivalent between Rust and PHP (therefore NOT the cause):** `status.php`, `cloud/users/admin`, `capabilities` (key set + dav/files blocks), `directEditing`, `notifications`, `notifications/push`, `recommendations`, `user_status`, `dashboard/widgets`, and `PROPFIND Depth:0` on the root (200-propstat properties *and* response headers).

**Unconfirmed variable:** Rust's PROPFIND XML *serialization* differs from PHP in ways that are standards-valid but unusual — `<d:resourcetype xmlns:d="DAV:"><D:collection></D:collection></d:resourcetype>` (mixed `d:`/`D:` prefixes, redundant `xmlns:d`, non-self-closing) vs PHP's `<d:resourcetype><d:collection/></d:resourcetype>`; and raw `"` in `getetag` vs PHP's `&quot;`. A conformant parser treats them identically, but the iOS app's WebDAV parser is the real arbiter and this has not been confirmed one way or the other for that parser.

## 4. What to try next (in order)

1. **Get the fresh Rust account's mitmproxy flow** and diff it against the working PHP flow above — see exactly which request the app makes and where it stops/diverges. (The earlier Rust capture was a *stale* account that did `REPORT` + per-file `PROPFIND`s; a fresh account's flow is the missing data.)
2. If the app parses the `Depth:0` fine but skips `Depth:1`, hunt the *decision driver* — likely a capability nuance or a response detail the app keys the listing strategy off.
3. **Make Rust's PROPFIND serialization match PHP byte-for-byte** (self-closing `<d:collection/>`, consistent lowercase `d:` prefix, no redundant `xmlns`, `&quot;`-escaped etag) and retest — this eliminates the serialization variable regardless of theory.
4. Fix the `DAV` response header trailing `, 2` Rust emits that PHP doesn't.

## 5. What has been tried and RULED OUT

- `oc:permissions` value as the gate — fixed to match PHP anyway; not the listing gate.
- Capabilities completeness — fixed; identical to PHP.
- Basic auth — fixed (argon2).
- Proxied-OCS 500s — fixed (shim `setUser`).
- "Proxy capabilities to PHP" — retracted; native capabilities is in-scope and now correct.
- The old `comparison.md` PHP capture showing `GDNVCK`/`15` — a stale cold-start (LazyUserFolder) artifact; live PHP is `RGDNVCK`/`31`.
- **Using `cloud.home.lan` / `cloud2.home.lan` as references — STOP.** They are a different environment (production, version `34.0.1`, extra apps) and `cloud2` is stale Rust. Use the local A/B harness (§6). All traces of them as a verification target are removed from this doc.

## 6. Local A/B testing harness (USE THIS)

The dev docker runs **both** the Rust `nc-server` (port 80) and php-fpm inside `master-nextcloud-1`. The proxy (`master-proxy-1`) now exposes two clean entries on the LAN:

| entry | URL | path |
|---|---|---|
| **Rust** | `http://<lan-ip>:8080` | proxy `default_server` → `nextcloud:80` |
| **PHP** | `http://<lan-ip>:9090` | proxy nginx vhost → php-fpm TCP `nextcloud:9000` (bypasses Rust) |

`<lan-ip>` is `192.168.50.96` here. mitmproxy runs on `8081`, so PHP was put on `9090` to avoid the clash.

**What was added to build this:**
- `docker/configs/php-fpm-tcp.conf` — a second php-fpm pool `[tcp]` on `:9000`. The `[www]` pool stays on the unix socket for the Rust `nc-server`. Named `zzzz-*` (mounted as `…/php-fpm.d/zzzz-tcp.conf`) so it loads *after* the header-less `zzz-nextcloud.conf` (whose directives otherwise re-apply to the last-opened pool and would steal `:9000`).
- `docker/nginx/my_proxy.conf` — added a PHP `server` block (`listen 9090;` + `server_name php.dev.local;`) using the official Nextcloud fastcgi config (`fastcgi_pass nextcloud:9000`, `fastcgi_split_path_info`, `SCRIPT_FILENAME /var/www/html$fastcgi_script_name`). The official `try_files $fastcgi_script_name =404` is dropped (files live in the other container). **Static assets** (`css|js|svg|png|…`) are proxied to `nextcloud:80` (Rust serves the same `/var/www/html`) so the web **login flow renders** for the iOS app; all API/DAV stays on pure php-fpm.
- `docker-compose.yml` — proxy port `9090:9090`; `nextcloud` mounts `php-fpm-tcp.conf`.
- `docker/bin/bootstrap.sh` + runtime `occ` — `trusted_domains` now includes `192.168.50.96:8080`, `192.168.50.96:9090`, `php.dev.local`, `rust.dev.local`. (PHP enforces `trusted_domains` **including the port**; Rust is laxer.)

**Comparing an endpoint:**
```bash
curl -H "Host: rust.dev.local"        "http://127.0.0.1:8080/<path>"   # Rust
curl -H "Host: 192.168.50.96:9090"    "http://127.0.0.1:9090/<path>"    # PHP
```
**iOS app:** add the Rust account at `http://<lan-ip>:8080` and the PHP account at `http://<lan-ip>:9090`, both as `admin`.

## 7. Rebuilding the dev docker

Docker is **podman** ("Emulate Docker CLI using podman").
```bash
cd nextcloud-docker-dev
# After Rust or php-shim changes (rebuilds nc-server and re-copies the shim):
docker compose up -d --build nextcloud
# After proxy/nginx config or compose port changes:
docker compose up -d --build proxy
# IMPORTANT: after recreating `nextcloud`, the proxy caches the old upstream IP
# and returns 502 — restart it:
docker compose restart proxy
```
- Logs: `docker logs master-nextcloud-1`
- DB: `docker exec master-database-pgsql-1 psql -U postgres -d nextcloud`
- The php-shim is baked into the image at `/usr/local/share/nc-server/php-shim/index.php`. For a quick live test you can `docker cp core-rs/php-shim/index.php master-nextcloud-1:/usr/local/share/nc-server/php-shim/index.php` (PHP reads it per-request, no restart needed) — but rebuild to persist.
- **Gotcha:** `trusted_domains` set via `occ` is wiped on a full reinstall; persist additions in `docker/bin/bootstrap.sh` (the `NEXTCLOUD_TRUSTED_DOMAINS` line). Also avoid **sparse** `trusted_domains` indices (e.g. setting index 90) — PHP `json_encode`s a sparse array as an object, which the Rust config loader rejects (`invalid type: map, expected a sequence`) and `nc-server` won't start.

## 8. Environment facts

- The **dev docker** (`master-*` containers) is the only verification target. `master-nextcloud-1` runs the Rust `nc-server` on `:80` **and** php-fpm. `master-proxy-1` fronts it (`8080→80`, `8443→443`, `9090→9090` PHP entry).
- `cloud.home.lan` = production PHP, `cloud2.home.lan` = production Rust (stale). Both resolve to `10.99.0.2` (the **production** proxy on `:443`), *not* the dev docker. **Do not use them for verification.**
- dev instance id `oc4cab2jkst1`; `admin` home fileid `2`; `config.php` version `34.0.0.1`.
- The PHP reference source is `workspace/server/` (mounted at `/var/www/html` in the container).

## 9. Known remaining divergences (non-blocking, queued as tasks)

- **File-level PROPFIND:** Rust returns `quota-available-bytes`/`quota-used-bytes` in the 200-propstat for regular *files* (PHP returns them only on the home root/collections, 404 on files), and Rust populates `oc:downloadURL` with a webdav URL (PHP returns it empty).
- **Capabilities cosmetics:** `core.user.timezone` (Rust `UTC` vs the user's zone), `core.mod-rewrite-working` (Rust `false` vs `true`), `version.string` missing the ` dev` channel suffix, and URL bases (`files.directEditing.url`, `ocm.endPoint`, `theming.*`) use `overwrite.cli.url` (`localhost`) instead of the request host.
- **DAV response header:** Rust appends `, 2` to the `DAV` compliance list; PHP doesn't.
- **PROPFIND XML serialization** cosmetics (see §3) — to be made byte-identical to PHP.

## 10. Uncommitted changes

Working tree (uncommitted) spans: `nc-ocs` (handlers/capabilities), `nc-auth` (new `hasher`), `nc-db` (config `passwordsalt`), `nc-server` (middleware/auth), `nc-dav` (filesystem/row/props permission fixes + tests), `nc-fastcgi` (capability-fetch logging), `php-shim/index.php` (`setUser`), plus the docker harness files (`docker/nginx/my_proxy.conf`, `docker/configs/php-fpm-tcp.conf`, `docker-compose.yml`, `docker/bin/bootstrap.sh`). `cargo test --lib` passes except the pre-existing, environmental `nc-fastcgi::registry_scans_real_apps_dir`.
