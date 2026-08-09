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
