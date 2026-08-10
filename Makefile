# Repo-root Makefile — dev docker + differential harness orchestration.
#
# The inversion made nc-server-core the parent repo: nextcloud-docker-dev (a
# fork of juliusknorr/nextcloud-docker-dev) and workspace/server (the PHP
# reference) are submodules.  The Rust source lives at core-rs/ and reaches the
# php84 image build as podman's `rustsrc` build context — compose's classic
# builder cannot pass additional build contexts, so the shared image is built
# here with `podman build --build-context` and the compose services only
# reference `master-nextcloud:latest` (both SUT and oracle — byte-identical by
# construction, a correctness requirement for the differential oracle).

SHELL := /bin/bash

FORK    := nextcloud-docker-dev
COMPOSE := docker compose -f $(FORK)/docker-compose.yml

.PHONY: sut-image up wait diff-up diff-test diff-one

# ── Build ─────────────────────────────────────────────────────────────────────
# Build the shared SUT/oracle image.  core-rs is supplied by name so the
# Dockerfile's `COPY --from=rustsrc …` lines resolve; the build uses buildah
# cache mounts (--mount=type=cache) so cargo registry + target survive
# rebuilds — unchanged builds are near-instant.
sut-image:
	podman build --build-context rustsrc=$(CURDIR)/core-rs \
		-t master-nextcloud:latest \
		-f $(FORK)/docker/php84/Dockerfile $(FORK)/docker

# ── Bring-up ──────────────────────────────────────────────────────────────────
up: sut-image
	$(COMPOSE) up -d --build proxy
	$(COMPOSE) up -d nextcloud oracle database-pgsql redis previews_hpb
	$(COMPOSE) restart proxy

wait:
	@echo "Waiting for both instances to report installed:true …"; \
	until curl -fsS -H 'Host: nextcloud.local' http://127.0.0.1:8080/status.php 2>/dev/null | grep -q '"installed":true'; do sleep 2; done; \
	echo "  ready: SUT    http://127.0.0.1:8080  (Rust)"; \
	until curl -fsS -H 'Host: oracle.local' http://127.0.0.1:9091/status.php 2>/dev/null | grep -q '"installed":true'; do sleep 2; done; \
	echo "  ready: Oracle http://127.0.0.1:9091  (pure PHP)"

diff-up: up wait
	@echo "stack ready — run make diff-test"

# ── Differential suite (Phase 16) ─────────────────────────────────────────────
# The scenario suite runs as #[ignore]d integration tests (no DB/network in a
# plain `cargo test --lib`).  diff-one filters with S=, e.g. `make diff-one S=27`.
diff-test:
	cd core-rs && cargo test -p nc-difftest --release -- --ignored

diff-one:
	cd core-rs && cargo test -p nc-difftest --release -- --ignored $(S)

# ── Performance budget gate (Phase 20) ───────────────────────────────────────
# Fails when any request class exceeds its query-count budget
# (core-rs/perf-budget.yaml).  Needs a live stack (`make diff-up`).
perf-gate:
	cd core-rs && cargo run --release -p nc-bench -- budget --budget perf-budget.yaml

# ── Benchmarking / profiling (Phase 17) ──────────────────────────────────────
# All bench targets need a live stack (`make diff-up`).  `nc-bench` compares
# the Rust SUT (:8080) against the pure-PHP oracle (:9091) on the same stack;
# config comes from the same NC_DIFFTEST_* env vars as the differential suite.
BENCH := cd core-rs && cargo run -p nc-bench --release --

bench:           # full scenario latency comparison (per-op p50/p90/mean + ratio)
	$(BENCH) scenario

bench-one:       # a single scenario, e.g. `make bench-one SC=10_put_get`
	$(BENCH) scenario --scenario $(SC)

bench-load:      # concurrent throughput on the read-only probe set
	$(BENCH) load

bench-json:      # scenario comparison as machine-readable JSON on stdout
	$(BENCH) scenario --json

# Capture a CPU profile of the SUT *under load* (pprof-rs, 1000 Hz):
#   1. build the symbol-bearing profiling binary (`[profile.profiling]` keeps
#      strip=false so flamegraphs resolve) and hot-swap it into the SUT,
#   2. restart nc-server in-container with NC_PROFILE_DIR set,
#   3. run `bench load` in the background, SIGUSR2 mid-run, wait the window,
#   4. copy the flamegraph SVG + pprof protobuf out to ./profiles/.
# The swap is ephemeral (lost on container recreate) — run `make sut-image up`
# to restore the stock binary.
PROFILE_SECS ?= 10
PROFILES := profiles

profile:
	@echo "── 1/4 building profiling binary (strip=false, debug info)…"
	cd core-rs && cargo build --profile profiling --bin nc-server
	@echo "── 2/4 hot-swapping into the SUT container and restarting nc-server…"
	@mkdir -p $(PROFILES)
	docker cp core-rs/target/profiling/nc-server master-nextcloud-1:/usr/local/bin/nc-server
	# The image's release binary carries cap_net_bind_service via setcap (a
	# filesystem attribute that docker cp does not carry) — re-apply it or the
	# profiling binary cannot bind :80 and the SUT dies.
	docker exec master-nextcloud-1 setcap 'cap_net_bind_service=+ep' /usr/local/bin/nc-server
	docker exec master-nextcloud-1 bash -c 'mkdir -p /tmp/nc-profile && chown www-data:www-data /tmp/nc-profile'
	docker exec master-nextcloud-1 bash -c 'pkill -x nc-server || true'
	sleep 1
	# `sudo -E` keeps the container env (NC_FASTCGI_SOCKET, NC_PHP_SHIM) that
	# the bootstrap process inherited — dropping them disables the FastCGI
	# proxy and every PHP-FPM-bound route 502s.
	docker exec -d master-nextcloud-1 bash -c 'sudo -E -u www-data env NC_PROFILE_DIR=/tmp/nc-profile NC_PROFILE_SECS=$(PROFILE_SECS) /usr/local/bin/nc-server --root /var/www/html --listen 0.0.0.0:80'
	sleep 2
	# The proxy keepalive pool holds connections to the old process — restart
	# it or the SUT appears ~300 ms slow / hangs (documented `up` behavior).
	$(COMPOSE) restart proxy
	@echo "── 3/4 loading the SUT, then signalling SIGUSR2 mid-run…"
	@(cd core-rs && cargo run -p nc-bench --release -- load --duration $(shell echo $$(($(PROFILE_SECS) + 4))) >/dev/null 2>&1 & \
	  sleep 3; docker exec master-nextcloud-1 bash -c 'pkill -USR2 -x nc-server'; wait)
	@echo "── 4/4 fetching the dump…"
	@docker cp master-nextcloud-1:/tmp/nc-profile/. ./$(PROFILES)/
	@ls $(PROFILES)/profile-*.svg $(PROFILES)/profile-*.pb 2>/dev/null || true
