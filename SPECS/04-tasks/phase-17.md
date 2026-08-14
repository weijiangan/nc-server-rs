# Phase 17 — Performance measurement: benchmark harness + Rust profiling

Goal: a measurement stack that answers the rewrite's core question — *how much faster is Rust than PHP, and where does the time go?* Two layers: a black-box benchmark harness (`nc-bench`) that replays the Phase 16 scenario corpus and load probes against the SUT (Rust, `:8080`) and the oracle (pure PHP, `:9091`) for latency/throughput comparison, and in-process Rust profiling (`pprof-rs` flamegraphs + debug-level `tracing` spans) for hot-spot analysis.

Full plan: [`SPECS/03-implementation-plan/plan/18-performance-measurement-and-profiling.md`](../03-implementation-plan/plan/18-performance-measurement-and-profiling.md).

> **Why this exists.** The rewrite's premise is performance (CLAUDE.md principle 2), yet nothing in the repo currently *measures* it. `nc-difftest` (Phase 16) proves Rust behaves like PHP but says nothing about speed. Every optimization PR so far has been argued from first principles ("PHP does one query, so we do one query") with no numbers behind it. This phase makes performance a measured, tracked property.
>
> The dev docker is already the ideal test bed: SUT and oracle are byte-identical images on **separate** databases (`nextcloud` vs `oracle`), so load tests don't contend or cross-contaminate. The PHP side under test is the **oracle** (`:9091`), not the dev PHP entry `:9090` (which shares the SUT's DB — benchmarking there would measure two servers fighting over one database).

---

## Governing decisions (grounded)

- **The benchmark is black-box, like the differential harness.** `nc-bench` links no `nc-*` server crate; it speaks HTTP through `NextcloudClient` (same reqwest client, same headers, same bodies on both sides — a fair measurement by construction) and reuses `Config::from_env()` (`NC_DIFFTEST_*`), `Scenario::load`, `preconditions::check`, and `run_ops` from `nc-difftest`. The 20+ scenario YAMLs already carry cleanup ops, so replays are re-runnable.
- **The oracle, not `:9090`, is the PHP baseline.** The dev PHP entry shares the SUT's database and instance; load on it contends with the SUT and its writes mutate state the SUT reads. The oracle has its own DB and file tree (Phase 16.1) — clean isolation.
- **Warmup + interleaving, not first-run numbers.** PHP opcache and Rust caches need a warmup pass. Measured iterations alternate the starting side (Rust-first on even iterations, PHP-first on odd) so drift cancels instead of biasing one side.
- **Load sides run sequentially, never concurrently.** Rust load and PHP load would contend for CPU/Postgres/Redis if interleaved; each side's run is isolated with a cooldown between them.
- **SUT-proxied ops are measured as-is, not excluded.** Ops the SUT forwards to PHP-FPM (shares, calendar DAV, most OCS) still have a Rust-side cost (FastCGI proxying, auth, static files). They're labeled `PROXY` vs `NATIVE` in the report so the numbers are interpretable, not hidden.
- **Flamegraphs need unstripped symbols.** `[profile.release]` strips the binary (Cargo.toml); a dedicated `[profile.profiling]` (inherits release, `strip = false`, `debug = "limited"`) is required or pprof output is unresolvable frames.
- **SIGUSR2 reaches nc-server via `docker exec`, not `docker kill`.** PID 1 in the container is `bootstrap.sh`, which backgrounds nc-server; `docker kill -s USR2` would signal the shell. The profile trigger is `docker exec master-nextcloud-1 bash -c 'pkill -USR2 -x nc-server'` (env-gated on `NC_PROFILE_DIR` so production behavior is unchanged).

---

## Verifiable stops

Each stop ends in a concrete, demonstrable gate. Do not proceed past a stop whose gate is red.

| Stop | Tasks | Gate |
|---|---|---|
| **S0 — Bench skeleton** | 17.1, 17.2 | `make bench-one SC=01_propfind_readonly` prints a per-op latency table (p50/p90/mean) for both sides with a ratio; two consecutive runs agree. |
| **S1 — Full suite + load** | 17.3, 17.4 | `make bench` runs the whole scenario corpus, exit 0; `make bench-load` reports req/s + percentiles for all probes; `--json` is well-formed. |
| **S2 — Rust profiling** | 17.5, 17.6 | `make profile` yields a flamegraph SVG with **resolved** function names; `RUST_LOG=nc_server=debug` shows per-handler span trees in SUT logs. |

---

## Tasks

### 17.1 `nc-bench` crate skeleton (workspace plumbing)

