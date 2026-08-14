# Phase 22 — Remaining plan items T6–T10

Goal: execute the rest of the amended order from plan section 21 (after Tier 1): **T6** prop-set filtering + batch-family merges, **T3** native `PgPool` enum, **T4** native array binds, **T7** single-query CTE PROPFIND, **T9** write path, **T2** pipelining, **T8** cleanup, **T10** index audit. T6 removes statements first (budgets drop); T3 is the structural unlock for T4/T7/T2.

Full plan: [`SPECS/03-implementation-plan/plan/21-propfind-round-trip-reduction.md`](../03-implementation-plan/plan/21-propfind-round-trip-reduction.md) — the tiered task list (T1-T10), grounded decisions, and the execution order. Dependencies: T4 ← T3, T7 ← T3, T2 ← T3; T6 and T10 are independent.

---

## T6 — Prop-set filtering + batch-family merges

Goal: stop doing work the client didn't ask for, and merge the batch families that scan the same table twice. The first tier that **removes statements** — the perf-budgets get lowered (delta 9 → 7, propfind_depth1 20 → 18), not just held. Plan finding 6 + the 6a-6e trims.

### T6 decisions (grounded)

- **The requested-prop plumbing already exists in the vendored dav-server — only the last hop is missing.** `PropWriter` carries `requested: Option<Vec<(Option<String>, String)>>` (namespace, name) for explicit `<prop>` requests, `None` for allprop/propname (`vendor/dav-server/src/handle_props.rs:233-236`, the PHASE-12.1 patch; the parsed `<d:prop>` list lands at `handle_props.rs:675`). The FS driver never sees it. The patch is a per-request setter on the filesystem (a new trait method with a default no-op, so `voidfs`/`memfs`/`localfs` are untouched), not a signature change.
- **allprop/empty-body requests are unaffected — the harness and the perf-gate both send bare PROPFINDs (no body → allprop).** Filtering only kicks in for explicit `<prop>` bodies (the desktop client). So the milestone suite is unchanged, and the perf-gate budgets drop **only** from the merges; the filtering win is verified by replaying the desktop client's actual prop set.
- **Family → prop mapping**: `prefetch_tags` → `{oc:}favorite`, `{oc:}tags` · `count_children_batch` → `{nc:}contained-*-count` · `share_details_batch` → `{oc:}share-types`, `{nc:}sharees` · `share_notes_batch` → `{nc:}note` · `comments_counts/unread` → `{oc:}comments-count`/`-unread` · `system_tags_batch` → `{nc:}system-tags` · `custom_properties_batch` → any prop outside the known `d:`/`oc:`/`nc:` namespaces. A family is skipped when the requested set is non-empty and contains none of its props.
- **Merge semantics differ per pair — do not blind-merge.** `share_details_batch` filters `share_type IN (0,1,3,4,6,7,10,12)` + uid conditions; `share_notes_batch` is `note != '' ORDER BY file_source, stime DESC` (most-recent note per file, no share-type filter) — one scan, split in Rust. The comments pair merges as one `GROUP BY c.object_id` with `COUNT(*)` + `COUNT(*) FILTER (WHERE …)` (the unread predicate). `count(*) FILTER` is SQLite-safe (≥3.30). The unread-marker `LEFT JOIN` de-correlation is a **separate task** (below) — each piece stays individually verifiable against the SQLite batch-vs-single tests.
- `count_children_batch` can restrict its list to directory children (read_dir has the children rows in memory); `SUM(CASE WHEN …)` → `count(*) FILTER (WHERE mimetype = …)`.

### T6 stops

| Stop | Tasks | Gate |
|---|---|---|
| S0 — Merges | T6.1-T6.3 | `cargo test --lib` (batch-vs-single parity pins every merge); perf-gate re-measured with **lowered** budgets (delta 9 → 7, propfind_depth1 20 → 18); bench SC=14 flat. |
| S1 — De-correlation | T6.4 | `cargo test --lib`; perf-gate holds. |
| S2 — Filtering | T6.5, T6.6 | `cargo test --lib`; perf-gate holds (allprop unchanged); desktop-client prop-set replay + `EXPLAIN ANALYZE` on the Postgres path: skipped families' CTE SubPlans show `never executed` (the statement-log replay is unfalsifiable post-T7 — the gated families are subplans, not statements; see the T6.6 deviation). |

### T6 tasks

- [x] **T6.1** Merge `share_details_batch` + `share_notes_batch` into one `oc_share` scan (`row.rs`): one query with the details filter (uid conditions + share_type list), rows split in Rust — `note != ''` rows feed the notes map with per-file most-recent-`stime`; all rows feed `share_details` (preserving `ShareDetail` + the display-name batch). Extend the SQLite tests to pin the merged behavior.
- [x] **T6.2** `count_children_batch`: `SUM(CASE WHEN …)` → `count(*) FILTER (WHERE mimetype = …)`; filter `parent_ids` to directory children (mimetype == dir_mime_id) before building the list.
- [x] **T6.3** Merge `comments_counts_batch` + `comments_unread_batch` into one `GROUP BY c.object_id` with `COUNT(*)` + `COUNT(*) FILTER (WHERE …)`; `read_dir`'s comment-map filling consumes (count, unread) per fileid; update `comments_batches_match_singles`.
- [x] **T6.4** De-correlate the unread-marker subquery: `LEFT JOIN {prefix}comments_read_markers m ON m.user_id = $uid AND m.object_type = 'files' AND m.object_id = c.object_id`, `COALESCE(m.marker_datetime, '1970-01-01 00:00:00')`; same in the single-row `get_comments_unread` fallback.
- [x] **T6.5** Vendored patch: new `Filesystem` trait method (default no-op) receiving the parsed requested props; `handle_propfind` calls it; `NcFileSystem` stores them per-request with a `prop_requested(ns, name)` helper.
- [x] **T6.6** Gate each batch family in `read_dir` (`filesystem.rs:1445-1547`) on the family→prop mapping; `custom_properties_batch` skipped when every requested prop is in the known namespaces; the lazy `cache/` ensure (phase-21) stays outside the filtering.

