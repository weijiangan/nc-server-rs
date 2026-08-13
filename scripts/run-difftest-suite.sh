#!/bin/bash
# Run the full differential scenario suite (the milestone procedure).
#
# `make diff-test` only runs preconditions_pass; the real suite is one
# `difftest run <scenario.yaml>` per YAML.  This loops over every scenario,
# records pass/fail per scenario, prints the divergence tail for failures,
# and exits non-zero when any scenario diverges.
#
# Needs a live stack (`make diff-up`) with FULL readiness — installed:true on
# both instances, installing.html gone, and the ~90 s settle (background
# `occ user:add` and install tail-writes land after installed:true and race
# early scenarios — see CLAUDE.md "Differential-test operations").
set -u
cd "$(dirname "$0")/../core-rs"

cargo build --release -p nc-difftest >/dev/null || exit 1
BIN=target/release/difftest

PASS=0; FAIL=0; FAILED=""
for s in crates/nc-difftest/scenarios/*.yaml; do
  name=$(basename "$s" .yaml)
  out=$("$BIN" run "$s" 2>&1)
  rc=$?
  if [ $rc -eq 0 ]; then
    PASS=$((PASS+1)); echo "PASS  $name"
  else
    FAIL=$((FAIL+1)); FAILED="$FAILED $name"
    echo "FAIL  $name (rc=$rc)"
    echo "$out" | tail -8 | sed 's/^/      /'
  fi
done
echo "=============================="
echo "SUITE: $PASS passed, $FAIL failed"
[ -n "$FAILED" ] && echo "failed:$FAILED"
[ $FAIL -eq 0 ]
