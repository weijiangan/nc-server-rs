# nc-bench — measuring Rust vs PHP (Phase 17)

`nc-bench` is the performance arm of the differential harness: it answers
*how much faster is Rust than PHP, and where does the time go?* It reuses the
nc-difftest machinery (same `NC_DIFFTEST_*` config, same `NextcloudClient`, the
same scenario corpus) so the comparison is fair by construction — identical
headers, bodies, and keep-alive behavior on both sides.

Two measurement modes:

| mode | what it measures | command |
|---|---|---|
| `scenario` | per-op end-to-end latency percentiles of the real difftest operation sequences | `make bench` / `make bench-one SC=…` |
| `load` | concurrent throughput (req/s) + latency percentiles of read-only probes | `make bench-load` |

Both require the live stack (`make diff-up`): the Rust SUT on `:8080` and the
pure-PHP oracle on `:9091`.

## Methodology

- **The oracle, not `:9090`, is the PHP baseline.** The dev PHP entry (`:9090`)
  shares the SUT's database and instance — load on it would contend with the
  SUT and its writes would mutate state the SUT reads. The oracle has its own
  DB and file tree (Phase 16.1), so the two sides are isolated.
- **Warmup before measuring.** One unmeasured replay per side (scenario mode)
  or a warmup window (load mode) lets PHP opcache and the Rust caches reach
  steady state.
- **Interleaving cancels drift** (scenario mode): measured iterations alternate
  the starting side — Rust first on even iterations, PHP first on odd.
- **Load sides run sequentially, never concurrently**, with a 1 s cooldown —
  the two implementations must not contend for the shared Postgres/Redis.
- **SUT-proxied ops are measured as-is.** Ops the SUT forwards to PHP-FPM
  (shares, calendar DAV, most OCS) still have a Rust-side cost. They are
  labeled `PROXY` in the report vs `NATIVE` (Rust-handled) so the numbers stay
  interpretable.