---

## T3 — Native `PgPool` enum

Goal: `enum DbPool { Pg(PgPool), Sqlite(SqlitePool) }` replacing `sqlx::AnyPool` — native binary decode, no `AnyValue` boxing, and the prerequisite for T4/T7/T2. Plan finding 3.

**Decisions**: the enum implements sqlx's `Executor`/`Acquire` by delegation, so existing call sites compile unchanged (the refactor is the enum + delegation impls, not a sweep). `backend_is_postgres()` (21.1) and the propagator's `tx.backend_name()` checks become the enum variant. SQLite becomes a native `SqlitePool`; the `any` driver + `install_default_drivers()` are dropped (the `postgres` feature becomes explicit, `nc-db/Cargo.toml:10`). **No SQL changes in this phase** — the win is the driver; verification is behavior-neutral + latency. The 21.1 pool flags carry to both arms.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T3.1, T3.2 | Workspace builds + `cargo test --lib` (SQLite arm); perf-gate holds; bench SC=14 p50 ≤ 21.4 numbers. |
| S1 | T3.3 | Builds without the `any` feature; unit tests green; milestone suite (D-gates). |

- [x] **T3.1** The enum in `nc-db/src/pool.rs`; `build_pool` picks the variant from `config.dbtype`; `impl<'c> Executor<'c>` (+ `Acquire` where `pool.acquire()`/`pool.begin()` are used — the propagator).
- [x] **T3.2** Replace the dialect checks (`backend_is_postgres()`, `tx.backend_name()`) with the variant; a `DbTxn` enum may be needed for `begin()` call sites.
- [x] **T3.3** Drop the `any` driver: remove `install_default_drivers()`, make the `postgres`/`sqlite` features explicit, shed the Any driver from `Cargo.lock`; delete the `string_to_array`-era comments referencing the Any limitation (the SQL stays until T4).

---

## T4 — Native array binds

Goal: replace the `string_to_array($1, ',')` interim (21.3) with real `= ANY($1::bigint[])` / `= ANY($1::text[])` binds on the native pool — the `Any` driver's missing array kind forced the interim (`sqlx-core any/value.rs` has no `Array`). `custom_properties_batch` finally gets its `text[]` bind (the comma-in-filename hazard that kept it on `IN`). Plan finding 4 (fix amendment).

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T4.1, T4.2 | `cargo test --lib` (SQLite keeps `IN`); perf-gate holds; statement-text stability probe still bounded; milestone suite. |

