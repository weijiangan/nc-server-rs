# Packaging nc-server

`nc-server` ships as two artifacts that **must stay in lockstep** — always
build and package them from the same source revision:

| Artifact | Installed location | Source |
|---|---|---|
| `nc-server` binary | `/usr/bin/nc-server` | `target/release/nc-server` |
| PHP bootstrap shim | `/usr/share/nc-server/php-shim/index.php` | `php-shim/index.php` |
| systemd unit | `/usr/lib/systemd/system/nc-server.service` | this directory |
| env overrides | `/etc/nc-server/nc-server.env` | `nc-server.env.example` |

`PKGBUILD` in this directory builds and installs all of the above from the
surrounding checkout (`cd packaging && makepkg -si`).

`/usr/share/<pkg>` is the FHS location for architecture-independent
read-only data.  The shim is code the server invokes, **not** configuration
— never install it under `/etc` (admin edits would silently break the trust
boundary, and package upgrades must replace it automatically).

## Shim path resolution (runtime)

1. `NC_PHP_SHIM` env var — explicit override (full path to `index.php`).
2. Compiled-in packaged default — used only when that file exists.
3. In-tree development layout — `{nc_root}/core-rs/php-shim/index.php`.

The resolved path is logged at startup (`PHP-FPM proxy enabled … shim=…`).
If it does not exist, `nc-server` logs a warning and every PHP-FPM-proxied
request returns 502.

## Build-time configuration

The packaged default is `$NCSHIMDIR/php-shim/index.php`, with `NCSHIMDIR`
defaulting to `/usr/share/nc-server`.  Retarget for prefix-style installs:

```sh
NCSHIMDIR=/opt/nc-server/share cargo build --release
# shim default → /opt/nc-server/share/php-shim/index.php
```

Standard distro packaging does not need this — install to
`/usr/share/nc-server` and the default applies.

## Reference install

```sh
cd packaging && makepkg -si
systemctl enable --now nc-server
```

Before starting, adjust `--root` / `--listen` in the unit and its
`User=`/`Group=` to match the PHP-FPM pool user (the FastCGI socket is
0600-owned by that user — see `docs/deployment.md`).

## Notes for distro packagers

- Ship the binary and the shim in **one package, one version**.  The two
  implement a shared trust protocol (`HTTP_X_NC_PROXIED` marker and the
  `X-NC-*` identity headers — see `docs/deployment.md`, "FastCGI trust
  boundary"); a version-skewed pair is a correctness and security risk.
  Same-version packaging makes skew impossible, and `rpm -V` / `dpkg -V`
  then cover post-install tampering for free.
- For non-standard filesystem layouts, prefer `NC_PHP_SHIM` in
  `/etc/nc-server/nc-server.env` over rebuilding with `NCSHIMDIR` — the
  operator can fix it without a recompile.