- **App-token auth, not the plain password.** Real clients authenticate with
  app tokens — a cheap SHA-512 lookup (cached 5 min) — while the plain
  password path runs an argon2id verification (~115 ms on this stack in both
  PHP's C implementation and pure-Rust argon2). `nc-bench` creates one token
  per instance by inserting into `oc_authtoken` (`hex(sha512(raw + secret))`,
  `version = 2` — exactly what PHP's `PublicKeyTokenMapper` filters on) and
  authenticates with it. The OCS `core/apppassword` endpoint returns 405 on
  this stack, hence the direct insert. Without tokens the numbers measure the
  KDF, not the server.
- **Bruteforce throttle state is reset first.** Nextcloud (PHP and the
  faithful Rust port) imposes a `100 ms × 2^attempts` sleep on every request
  from a subnet with failed logins in the last 12 h — one stray failed login
  silently adds ~200 ms to every measured request for half a day. The harness
  deletes `oc_bruteforce_attempts` on both sides before measuring (dev-stack
  test bookkeeping).
- **One client per side for the whole run.** Per-scenario connection churn
  through the proxy is avoided by reusing one reqwest client per side, like
  a real client. (The proxy's upstream keepalive pool only misbehaves after
  an nc-server restart — stale connections to the dead process make requests
  hang/502 until the proxy is restarted, which `make profile` does. It does
  not add per-request latency while the backend is alive.)
- **Dev-hardware numbers.** Both instances run on the same host (shared
  Postgres, Redis, CPU). These are relative comparisons on dev hardware, not
  absolute production figures. Report `ratio = php_ms / rust_ms` — >1 means
  Rust is faster.
- **JSON for CI**: `--json` emits a machine-readable report on stdout
  (progress goes to stderr); `make bench-json` wraps the scenario mode.

## Commands

```bash
make diff-up                 # bring the stack up first
make bench                   # full scenario latency comparison
make bench-one SC=10_put_get # a single scenario
make bench-load              # throughput on the default probe set
make bench-json              # scenario comparison as JSON
```

`nc-bench` also accepts extra load probes:

```bash
cargo run -p nc-bench --release -- load \
  --probe "GET /status.php" \
  --probe "PROPFIND /remote.php/webdav/ depth=1" \
  --concurrency 8 --duration 20
```

### Default load probes

| probe | notes |
|---|---|
| `GET /status.php` | always-on native endpoint, no auth |
| `GET /ocs/v2.php/cloud/capabilities?format=json` | native OCS + auth + capability cache |
| `PROPFIND /remote.php/webdav/` (depth 0) | native DAV files tree, collection root |
| `PROPFIND /remote.php/webdav/` (depth 1) | native DAV files tree, one level of children |

## Profiling the Rust side

Two complementary tools:

1. **CPU flamegraphs (pprof-rs, 1000 Hz)**: `make profile` builds a
   symbol-bearing binary (`[profile.profiling]` — release optimizations with
   `strip=false` so frames resolve), hot-swaps it into the SUT container,
   restarts `nc-server` with `NC_PROFILE_DIR` set, runs load, signals SIGUSR2
   mid-run, and copies the dump (`profile-<ts>.svg` flamegraph +
   `profile-<ts>.pb` pprof protobuf) into `./profiles/`. The `.pb` opens in
   `go tool pprof` / speedscope. The swap is ephemeral — `make sut-image up`
   restores the stock binary.
2. **Per-handler span trees**: with `RUST_LOG=nc_server=debug` the SUT logs a
   debug-level span per request for auth, the DAV handler, the FastCGI proxy,
   and the native OCS dispatch, with wall time per handler — the direct
   counterpart to an xhprof/excimer breakdown on the PHP side (the image ships
   excimer/blackfire for future PHP-side profiles).

## Baseline — 2026-08-10 (dev docker, host arch, app-token auth)

Measured on the Phase 17 bring-up stack (`make diff-up`), app-token auth
(hot path), `oc_bruteforce_attempts` reset before the run. Scenario numbers
are per-scenario totals (mean of 5 iterations). Ratio = php/rust, >1 = Rust
faster.

### Scenario totals

| scenario | rust (ms) | php (ms) | ratio |
|---|---|---|---|
| 01_propfind_readonly | 2.9 | 27.4 | 9.6× |
| 10_put_get | 55.0 | 124.8 | 2.3× |
| 10_put_get_delete | 128.1 | 245.3 | 1.9× |
| 11_mkdir_nested | 39.3 | 190.5 | 4.8× |
| 12_move_rename | 175.2 | 342.1 | 2.0× |
| 13_copy | 201.0 | 462.1 | 2.3× |
| 14_propfind_depth1 | 11.9 | 58.4 | 4.9× |
| 15_proppatch_favorite_tags | 7.5 | 33.7 | 4.5× |
| 16_overwrite_put | 176.9 | 278.8 | 1.6× |
| 17_delete_to_trash | 148.7 | 236.4 | 1.6× |
| 18_explicit_mtime | 121.9 | 298.6 | 2.4× |
| 20_chunked_upload_v2 | 50.4 | 748.5 | 14.8× |
| 21_bulk_upload | 1.2 | 24.0 | 19.8× |
| 22_invalid_filename | 1.0 | 24.6 | 25.8× |
| 23_quota_exceeded | 67.7 | 94.7 | 1.4× |
| 24_checksum_upload | 60.7 | 245.7 | 4.0× |
| 25_preview_image | 92.4 | 93.1 | 1.0× |
| 26_preview_unpreviewable | 83.0 | 131.4 | 1.6× |
| 27_imaginary_preview | 106.9 | 107.4 | 1.0× |
| 30_share_create_selfcheck | 97.9 | 101.0 | 1.0× |

Notable per-op wins (mean, from the suite's op-level rows):

| op | rust (ms) | php (ms) | ratio |
|---|---|---|---|
| CHUNKED_V2 PUT (chunk 2, 1 MB) | 3.0 | 193.2 | 63.6× |
| CHUNKED_V2 PUT (chunk 1, 1 MB) | 2.2 | 127.5 | 59.3× |
| CHUNKED_V2 MKCOL | 1.1 | 61.8 | 55.2× |
| PUT invalid filename (rejection) | 1.0 | 24.6 | 25.8× |
| BULK upload (2 files) | 1.2 | 24.0 | 19.8× |
| GET file | 1.3-1.4 | 26-28 | ~19.8× |
| CHUNKED_V2 MOVE assembly | 42.6 | 338.4 | 7.9× |
| PROPFIND depth-0 | 2.9 | 27.4 | 9.6× |
| PROPFIND depth-1 | 5.7-6.2 | 27.4-31.0 | 4.4-5.5× |
| MKCOL | 11.7-12.8 | 49.4-60.2 | 3.9-5.2× |
| PUT (regular) | 53.5-60.3 | 96.8-135.5 | 1.8-2.3× |
| DELETE | 73.2-85.4 | 109.4-121.2 | 1.3-1.7× |

Previews (25-27) sit at parity because both sides share the Imaginary
generation backend; proxied ops (30, `OCS_QUOTA`) sit at parity because both
sides run the same PHP.

### Load probes (4 workers, 10 s)

| probe | rust req/s | php req/s | ratio | rust p50 (ms) | php p50 (ms) |
|---|---|---|---|---|---|
| GET /status.php | 1921 | 866 | 2.2× | 2.17 | 4.50 |
| GET /ocs/v2.php/cloud/capabilities?format=json | 1192 | 161 | 7.4× | 2.43 | 23.06 |
| PROPFIND /remote.php/webdav/ (depth 0) | 1595 | 163 | 9.8× | 2.33 | 22.31 |
| PROPFIND /remote.php/webdav/ (depth 1) | 750 | 157 | 4.8× | 4.58 | 23.37 |

## Known cost centers

Measured or profile-identified costs on the dev stack, ranked by leverage.
Evidence: baseline tables above; first flamegraph pass
(`profiles/profile-1786300646.svg`, 10 s at 1000 Hz under the load probe set;
`profile-1786300846.svg`, light PROPFIND loop).

| cost center | evidence | current impact | status |
|---|---|---|---|
| argon2id verify on plain-password auth | `oc_users.password` is `m=65536,t=4`; PHP `password_verify` 123.9 ms, Rust whole request 117-124 ms direct-to-container (argon2-only baseline) | both sides pay ~115 ms per request; parity Rust ≈ PHP | benchmark uses app tokens; server must keep verifying (PHP parity) |
| bruteforce throttle sleep | `100 ms × 2^attempts` for failed logins in 12 h — faithful to PHP `Throttler::calculateDelay` | 200 ms added to every request after 1 failed attempt; a measurement hazard, not a hot-path cost | harness resets `oc_bruteforce_attempts` |
| axum per-request service-stack cloning | load flamegraph: `Route::clone`, `CloneService::clone_box`, `MapFuture/MapErr/MapIntoResponse::clone` + their `drop_in_place` out-sample all handler frames | largest non-skeleton cost under load; router is ~70-80 routes (`nc-server/src/router.rs`) | improvement target: consolidate DAV mount routes |
| `try_static_files` fs stat per request | ~5% of light-load profile; `router.rs:36` stats every request before routing | every API request pays a `tokio::fs::metadata` + `ServeDir::new`, though static files live only under `/core` `/dist` `/themes` `/apps` | improvement target: path whitelist before the stat |
| `auth_layer` per-request DB work | ~10% combined in light-load profile (`middleware/auth.rs`): two bruteforce COUNT queries, 2FA check, admin-group check, token lookup | ~10% of request CPU at light load; token lookup is cached | improvement target: short-TTL throttler count cache |
| depth-1 PROPFIND per-child work | load probe: depth-1 4.58 ms vs depth-0 2.33 ms p50; scenario 14 at 5.7-6.2 ms | 2× the depth-0 cost; grows with directory size | follow-up: batch per-child property lookups |
| write ops (PUT/DELETE/MOVE/COPY) | baseline: 1.3-2.3× vs PHP — the smallest native wins | real server work (DB writes, propagator), not KDF-bound | watch item: biggest gap among native ops |
| debug-level logging | `RUST_LOG=debug` puts sqlx query statements + span events in the sample set | visible in light-load profile; negligible at `info` | run benchmarks with `RUST_LOG=info` |

## Phase 18 improvements — before/after (2026-08-10)

Three flamegraph-driven changes: DAV route consolidation (~30 routes → 6 via
arbiter classification), static-file path whitelist (no fs stat on API
traffic), throttler COUNT cache (2 s TTL). Measured on the same stack, same
methodology.

### Load probes (4 workers, 10 s)

| probe | before rust req/s | after rust req/s | before p50 (ms) | after p50 (ms) |
|---|---|---|---|---|
| GET /status.php | 1921 | **2280** (+19%) | 2.17 | **1.39** (−36%) |
| GET /ocs/v2.php/cloud/capabilities?format=json | 1192 | **1457** (+22%) | 2.43 | 2.46 |
| PROPFIND /remote.php/webdav/ (depth 0) | 1595 | **1692** (+6%) | 2.33 | 2.29 |
| PROPFIND /remote.php/webdav/ (depth 1) | 750 | **832** (+11%) | 4.58 | **4.04** (−12%) |

### Flamegraph clone-machinery share (10 s load dump)

35.0% of frames before → 33.6% after. The remaining router (~45 registry/OCS/
static-PHP routes) still dominates the per-request axum clone cost — the
registry's explicit per-app 404 semantics (Phase 7.5) is a deliberate
tradeoff, so the full clone win awaits a different architecture (single
catch-all + in-handler dispatch) if it is ever wanted.

### Scenario suite totals

Within run-to-run noise (shared host, no quiescence): a few scenarios −12-30%
(16_overwrite_put −30%, 17_delete_to_trash −13%, 14_propfind_depth1 −12%,
21_bulk −15%, 22_invalid −18%), a few +7-14% (10_put_get, 18_explicit_mtime,
20_chunked), the rest flat. The differential suite stays 20/20 green.