- [x] Add `"crates/nc-bench"` to workspace `members`; add `nc-difftest = { path = "crates/nc-difftest" }` to `[workspace.dependencies]` in `core-rs/Cargo.toml`.
- [x] New binary-only crate `core-rs/crates/nc-bench/` (clap subcommands `scenario` and `load`; deps: tokio, anyhow, serde/serde_json; reqwest/sqlx transitively via nc-difftest).
- [x] Subcommand skeleton loads `Config::from_env()` and runs `preconditions::check` before any measurement; exits non-zero on a broken stack.

### 17.2 Scenario latency mode (per-op timing)

- [x] Add `pub elapsed: std::time::Duration` to `OpResult` (`nc-difftest/src/scenario.rs`) and record it per op in `run_ops` — additive; the differential runner reads only `op`/`status`/`body*`.
- [x] `bench scenario [--scenario NAME] [--iterations N] [--warmup N]`: warmup replay per side, then measured replays alternating the starting side; per-side `vars` (share-id capture) mirror the differential runner's flow; `run_cleanup` unmeasured so iterations stay re-runnable.
- [x] Per-op aggregation: p50/p90/p99/mean/max per side + `ratio = php_ms / rust_ms`; scenario-total row; per-op `NATIVE`/`PROXY` label from a path-prefix classifier mirroring `router.rs` (`nc-server/src/router.rs:115-217`).

### 17.3 Load mode (concurrent throughput)

- [x] Read-only probes hammered with `--concurrency C` (default 4) workers for `--duration S` (default 10s) after a `--warmup` (2s); sides run sequentially with a 1s cooldown.
- [x] Default probes: `GET /status.php`, `GET /ocs/v2.php/cloud/capabilities?format=json`, `PROPFIND /remote.php/webdav/` depth 0, depth 1; arbitrary probes via repeated `--probe "METHOD path [depth=N]"`.
- [x] Worker loop records `Instant` deltas into a shared channel; aggregator reports `req/s, p50/p90/p99/max, error count` per probe.

### 17.4 Reporting

- [x] Aligned terminal table per scenario/probe; `--json` emits machine-readable output (kind, per-op/probe stats, ratios) for scripting/CI.
- [x] Exit non-zero if any op/probe errors or a side is unreachable — the suite fails loudly on a broken stack.

### 17.5 `pprof-rs` flamegraph on SIGUSR2

- [x] `pprof` (features `flamegraph`, `prost-codec`) + `prost` in `nc-server`.
- [x] Add `[profile.profiling]` (inherits release, `strip = false`, `debug = "limited"`) to `core-rs/Cargo.toml`.
- [x] `main.rs`: when `NC_PROFILE_DIR` is set, extend the `shutdown_signal` select with a SIGUSR2 branch that spawns: `ProfilerGuardBuilder::default().frequency(1000).build()` → sleep `NC_PROFILE_SECS` (default 10) → `report.flamegraph(svg)` + `report.protobuf(pb)` into `NC_PROFILE_DIR`, logging start/stop at `info!`.

### 17.6 Debug-level tracing spans

- [x] `#[tracing::instrument(skip_all, level = "debug")]` on `nc_dav::handler::dav_handler`; `span!` in `auth_layer` (`middleware/auth.rs`), `php_fpm_fallback` / `nc_fastcgi::proxy_handler`, and the native OCS dispatch.
- [x] Zero cost at the default `info` filter (spans are `debug`-gated); documented usage `RUST_LOG=nc_server=debug` in `docs/benchmarks.md`.

### 17.7 Makefile targets

- [x] `bench` (full scenario suite), `bench-one SC=…`, `bench-load`, `bench-json` — `cargo run -p nc-bench --release` variants.
- [x] `profile`: build with `--profile profiling` → `docker cp` the binary into `master-nextcloud-1` → restart nc-server in-container → `pkill -USR2 -x nc-server` → wait `NC_PROFILE_SECS` → `docker cp` the svg/pb out to `./profiles/`.

### 17.8 Docs + baseline

- [x] `core-rs/docs/benchmarks.md`: methodology (warmup, interleaving, why the oracle not `:9090`, shared-host caveat, proxied-op measurement), commands, probe list.
- [x] Capture the first real run into the baseline table in that doc.

---

## Changes

