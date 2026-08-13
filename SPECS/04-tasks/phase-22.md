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
| S2 — Filtering | T6.5, T6.6 | `cargo test --lib`; perf-gate holds (allprop unchanged); desktop-client prop-set replay shows unrequested families absent from the statement log. |

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

- [ ] **22.2.1** The `LATERAL` sub-select in `propfind_batch_cte` (Postgres arm) + Rust-side decode into the same tag shape `prefetch_tags` produces; the directory's own tags included.
- [ ] **22.2.2** Drop the `prefetch_tags` call from `read_dir`'s join on the Postgres path (SQLite keeps it); re-measure and lower the delta budget.

---

## Deviations from the task descriptions

- **T7.2** — `custom_properties_batch` is NOT folded into the CTE (it stays a separate gated statement). The `>250`-char property-path hash (`format_property_path`) is Rust-side, and the children's names — needed to build those paths — only exist after the query returns, so a CTE sub-select would need pgcrypto (not guaranteed) or an `unnest` indirection over paths built from a second query. One statement either way; the CTE still collapses 5 families → 1.
- **T3.3** — "builds without the `any` feature" is not achievable within T3: the `Executor` delegation and the `string_to_array` SQL are Any-typed by construction (a single `DbPool` type serving both backends forces `Database = Any` at unmigrated call sites — verified against sqlx 0.8.6's `Executor`/`IntoArguments`/`AnyConnection` APIs). T3 sheds the Any *machinery* (AnyPool/AnyPoolOptions/`install_default_drivers`, the driver registry, the global `backend_is_postgres` latch) and builds native pools; the `any` cargo feature + the array-interim SQL stay until T4/T7 migrate the call sites to per-variant native queries, at which point the feature can be dropped for real.
- **T6.6** — the task scopes prop-set gating to `read_dir`; the `{oc:}favorite`/`{oc:}tags` family additionally gates `get_props`'s `get_tag_info` call. Without it, skipping the prefetch turns one batch query into one query per child (the N+1 the batch exists to prevent) — the task's own goal would fail. Same predicate on both sides; behavior-neutral (PropWriter's 12.1 filter drops the props either way).

## Changes

- 2026-08-14: **22.1 implemented** — verified: workspace compiles, 311 nc-dav + 38 nc-db lib tests green; milestone suite + perf-gate (counts must hold) + a depth-0 bench pending. Found during the restructure: the custom-props tail re-queried (its `let custom_props` shadowed the joined value); it now consumes the joined rows.
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