- [x] **T4.1** Convert the 21.3 helpers to `Vec<i64>`/`Vec<String>` binds on the `Pg` arm (`count_children_batch`, `share_details_batch` + its users/accounts lists, `share_notes_batch`, `comments_counts_batch`, `comments_unread_batch` (post-merge), `system_tags_batch`, `list_extended_batch`, `lookup_by_ids`, the propagator's pre-lock + UPDATEs) — the dialect branch collapses to the enum variant, the `ids_csv` helper and `string_to_array` die.
- [x] **T4.2** `custom_properties_batch` → `propertypath = ANY($1::text[])` on the `Pg` arm; remove the "stays on IN" comment.

---

## T7 — Single-query CTE PROPFIND

Goal: the whole child fan-out in one statement — `LATERAL` + `json_agg` sub-selects keyed on `fileid` — one round trip for the entire depth-1 batch. Plan finding 2.

**Decisions**: Postgres-only (SQLite keeps the batch path behind the variant). The difftest cannot distinguish (same bytes) — the gates are the perf-budget (scaling delta → ~1) and the milestone suite. The `json_agg` result decodes in Rust into the same per-family shapes the merges (T6) produce — the merges make this tractable. The statement must preserve the per-family semantics (share notes' `stime DESC` per file, the unread filter, the dir counts' mimetype split). The lazy `cache/` ensure and the root's single-row fallbacks stay as-is.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T7.1 | Core families in the CTE (children + extended + dir counts); perf-gate re-measured (delta drops hard — re-measure, then lower). |
| S1 | T7.2 | Shares/comments/tags/props families; perf-gate; milestone suite. |

- [x] **T7.1** The `WITH kids AS (SELECT fc.*, fe.metadata_etag, … FROM … WHERE parent = $1 AND storage = $2)` + `LATERAL`/`json_agg` sub-selects for children + extended + `count(*) FILTER` dir counts; decode in Rust.
- [x] **T7.2** Add the remaining families (shares+notes from the merged scan, comments counts+unread, system tags, custom props) as `LATERAL` sub-selects; drop the corresponding batch calls; re-measure and lower the budgets.

---

## T9 — Write path

Goal: fold the propagation `BEGIN` → `SELECT … FOR UPDATE` → `UPDATE` → `COMMIT` into one CTE statement (same deadlock-avoiding `ORDER BY path_hash` lock order, 1 RTT instead of 4), and evaluate the filecache + `filecache_extended` upsert merge. Plan finding 9.

**Decisions**: Postgres-only (SQLite keeps the multi-step path — `propagator.rs:169-171` already branches; no row locks there). The CTE:

```sql
WITH locked AS (
  SELECT fileid FROM {prefix}filecache
  WHERE storage = $1 AND path_hash = ANY($2::text[])
  ORDER BY path_hash FOR UPDATE
)
UPDATE {prefix}filecache fc SET … FROM locked l WHERE fc.fileid = l.fileid
```

**Measure before the upsert merge** — section 19 flags PUT as already the fastest-vs-PHP area (1.3-2.3×); the propagation CTE is the guaranteed win, the upsert merge only if the PUT query count justifies it.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T9.1 | Propagation CTE; perf-gate re-measured (`put_new` drops); `make bench-one SC=10_put_get` improves or holds. |
| S1 | T9.2 | Upsert merge **only after measuring**; milestone suite. |

- [x] **T9.1** The propagation CTE (pre-lock + the size/etag UPDATE branches) on the `Pg` arm; SQLite unchanged; `try_propagate`'s four round trips become one.
- [x] **T9.2** Measure `put_new` statements and PUT latency first; if justified, merge the filecache insert + `filecache_extended` upsert (`filesystem.rs:2289-2292`, `2904-2908`) into one `WITH … RETURNING` statement.

---

## T2 — Pipelining (tokio-postgres + deadpool)

Goal: all batch families on **one connection** with no awaits between sends — one RTT, no pool contention — superseding the `join!` (21.2). Plan finding 3's pipelining note.

**Decisions**: sqlx cannot pipeline (every query takes `&mut conn` and awaits its full round trip); tokio-postgres allows issuing without awaiting. This is a driver swap behind the T3 enum (the `Pg` arm becomes tokio-postgres + deadpool) — the row access layer changes (`PgRow` vs the sqlx row API), so it is the largest refactor in the plan; do it only after T3-T7 land and the batch surface is stable. Verification is the bench against the 21.2 numbers (SC=14 p50 must improve beyond 2.09/1.69), not the gates (statement counts unchanged).

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T2.1 | Driver swap behind the enum; everything green (perf-gate, unit tests). |
| S1 | T2.2 | Pipelined batch: issue the families on one connection without awaiting between sends; bench SC=14 beats the join! numbers; milestone suite. |

- [ ] **T2.1** The `Pg` arm backed by tokio-postgres + deadpool (binary protocol, prepared-statement caching via the connection's own statement cache); keep the sqlx `Pg` arm only if the swap proves worse.
- [ ] **T2.2** The batch pipeline: one connection, send all families, then drain; retire `tokio::join!` if the benchmark confirms.

---

## T8 — Cleanup

Goal: the remaining low-value trims — build SQL once, single-lock the batch, hoist the leftover mime lookups. Plan findings 7-8.

**Decisions**: these are alloc/lock trims, not latency wins — each independently verified with perf-gate + unit tests + flat bench. The `Arc<Mutex>` per-map layout is *required* by dav-server-rs's filesystem cloning (`filesystem.rs:102-105`) — `RefCell` is impossible; the valid consolidation is one `Mutex<PropfindBatchInner>`. The mime-id call sites are cache hits post-warmup (`main.rs:63` warms); hoisting the remaining write-path sites is opportunistic. `OnceLock<Queries>` only pays off after T4 (statement texts are stable then).

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T8.1-T8.3 | Unit tests green; perf-gate holds; bench flat. |

- [x] **T8.1** `OnceLock<Queries>` of the fixed SQL strings (after T4's texts stabilize).
- [x] **T8.2** `PropfindBatch` → one `Mutex<PropfindBatchInner>` (9 locks → 1) keeping the `Arc` sharing.
- [x] **T8.3** Hoist the remaining `get_or_insert_mime_id("httpd/unix-directory"|"httpd")` call sites (`create_dir`, `versions.rs`, `archive.rs`, `handler.rs`) onto the `AppState` ids from 21.4.

---

## T10 — Index audit

Goal: verify the index claims against the live DB — verification only. Plan finding 10.

**Decisions**: the schema is Doctrine-owned — **adding** an index is a divergence needing an explicit `improvements.md` decision, never a silent migration. The phase-21 audit already found: `properties_path_index(userid, propertypath)` ✓, `file_source_index(file_source)` ✓, `comments_object_index(object_type, object_id, creation_timestamp)` ✓ (superset), `oc_filecache(parent, storage)` **does not exist** (only `fs_parent(parent)` + `fs_parent_name_hash(parent, name)`), systemtag mapping has two single-column indexes, and `comments_marker_object_index(object_type, object_id)` lacks a `user_id` leading column — relevant after T6.4's `LEFT JOIN`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | T10.1 | `EXPLAIN` on the hot queries (depth-1 batch, after T7; the unread join after T6.4) against the live DB; the decision record (no index changes without the `improvements.md` entry). |

- [x] **T10.1** `EXPLAIN` the current batch + unread-join plans on the live stack; record which claimed indexes matter, which are missing, and whether the planner uses them; write the `improvements.md` decision for anything that needs adding (or explicitly defer).

---

## 22.1 — Depth-0 round-trip join

Goal: collapse the depth-0 root's serial single-row statement chain in `get_props` (~10 statements for a full allprop root PROPFIND) into ~2 rounds with one `tokio::join!` — the same trick T1 applied to `read_dir`'s families. The root is visited by dav-server-rs **before** `read_dir`, so it is never in the batch: every batch miss falls back to a single-row query, and `get_props` awaits them strictly sequentially (`filesystem.rs:2835-3210`). This is the round-trip mass of the current design (item (c) of the T2-strike analysis) — the depth-1 delta is 3 statements, depth-0 is 11.

**Decisions (grounded):**

- **Only `load_meta` gates the rest.** After the lookup yields `meta.fileid`, `count_children`, `get_share_note`, `get_tag_info`, `get_share_details` (+ its internal display-name chain, unchanged), `get_comments_count`/`get_comments_unread` (join the pair), `get_system_tags_for_file`, `list_custom_properties` and `cached_user_state` (cache hit in steady state — joined so cold requests pay once) depend only on the fileid/uid/path known up front. One `tokio::join!` collapses ~10 serial RTTs into ~1; the pool (≥16 connections) holds them concurrently.
- **Behavior-neutral.** Same statements, same results, same HTTP bytes — only scheduling changes; the A/B harness is the parity gate.
- **In-batch nodes (depth-1 children) keep the batch-hit path.** Each joined future keeps its `batch_contains`/`batch_get` check, so in-batch `get_props` still issues no statements.
- **Statement counts are unchanged** → the perf-gate budgets hold (the gate measures counts, not RTTs); the win is latency, verified with `nc-bench` on a depth-0 PROPFIND (SUT vs oracle) before/after.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.1 | `cargo test --lib`; `make diff-test`; `make perf-gate` (counts identical); depth-0 bench p50 drops vs baseline. |

- [x] **22.1** Restructure `get_props`'s post-`load_meta` query blocks into one `tokio::join!` (batch-check + single-row fallback per future, pure-Rust computation stays in its existing order; the comments pair joins internally).

## 22.2 — Tag-prefetch fold into the CTE

Goal: fold `prefetch_tags`'s `oc_vcategory` / `oc_vcategory_to_object` scan into the PROPFIND CTE as a `LATERAL` sub-select — same shape as the system-tags sub-select already in it (`objid = fileid`). Depth-1 statement delta 3 → 2 (item (b) of the T2-strike analysis).

**Decisions (grounded):**

- **RTT-neutral, not a latency win.** The prefetch already runs *concurrently* with `custom_properties_batch` in `read_dir`'s second-round join, and `custom_properties_batch` cannot fold (T7.2 — the property-path hash is Rust-side). So the two serial rounds stay; what disappears is one of the two second-round statements: fewer statements (budget drop), one fewer connection slot, one less parse/plan/marshal.
- **The "a different shape" guard (`filesystem.rs:1819`) does not hold up** — `oc_vcategory_to_object` is keyed on `objid = fileid`, a textbook `LATERAL` sub-select.
- The prefetch also covers the **directory's own** fileid (the `prefetch_ids` list is `child_ids + dir_fileid`) — the sub-select must include the parent row.
- SQLite keeps the batch path behind the variant.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.2.1 | `cargo test --lib` (SQLite path); `make diff-test`; perf-gate re-measured with `scaling_delta_budget` 3 → 2. |

- [x] **22.2.1** The `LATERAL` sub-select in `propfind_batch_cte` (Postgres arm) + Rust-side decode into the same tag shape `prefetch_tags` produces; the directory's own tags included.
- [x] **22.2.2** Drop the `prefetch_tags` call from `read_dir`'s join on the Postgres path (SQLite keeps it); re-measure and lower the delta budget.

---

## 22.3 — CTE gating probe in the harness

Goal: make the S2 gate's EXPLAIN assertion permanent — a harness probe that proves, on the deployed stack, that a narrow-prop depth-1 PROPFIND leaves the skipped CTE families unexecuted. Closes the blind spot the 22.2-C work exposed: any future edit to the CTE SQL (or any other PG-only statement) is currently validated only by the live stack by hand — SQLite tests and compilation are blind to it by construction (the vcategory `bigint` type bug was caught only by a manual `EXPLAIN`).

**Decisions (grounded):**

- **Black-box, like the rest of the harness** (plan 20: `nc-bench` links no `nc-*` server crate). The probe must **not** import the CTE SQL from source — it must test the *deployed* statement (a source-derived probe would drift and could pass while the deployed SQL is broken). Design: enable `log_statement='all'` (the perf-gate's existing plumbing), fire a depth-1 PROPFIND with the desktop client's narrow prop body, harvest the CTE statement text from the PG log, then `EXPLAIN (ANALYZE)` it with the flag binds set to the narrow request's values (all false except tags) and assert the skipped SubPlans are absent or `never executed` while the tags SubPlan executes. Run a second EXPLAIN with all flags true as the sanity direction.
- **Also covers the runtime-type-bug class**: a wrong cast in the CTE (the `::text`-vs-bigint class) makes the EXPLAIN fail outright — the probe is the permanent tripwire for it.
- The narrow body is the desktop client's fixed set (`d:getetag`/`getlastmodified`/`getcontentlength` + `oc:id`/`permissions`/`size`/`favorite` — no share-types, comments, system-tags, contained-*-count, custom props); a fixed replay, not a recorded capture.
- The harness already owns the superuser DSN and the `log_statement` toggle (budget.rs) — the probe reuses both.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.3.1, 22.3.2 | Probe passes on the current stack (narrow PROPFIND → skipped SubPlans unexecuted); non-zero exit on any violation; milestone procedure runs it. |

- [ ] **22.3.1** The probe in `nc-bench` (a `gates` subcommand or equivalent): narrow-prop PROPFIND → harvest the CTE text from the PG log → `EXPLAIN (ANALYZE)` with the narrow flag binds → assert no skipped-family SubPlan executes and the tags SubPlan does; second EXPLAIN all-true as sanity. Non-zero exit on violation.
- [ ] **22.3.2** Expose it as a make target (`make gate-probe` or fold into `make perf-gate` — decide in the task) and add it to the milestone procedure; verify it passes against the current stack.

---

## 22.4 — Pg-backed CTE decode test (gates-off shape)

Goal: pin the CTE decode's NULL handling so the 22.2-C gates can never panic again — the `UnexpectedNullError` class (the production root-PROPFIND panic, 2026-08-14).

**Decisions:** the decode is inline in `propfind_batch_cte` and Postgres-only — no unit test executes it today (the SQLite arm returns early), which is why the `dir_counts`/`comments` panics shipped. Extract the per-row decode into a testable helper that takes the raw column values; unit-test both the gated-off (`None`) and populated shapes for all six sub-selects. Review rule applied at the same time: any column that can be NULL (CASE-gated, LEFT JOIN, optional) decodes via `try_get::<Option<…>>` — `r.get()` panics on NULL.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.4.1 | `cargo test --lib` green including the new decode tests; a regression test that fails against the pre-fix non-Option decode |

- [ ] **22.4.1** Extract the per-row decode from `propfind_batch_cte` into a testable function; unit tests for each sub-select's gated-off (NULL) and populated shapes; audit the remaining `r.get()` decode sites.

## 22.5 — Narrow-prop scenarios in the diff suite

Goal: exercise the gates-off path — the desktop client's explicit `<d:prop>` body — which the allprop-only suite never sends, leaving the gated sub-selects (and their NULL decodes) untested end to end.

**Decisions:** new scenarios replaying a narrow prop body (the desktop set: `d:getetag`/`getlastmodified`/`getcontentlength`/`resourcetype` + `oc:id`/`permissions`/`size`/`favorite`/`share-types` + `nc:system-tags` — no `contained-*-count`, no comments). Same body to both sides; the 22.6 cardinality assertion applies. The 22.3 gating probe hooks the same shape.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.5.1 | New scenario(s) green on the fixed binary; `make diff-suite` green |

- [x] **22.5.1** The narrow-prop scenario(s): depth-1 on a directory with shares/tags/comments, plus an allprop control.

## 22.6 — Response-content assertions for propfind scenarios

Goal: the propfind scenarios must fail when the SUT's listing is empty or truncated — delta-comparison alone is blind to response correctness (a panicked SUT request writes no deltas and passes, as the 2026-08-14 milestone demonstrated).

**Decisions:** compare structural invariants between SUT and oracle responses — the `<d:response>` cardinality and the set of `d:href`s — not bytes (the XML shapes differ in known ways). Cheap, and it catches the empty/truncated-listing class.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.6.1 | Suite green with the assertion; a forced empty listing fails the scenario |

- [x] **22.6.1** The cardinality/href-set assertion in the propfind scenarios' replay comparison.

## 22.7 — "No PG ERRORs" milestone assertion

Goal: the milestone procedure must fail if the SUT's Postgres log window contains ERROR lines — the desync class (44 errors during the 2026-08-14 milestone) passed every gate.

**Decisions:** capture the SUT PG error count before/after the suite run (the perf-gate already drives the superuser DSN); a non-zero delta → fail. Fold into `make diff-suite` or `make perf-gate`.

| Stop | Tasks | Gate |
|---|---|---|
| S0 | 22.7.1 | Suite green with the assertion; a forced PG error fails the run |

- [ ] **22.7.1** The error-count assertion in the milestone procedure.

---

## Deviations from the task descriptions

- **T7.2** — `custom_properties_batch` is NOT folded into the CTE (it stays a separate gated statement). The `>250`-char property-path hash (`format_property_path`) is Rust-side, and the children's names — needed to build those paths — only exist after the query returns, so a CTE sub-select would need pgcrypto (not guaranteed) or an `unnest` indirection over paths built from a second query. One statement either way; the CTE still collapses 5 families → 1.
- **T3.3** — "builds without the `any` feature" is not achievable within T3: the `Executor` delegation and the `string_to_array` SQL are Any-typed by construction (a single `DbPool` type serving both backends forces `Database = Any` at unmigrated call sites — verified against sqlx 0.8.6's `Executor`/`IntoArguments`/`AnyConnection` APIs). T3 sheds the Any *machinery* (AnyPool/AnyPoolOptions/`install_default_drivers`, the driver registry, the global `backend_is_postgres` latch) and builds native pools; the `any` cargo feature + the array-interim SQL stay until T4/T7 migrate the call sites to per-variant native queries, at which point the feature can be dropped for real.
- **T6.6** — the task scopes prop-set gating to `read_dir`; the `{oc:}favorite`/`{oc:}tags` family additionally gates `get_props`'s `get_tag_info` call. Without it, skipping the prefetch turns one batch query into one query per child (the N+1 the batch exists to prevent) — the task's own goal would fail. Same predicate on both sides; behavior-neutral (PropWriter's 12.1 filter drops the props either way).
- **T6.6 (post-T7) — the family gates are CTE bind flags on Postgres, and the S2 gate was unfalsifiable until 22.2-C.** T7 folded the five families into one statement, so `want_dir_counts`/`want_shares`/`want_comments`/`want_system_tags` were computed but consumed only by the SQLite branch — dead on the first-class target — and the S2 "absent from the statement log" replay passed trivially (they aren't statements any more). Fixed with the 22.2-C measurement and resolution: measured on the live stack (200-child dir), the four gateable sub-selects were ~1.0 ms of the CTE's 1.42 ms execution (~73%) — above the 10% threshold, so C was chosen: each sub-select is wrapped in `CASE WHEN $N THEN … END` with the gates as bind parameters (one statement text, `cached_sql` unchanged). Verified on the live stack: flags-false runs eliminate the SubPlans at plan time on the custom plan (0.097 ms) and show `never executed` on every skipped SubPlan under the generic plan (0.152 ms vs 1.277 ms all-on); the dir-counts subplan runs only for directory children (`loops=40` of 200 — T6.2's dir-child filter restored on Postgres via `k.mimetype = $3`). T6.6's scope now covers Postgres via the flags plus SQLite via the batch gates; only `custom_properties_batch` remains a standalone gated statement.

## Changes

- 2026-08-14: **Pipelining is definitively dead for the target profile (phase-23.9 record).** On 2-core/HDD with localhost Postgres, a round trip is ~0.05 ms of one core while CPU and platter I/O are the scarce resources — pipelining optimizes the abundant one. The supersession argument (T2's entry below) already kills it architecturally; this profile verdict closes the door operationally: the 23.6 seek-overlap and 23.3 I/O semaphore are where the same engineering effort pays.
- 2026-08-14: **Two post-milestone bugs found and fixed (both invisible to the milestone gates).** (1) `cached_sql` (T8.1) was keyed by the table prefix alone while four statements share it — the first caller per process won and the others executed its SQL with their own binds, producing intermittent `supplies N parameters, requires M` / `bigint = text[]` desync errors and a `ColumnNotFound` panic in the accounts fallback. Found by the 23.1 CPU measurement's statement-log forensics; fixed by keying on `(prefix, build)`. (2) 22.2-C's CASE gates make `dir_counts`/`comments` NULL when the client did not request them, but both decodes used non-Option `r.get()` → `UnexpectedNullError` panics on any listing with directories under a narrow prop set — reported from the production deploy (root PROPFIND panic, `row.rs:1270`). Fixed via `try_get::<Option<Value>>`. **Milestone-validity caveat:** the 2026-08-14 milestone (suite 20/20, perf-gate green) passed while the SUT was producing the desync errors — the suite compares DB deltas, and a panicked/errored SUT request produces no deltas, so propfind scenarios were blind to response correctness; the perf-gate counts statements regardless of success. The suite must be re-run on the fixed binary, and the harness gaps (Pg-backed decode tests, narrow-prop scenarios, response-content assertions) are tracked for follow-up.
- 2026-08-14: **Third PG-only bug — the bearer token lookup 401'd every request on Postgres** (structured audit of the T3.3 native-decode class; live-verified against the oracle). `lookup_bearer`'s tuple decoded `scope` (nullable text) as non-Option `String` and `expires`/`last_activity` (INT4 in the schema) into `i64` — sqlx's strict native decode rejects INT4→i64 ("Rust type i64 (as SQL type INT8) is not compatible with SQL type INT4") and NULL→String ("unexpected null"). Result: any `Authorization: Bearer` request → 401 on the SUT while PHP returns 200 — every real token row carries a non-NULL `last_activity`, so the failure rate was 100%. Invisible to every gate: SQLite's dynamic typing accepts the decodes, the harness authenticates via Basic (token as password — `basic.rs` selects different columns), and the perf-gate counts statements, not errors. `basic.rs` carried the same latent class (`expires` INT4 into `Option<i64>` — fails only when a token has a non-NULL expiry). Fix: SQL-side normalization to PHP's `PublicKeyToken` semantics with exact-type casts — `COALESCE(type, 0)::smallint`, `COALESCE(scope, '')`, `expires::bigint`, `COALESCE(last_activity, 0)::bigint` (the `0` literal promotes smallint→int4, so the `::smallint` re-cast is load-bearing — found by the first live test round). Verified live: SUT Bearer 200 == PHP 200 (was 401 vs 200); Basic with a non-NULL expires 200; difftest scenario 01 IDENTICAL; 60 nc-auth tests green. The audit also cleared the rest of the decode surface (CTE json decodes Option'd, `FileCacheRow`/`FileCacheExtRow` Option'd, smallint→i16, INT4→i32, COUNT→i64, `fileid::text` comparisons against the varchar `object_id` columns correct, `::json` cast SQLite-valid, no Any-driver remnants).
- 2026-08-14: **22.5 + 22.6 done — `04_propfind_narrow` exercises the gates-off CTE path; propfind replays now assert the response shape.** `Op::Propfind` gained an explicit `<d:prop>` body (the desktop client's narrow set — no contained-*-count, no comments), and the runner extracts a `PropfindShape` (response cardinality + href set, prefix-agnostic) from every propfind response; the comparison loop asserts shape equality across sides — the tripwire for the truncated/empty-listing class the delta diff is blind to (the 22.2-C panics wrote no deltas and passed the milestone). Scenario setup: proppatch favorite + group share on the skeleton `/Media` (no native write ops — an initial PUT-setup version raced the preview drain: the setup file queued preview generation whose rows landed in the oracle's window but not the SUT's, the same class scenarios 15/30 avoid). Parser bug found en route: `</d:href>` closing tags matched the href element and grabbed the NEXT element's content — double-counted responses and produced garbage hrefs (the assertion immediately caught it); fixed with a closing-tag skip, pinned by 3 new unit tests (18 lib tests total). Verified: 04 stable 3×, existing propfind/write scenarios (01/03/10/14/30) pass with the assertion active; the failure path was demonstrated live by the broken parser (SHAPE MISMATCH → divergence).
- 2026-08-14: **Harness gap closed — `03_bearer_auth` scenario exercises the Bearer auth path end-to-end.** The harness previously authenticated only via Basic (token as password), which is how the decode bug above escaped every gate. The scenario: `auth: bearer` (new scenario-level field) → the runner creates a per-side v2 app token (new `nc_difftest::auth::ensure_bearer_token`, the nc-bench insert machinery ported) BEFORE the before-snapshot, and the client (new `AuthMode::Bearer`) sends `Authorization: Bearer` instead of Basic. Read-only depth-1 PROPFIND: a broken bearer path yields `STATUS MISMATCH: SUT 401 vs oracle 207`. Tripwire proven live: with the pre-fix binary the scenario fails with exactly that mismatch; on the fixed binary it is IDENTICAL (stable across 3 runs; the existing 01/10 scenarios still pass). Two harness findings en route: (1) `oc_authtoken.last_activity` is written one-sidedly during a token-auth scenario (PHP's `updateTokenActivity` synchronously; Rust's `spawn_last_activity_update` async, landing after the snapshot) — a wall-clock bookkeeping value, so `oc_authtoken` joined the `db.rs` SKIP_TABLES set (same class as `oc_preferences`); the status comparison is the auth parity surface. (2) The initial registry attempt (`volatile_value`, then `ignore`) still diverged — the one-sided write registers as a row change-type mismatch, which masking cannot suppress; only skipping the table works.
- 2026-08-14: **22.2 implemented** — superseded on the gating question by 22.2-C (below): the tag sub-selects are `CASE`-gated like the other four families, not always-on. Empty-directory edge: no CTE rows → `dir_tags` absent → read_dir skips the dir cache insert (the target's `get_props` already cached it). Verified: compiles, 311 + 38 lib tests green; milestone suite 20/20 and perf-gate re-measure green (see the 22.2-C entry for the tightened budgets).
- 2026-08-14: **22.2-C — the CTE families are bind-gated (`CASE WHEN $N THEN …`), per the live measurement.** The T6.6-on-Postgres analysis (T7 nullified the family gates) was verified, then decided by the prescribed experiment: `EXPLAIN (ANALYZE, BUFFERS)` of the CTE on a live 200-child directory (12 shared, 8 tagged, 3 commented) showed the four gateable sub-selects ≈ 1.0 ms of 1.42 ms execution (~73%) — above the 10% rule, so option C over B. Implementation: `PropfindGates` struct; each of the six sub-selects (dir counts, shares, comments, system tags, tags, dir tags) wrapped in `CASE WHEN $N THEN … END` — one stable statement text, `cached_sql` key unchanged, 9 binds. The dir-counts gate carries `AND k.mimetype = $3`, restoring T6.2's dir-child filter on Postgres. Verified live: custom plan with flags-false eliminates the SubPlans at plan time (0.097 ms vs 1.277 ms all-on); the steady-state generic plan shows `never executed` on every skipped SubPlan (0.152 ms). Also caught and fixed en route: the 22.2 tag sub-select originally compared `vco.objid = k.fileid::text`, but `oc_vcategory_to_object.objid` is **bigint** — a runtime type error the SQLite tests and compilation can't see; the live `EXPLAIN` surfaced it. **Milestone (fresh `down -v` install, full suite + gate):** suite 20/20 — 18 passed first pass; 01 (fresh-install first-access: SUT's lazy `cache/` + storage-root bump fired inside the first run's window) and 15 (one-sided vcategory rows) both re-ran IDENTICAL, the documented first-run artifact class; nothing added to `divergences.yaml`. Perf-gate green and budgets tightened to the measured counts: `propfind_depth1` 12, `scaling_delta_budget` 1 (the delta is 1, not 2 — the depth-1 root's load_meta is a batch hit, saving one depth-0 statement); `put_new` 15 (one transient 17 in a counting window, 15 on 2 of 3 runs). The suite loop is now `make diff-suite` (`scripts/run-difftest-suite.sh`).
- 2026-08-14: **22.1 implemented** — verified: workspace compiles, 311 nc-dav + 38 nc-db lib tests green; milestone suite 20/20 and perf-gate green (11/12/15, delta 1). Found during the restructure: the custom-props tail re-queried (its `let custom_props` shadowed the joined value); it now consumes the joined rows. (No standalone depth-0 bench scenario exists — the bench reuses the difftest corpus; the depth-0 win is structural: ~11 serial statements → ~2 rounds.)
- 2026-08-14: **T3.3 completed** — the deviation's deferred half (migrate the remaining Any-typed call sites, then drop the `any` feature) is done; verified with 0 compile errors on the feature-less tree and 546 lib tests green (Postgres parity = next milestone difftest run). Also deleted (operator decision): the dead `batch_comments_counts`/`batch_comments_unread` helpers — superseded by T6.3's merged query, zero callers.
- 2026-08-14: **T2 struck — T7 removed its premise** (operator decision; the task body stays verbatim with the checkboxes open — this entry is the record). T2's goal was "all batch families on one connection with no awaits between sends — one RTT, superseding the `join!`". After T7 there are no ~8 independent families left to pipeline — there is **one CTE statement**. The Postgres depth-1 delta is 3 statements (`scaling_delta_budget: 3`, `perf-budget.yaml:43`): (1) `propfind_batch_cte`, (2) `custom_properties_batch`, (3) `prefetch_tags` — where 2 and 3 are **already concurrent** in one `tokio::join!` (`filesystem.rs:1651`). Critically, 2 and 3 are **data-dependent on 1**: `child_paths` comes from `metas`, which comes from the CTE's `children` (`filesystem.rs:1454-1495`); `prefetch_ids` comes from `child_ids`. Pipelining removes *scheduling* serialization, not *data* dependencies — you cannot send a query whose parameters do not exist yet — so T2's ceiling on the depth-1 path is ~zero. Its only residual win is "two queries on two pooled connections concurrently" vs "pipelined on one connection" — one connection's worth of pool contention at 2 statements, against the plan's largest refactor (the row-API change). Not a trade worth making. T2 would also actively fight the current architecture: swapping the Pg arm to tokio-postgres breaks the `Executor<'_, Any>` delegation every unmigrated call site still depends on (the `PgRow`→`AnyRow` translation at `pool.rs:map_pg_step`), forcing a full migration at once. **Where the residual value moved:**
  - **(a) Finish T3.3 — the real remaining driver cost.** The hot batch queries are native (`sqlx::query::<Postgres>` at `row.rs:415-422` and 9 more sites), but every *unmigrated* call site still runs through the Any delegation and pays the row boxing — that's `propfind_depth0` (11 statements), `put_new` (15), auth, appconfig. **The depth-1 hot path is native; everything else isn't.** Cheaper than T2 and it closes the T3.3 loop (the `any` cargo feature can finally be dropped for real).
  - **(b) Fold the tag prefetch into the CTE.** T7.2's deviation only justified excluding `custom_properties_batch` (the Rust-side path hash). `oc_vcategory_to_object` is keyed on `objid = fileid` — a textbook `LATERAL` sub-select, same shape as the system-tags one already in the CTE; the in-code reason ("the prefetch covers oc_vcategory, a different shape", `filesystem.rs:1655-1657`) does not hold up. Delta 3 → 2, and it eliminates one of the two data-dependent rounds — more than T2 would deliver, for a fraction of the work.
  - **(c) Depth-0 is now the round-trip mass, not depth-1** (11 statements vs a delta of 3). If any subset of those is mutually independent, it's the same `join!` trick that already worked — no driver swap needed. Same question for `put_new`'s 15.
  - If something like T2 comes back later it should be a **new task** with an honest goal (pipelining the *depth-0* / write-path statement sets), not this one.
- 2026-08-13: **T8.1 scope note:** the cached SQL set is the per-request PG-path texts (the depth-1 CTE, `custom_properties_batch`, the two display-name lookups) — the other batch statements (counts, comments, system tags, share scan, `list_extended_batch`, `lookup_by_ids`) run only on the SQLite path / write path post-T7, where their texts vary by list size or aren't per-request hot; caching them would add churn for sub-μs wins. T8.2 collapses the nine per-map mutexes into one `Mutex<PropfindBatchInner>` (all maps were touched one at a time; every read clones out before any await). T8.3 hoists the `httpd`/`httpd/unix-directory` lookups in `create_dir`, `rename`, COPY's mimetype recompute, `store_version`/`insert_version_entity` (threaded through `WriteCtx`), `try_serve_archive`, and the DELETE-directory check onto the 21.4 AppState ids.
- 2026-08-13: **T9.2 decision (measured, not merged):** `put_new` after T9.1 = 15 statements; the filecache INSERT + extended upsert merge would save 1 (6%) and change the extended-upsert failure from warn-only to fatal (the merged statement fails atomically). PUT is already the fastest-vs-PHP area (SC=10: 46.9 ms p50, 2.16× vs PHP — improved from the 55 ms / 2.3× baseline). Not justified per the task's own condition; the separate upsert stays.
- 2026-08-13: T6 landed (merges, de-correlation, prop-set plumbing + gating). **Divergence candidate (unchanged, needs an explicit decision):** PHP's `getNumberOfUnreadCommentsForObjects` has no `actor_type`/`actor_id` filter (`Manager.php:673-689`) — a user's own comment newer than the marker counts as unread in PHP; Rust's `get_comments_unread` excludes it since phase 12.6, and no difftest scenario exercises comments, so it was never A/B'd.
- 2026-08-13: Phase created as the combined doc for the plan's remaining items T6-T10 (the amended execution order after Tier 1; plan section 21). T6 grounding: `PropWriter.requested` already exists (`handle_props.rs:233-236`); harness + perf-gate send allprop, so budgets drop only from the merges (delta 9 → 7, propfind_depth1 20 → 18); the share-pair filters differ and split in Rust; the unread-marker de-correlation is its own task. T3-T10 grounding per plan findings 3-10; T2 is the largest refactor (row-API change) and intentionally last among the structural items.