- **2026-08-10 — Initial write.** Plan for the measurement stack, scoped to bench harness + Rust profiling (PHP-side excimer/blackfire deferred — the image already ships the extensions, `nextcloud-docker-dev/docker/php84/Dockerfile`). The detailed plan lives in `SPECS/03-implementation-plan/plan/18`.
- **2026-08-10 — `.folded` → `.pb` (task 17.5 deviation).** pprof 0.15 does not expose a collapsed-stacks writer (`Report::collapse` does not exist); it offers `flamegraph()` and `pprof()` (a prost `Profile`). The dump is therefore SVG + pprof protobuf, which `go tool pprof`/speedscope consume directly. Task text left verbatim per doc conventions.
- **2026-08-10 — Benchmark auth switched to app tokens (17.2/17.3 addition).** First measurements showed every op at ~120-320 ms on *both* sides: the plain admin password pays a full argon2id verify (~115 ms, `m=65536,t=4`) per request in PHP *and* in Rust — the KDF, not the server, was being measured. `nc-bench` now inserts an `oc_authtoken` row per instance (`hex(sha512(raw + secret))`, `version = 2` — matching PHP `PublicKeyTokenProvider::hashToken()` / `PublicKeyTokenMapper::getToken()`, verified in `workspace/server`) and authenticates with the raw token. The OCS `core/apppassword` endpoint (the desktop-client flow) returns 405 on the oracle, so the direct insert is the bootstrap of record. Falls back to the plain password with a warning.
- **2026-08-10 — Bruteforce-throttle reset (17.2/17.3 addition).** Debugging a persistent ~200 ms "mystery" latency led to `nc-auth`'s throttler: `delay = 100 ms × 2^attempts` for ANY failed login from a subnet in the 12 h window — verified **faithful** to PHP's `Throttler::calculateDelay` (same formula, same window, `workspace/server/.../Throttler.php`). One failed attempt (a stray curl during earlier debugging) throttled the whole dev subnet for half a day, silently adding ~200 ms to every measured request. `nc-bench` now deletes `oc_bruteforce_attempts` on both sides before measuring.
- **2026-08-10 — One client per side (17.2 addition).** The bench previously created a fresh `NextcloudClient` per scenario; the scenario mode now reuses one client per side for the whole run, matching real-client connection behavior.
- **2026-08-10 — The ~200 ms per-request stall was the bruteforce throttle, not the proxy pool.** The stall matched the throttle formula exactly (`100 ms × 2^1`, see the throttle entry), and a direct-to-container request — which bypasses both the proxy and (by subnet) the throttle — took 117 ms, i.e. argon2-only: the keepalive pool adds no per-request latency while the backend process is alive. The pool's genuine failure mode is different: after an nc-server restart, pooled connections to the dead process make proxied requests **hang or 502** until the proxy is restarted (the documented "restart proxy after recreating nextcloud" behavior — observed as multi-second hangs in this phase). `make profile` therefore restarts the proxy after hot-swapping nc-server.
- **2026-08-10 — `make profile` hardening (17.7).** Three failures surfaced on first use: (1) `docker cp` drops the binary's `cap_net_bind_service` file capability (a `setcap` attribute the image's release build carries) — without re-applying it, the profiling binary cannot bind `:80` and the SUT dies; (2) `sudo -u` without `-E` drops `NC_FASTCGI_SOCKET`/`NC_PHP_SHIM`, disabling the FastCGI proxy; (3) the profile output dir must be owned by `www-data`. All three fixed in the target.
- **2026-08-10 — Baseline captured.** Full suite (20 scenarios) + load probes run with the corrected methodology; both `make bench` and `make bench-load` exit 0; results recorded in `docs/benchmarks.md` (Rust 2-60× on native ops, ~1× on shared/preview and proxied ops). Remaining task: 17.8's checkbox for the baseline — done in this entry.
- **2026-08-14 — xdebug mode enforced before measuring (17.2/17.3 addition).** The 08-14 milestone comparison showed the oracle ~2× slower across the board; the milestone doc attributed it to "box CPU contention", which was wrong — the box was idle. A live bisect (status.php 13.5 ms → 4.9 ms on flipping one ini value) pinned the tax on xdebug's `develop` mode (~2.1-2.6× per PHP request; the image defaults to `develop` when `PHP_XDEBUG_MODE` is unset at bring-up, and the 08-13 `down -v` reinstall drifted into it — the 08-10 stack had effectively been running with xdebug off). `nc-bench` now detects the effective mode via `php -i` (CLI and FPM share the conf.d; the mode is a system ini read at process start) and forces `xdebug.mode=off` on **both** instances' php-fpm — the SUT's php-fpm serves the proxied ops that scenario mode measures — by writing a `zz-bench-xdebug.ini` override into the container's ephemeral conf.d layer and reloading php-fpm (USR2), with a post-reload verify. No persistent config is touched; the override dies with the container on the next bring-up, so a drifted stack is re-enforced on every run (same warn-not-fatal style as the throttle reset). The 08-14 milestone numbers in `docs/benchmarks.md` were re-measured under the enforcement; the corrected comparison and root-cause record live there and in the phase-23 Changes log.
