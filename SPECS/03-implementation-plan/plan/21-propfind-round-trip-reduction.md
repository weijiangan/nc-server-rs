# 21. DAV read-path round-trip reduction

## Context

Phase 18.1 batched depth-1 PROPFIND from ~11 queries per child to a fixed set
of batch families per `read_dir` (section 19, Task 6). The remaining cost is
no longer query **count** — it is **serialization**: `read_dir` awaits its
families strictly sequentially, each `.await` completing before the next
starts, so one depth-1 PROPFIND pays 9-10 network round trips for work that is
mutually independent. Nothing after the child listing consumes another
family's output; each batch result goes into a disjoint map of the per-request
`PropfindBatch` (`nc-dav/src/filesystem.rs:111-134`), so ordering is not
semantically required.

This section turns the read-path review into ten tasks (T1-T10) grouped into
four tiers by impact-per-effort — the cheap round-trip removals first, then
doing less work per request, then the structural driver/statement changes, then
verification — with a dependency-correct execution order (T4 needs T3's native
pool before it can bind a real array; T7 and T2 need T3 as well). The
regression guardrail this work needs already exists — the statement-budget gate
(section 20) — so no new counter task is proposed here.

Line references are against the code state at this section's creation; they
may drift as the tasks land.

## Tasks by tier

| Tier | Tasks | Focus |
|---|---|---|
| **Tier 1 — Cheap, low-risk, immediate round-trip removal** | T5 · T1 · T4 · T8 | remove the ping-on-every-acquire overhead; collapse the serial RTTs into one; make statement text stable so the prepared-statement cache stops churning; hoist lookups that are already cache-warm |
| **Tier 2 — Do less work per request** | T6 | honour the requested prop set (skip families whose props were not asked for); merge the families that scan the same table twice |
| **Tier 3 — Structural** | T3 · T7 · T9 · T2 | native `PgPool`; one-statement child fan-out; one-statement write-path propagation; pipelining |
| **Tier 4 — Verification / guardrails** | T10 · (guardrail exists) | index audit; query-count gate — already implemented as `make perf-gate` (section 20) |

## Execution order

The tiers group by risk and leverage; execution follows the dependency-correct
order: **T5 → T1 → T6 → T3 → T4 → T7 → T9 → T2 → T8 → T10**. The task
sections below are grouped by tier (the review's natural flow); this order is
what execution actually follows.

1. **T5 — pool flags** — one-line change, ~9 pings per PROPFIND disappear.
2. **T1 — `join!` the independent families** — same statements, same bytes;
   collapses ~8 RTTs into ~1. A/B stays green.
3. **T6 — prop-set filtering + family merges** — the first tier that
   **removes statements**; the perf-budgets get lowered, not just held.
4. **T3 — native `PgPool` enum** — the prerequisite for T4, T7, T2.
5. **T4 — `= ANY($1::…)` native binds** — stable statement text, no cache
   churn, no per-call `format!`.
6. **T7 — single-query CTE PROPFIND** — one statement for the whole child
   set; the scaling delta drops to ≈ 1.
7. **T9 — write path** — propagation in one CTE statement, 1 RTT instead
   of 4 per PUT.
8. **T2 — pipelining** — the optional one-connection fan-out that supersedes
   the `join!`; only after T3-T7 land and the batch surface is stable.
9. **T8 — cleanup** — SQL-once, single-lock batch, remaining static hoists.
10. **T10 — index audit** — verification only, against the live DB.

---

## Tier 1 — Cheap, low-risk, immediate round-trip removal

### T5 — Pool flags (`nc-db/src/pool.rs`)

`build_pool` (`pool.rs:16-33`) builds an `AnyPoolOptions` chain (`:22-27`) with
`min_connections(5)` / `max_connections(50)` (`:19-20`). Two changes:

- **`.test_before_acquire(false)`.** sqlx's default is `true`: every acquire
  of an idle connection pings it (`sqlx-core-0.8.6/src/pool/options.rs:149`,
  `pool/inner.rs:469-476`) — on Postgres a full write-flush +
  `wait_until_ready` round trip (`sqlx-postgres-0.8.6/src/connection/mod.rs:176-185`).
  With ~9 sequential `fetch_*(pool)` calls per PROPFIND that is ~9
  pure-overhead RTTs, and they stay serial in front of T1's `join!`. A dead
  connection is detected on first use and discarded; `max_lifetime` /
  `idle_timeout` prune idles — the ping adds nothing for a server pool.
- **`max_connections` 50 → machine-sized.** A Rust server can actually
  saturate Postgres; 50 backends thrash under concurrency. Size to the host:
  `4 × physical cores` (hyperthreads excluded — they add no DB-saturation
  capacity), clamped to [16, 64]. Do not lower `min_connections(5)`.

Also add a **cached dialect check** — `backend_is_postgres()`, a `OnceLock`
set once from `config.dbtype` in `build_pool`, default `false` (tests build
their own SQLite pools). T4's dialect branches key on it, computed once per
process per CLAUDE.md principle 6, never per query. T3 subsumes it with the
enum variant.

**Gate:** `cargo test --lib`; `make diff-test`; `make perf-gate` (counts
identical — pings are not logged statements, so the perf-gate cannot see this
stop; the latency bench is its gate); bench SC=14 (depth-1 PROPFIND, SUT vs
oracle) recorded before/after in `docs/benchmarks.md`.

### T1 — `tokio::join!` the 8 independent batch families (`nc-dav/src/filesystem.rs:1377-1547`)

`read_dir` awaits, in order: `list_children_with_ext` (`:1377`) →
`prefetch_tags` (`:1392`) → dir mime id (`:1460`) → `count_children_batch`
(`:1467`) → `share_details_batch` (`:1482`) → `share_notes_batch` (`:1494`) →
`comments_counts_batch` (`:1505`) → `comments_unread_batch` (`:1511`) →
`system_tags_batch` (`:1524`) → `custom_properties_batch` (`:1535`). Every
`.await` completes before the next starts: 9-10 serial RTTs for one depth-1
PROPFIND.

After the listing yields the child ids, every other family depends only on
that list — plus the dir mime id, which is a cache hit after startup warmup
(`get_or_insert_mime_id` fast path is an `RwLock` read + map lookup). Wrap
families 2-10 in one `tokio::join!`:

- The batch maps are **disjoint** (`filesystem.rs:111-134`), each with its own
  `Arc<Mutex<…>>`, so concurrent writers cannot deadlock; insertion order is
  preserved because the map-filling code stays after the join.
- `prefetch_tags` takes `&TagCache` (interior mutability) and is join-safe.
- The pool's ≥5 connections (max 50) hold all 8 futures concurrently — ~8
  serial RTTs collapse to ~1 wall-clock RTT.

Zero semantic risk: same statements, same results, same HTTP bytes — only
scheduling changes. The A/B harness must stay green unchanged.

**Gate:** `make diff-test` (byte parity — the semantic gate); `make perf-gate`
(statement counts unchanged: 20/11/16/9, delta 9); bench SC=14 p50 drops vs
the T5 baseline.

### T4 — Stable statement text: `= ANY($1::…)` native binds (`nc-dav/src/row.rs`, `nc-dav/src/propagator.rs`)

Seven batch helpers build `IN ($1, …, $N)` with arity == child count
(`count_children_batch` `:494`, `share_details_batch` `:1097`, `share_notes_batch`
`:601`, `comments_counts_batch` `:1362`, `comments_unread_batch` `:1404`,
`system_tags_batch` `:1599`, `custom_properties_batch` `:744`), plus
`list_extended_batch` (`:404`), `lookup_by_ids` (`:1713`) and the propagator's
pre-lock + UPDATEs (`propagator.rs:175,208,237`). Each distinct N is a
**distinct SQL text** → a distinct prepared statement on every pooled
connection, an extra `Parse`/`Describe` round trip on first use, and LRU
eviction of the statements other directories reuse (sqlx's statement cache is
100 entries — verified `sqlx-postgres-0.8.6/src/options/mod.rs:88`). A
directory with 137 children generates a statement no other directory will
reuse.

Fix: one stable statement text per helper —

- `WHERE fileid = ANY($1::bigint[])` for the id lists;
- `= ANY($1::text[])` for the md5 `path_hash` lists (propagator) and the uid
  lists (share display-name resolution);
- `custom_properties_batch` → `propertypath = ANY($1::text[])` — raw fc paths
  may contain commas, so the string-join forms are unsafe there; the native
  `text[]` bind is the only safe stable form for it.

One bind parameter, no per-call `format!` + placeholder join, no cache churn.

**Requires T3.** The Any driver has no array value kind (`sqlx-core-0.8.6/src/any/value.rs`),
and SQLite has no `ANY(array)` — the SQLite path keeps the `IN` form behind
the dialect check (as the propagator already branches), the `Pg` arm binds a
real array. This is why the execution order runs T3 before T4.

**Gate:** `cargo test --lib` (SQLite `IN` path intact); `make diff-test`;
`make perf-gate`; prepared-statement stability — PROPFINDs across 3+
directories of different child counts must leave `pg_prepared_statements`
bounded (~10) instead of growing with each distinct arity.

### T8 — Hoisted statics & cleanup

The remaining low-value trims — alloc/lock wins, not latency wins; each
independently verified with perf-gate + unit tests + a flat bench:

- **Hoist the static lookups.**
  - `get_or_insert_mime_id("httpd/unix-directory" | "httpd")` is called in
    `read_dir` (`filesystem.rs:1460`), `get_props` (`:2485`), `open`
    (`:285-292`) and ~10 more sites across `filesystem.rs`, `versions.rs`,
    `archive.rs`, `handler.rs`. The in-memory mime cache is loaded once at
    startup (`nc-server/src/main.rs:63`) and the fast path is a cache hit
    post-warmup — hoisting the ids into `AppState` removes the per-call
    `RwLock` read + map lookup, not a DB round trip.
  - `get_storage_string_id` (`row.rs:524`) is called per node on non-home
    storages (`filesystem.rs:2538`). `oc_storages` is tiny and near-static, and
    storage rows exist before any filecache row referencing them — a
    process-wide `numeric_id → string_id` map with **negative entries**
    removes it from the hot path.
- **Build SQL once.** Every `row.rs` helper does
  `format!("… FROM {prefix}filecache …")` per call. Once T4 stabilizes the
  statement texts, materialize the fixed strings in a `OnceLock`-backed cache
  keyed by table prefix (leaked `&'static str`): removes the per-query
  allocation and makes the text pointer-stable. Scope: the per-request
  hot-path statements; the list-sized SQLite texts are not worth caching.
- **Single-lock the batch.** `PropfindBatch` is nine `Arc<Mutex<…>>` fields
  (`filesystem.rs:111-134`). The per-map layout is *required* by dav-server-rs
  — it clones the filesystem per resource, and the clones must share one
  cache, so `RefCell` is impossible (the clone crosses tasks). The valid
  consolidation is one `Mutex<PropfindBatchInner>` (9 locks → 1) keeping the
  `Arc` sharing.

**Gate:** `cargo test --lib`; perf-gate holds; bench SC=14 flat.

## Tier 2 — Do less work per request

### T6 — Honour the requested prop set + merge redundant families

Two independent halves; the merges are the only part the statement gate sees
(the filtering win is invisible to allprop PROPFINDs).

### Prop-set filtering (`vendor/dav-server`, `nc-dav/src/filesystem.rs`)

`get_props(path, do_content)` (`filesystem.rs:2457`) never learns *which*
properties the client asked for, so `read_dir` unconditionally fetches
comments counts, unread counts, system tags, share notes and custom properties
even when the PROPFIND body requested none of them. The desktop client sends a
fixed, narrow prop set. The requested `<d:prop>` element list is parsed in the
**vendored** dav-server's `handle_props.rs` (the `PropWriter` carries the
`(namespace, name)` pairs; `None` for allprop/propname) and never reaches the
filesystem driver. dav-server is vendored (`core-rs/Cargo.toml`,
`[patch.crates-io] dav-server = { path = "vendor/dav-server" }`) — patch the
`Filesystem` trait with a per-request setter (default no-op, so
`voidfs`/`memfs`/`localfs` are untouched); `NcFileSystem` stores the set and
exposes a `prop_requested(ns, name)` helper.

Family → prop mapping (a family is skipped when the requested set is non-empty
and contains none of its props):

| family | props |
|---|---|
| `prefetch_tags` | `{oc:}favorite`, `{oc:}tags` |
| `count_children_batch` | `{nc:}contained-folder-count`, `{nc:}contained-file-count` |
| share scan | `{oc:}share-types`, `{oc:}sharees`, `{nc:}note` |
| comments query | `{oc:}comments-count`, `{oc:}comments-unread` |
| `system_tags_batch` | `{nc:}system-tags` |
| `custom_properties_batch` | any prop outside the known `d:`/`oc:`/`nc:`/`ocs:` namespaces |

Skipping 3-5 of the 9 families is realistic for the desktop client's fixed
prop set. Notes:

- **allprop/empty-body requests are unaffected** — the harness and the
  perf-gate both send bare PROPFINDs (no body → allprop), so the milestone
  suite is unchanged and the budgets drop **only** from the merges; the
  filtering win is verified by replaying the desktop client's actual prop set
  and checking the statement log.
- The `{oc:}favorite`/`{oc:}tags` gate must also guard `get_props`'s
  `get_tag_info` call — skipping only the prefetch turns one batch query into
  one query per child (the N+1 the batch exists to prevent). Same predicate on
  both sides.

### Family merges (`nc-dav/src/row.rs`)

All are independent of the trait change, and each is pinned by the SQLite
batch-vs-single unit tests:

- **`share_details_batch` (`:1080`) + `share_notes_batch` (`:587`)** — two
  `oc_share` scans of the same `file_source` set. The filters differ (details:
  `share_type IN (0,1,3,4,6,7,10,12)` + uid conditions; notes: `note != ''`
  with per-file most-recent `stime`), so **do not blind-merge** — one scan,
  split in Rust: `note != ''` rows feed the notes map, all rows feed
  `share_details`.
- **`comments_counts_batch` (`:1348`) + `comments_unread_batch` (`:1388)`** —
  one `GROUP BY c.object_id` with `COUNT(*)` + `COUNT(*) FILTER (WHERE …)`
  for the unread predicate (`FILTER` is supported by modern SQLite too).
- **`count_children_batch` (`:473`)** — restrict the parent list to directory
  children (read_dir has the children rows in memory; only dirs can have
  counts); `SUM(CASE WHEN mimetype = $1 …)` (`:491-492`) → `count(*) FILTER
  (WHERE mimetype = $1)`.
- **De-correlate the unread-marker subquery** — `comments_unread_batch` runs a
  correlated `comments_read_markers` lookup per comment row (`:1406-1410`);
  rewrite as a `LEFT JOIN` on the marker table (one row per (user, object)) —
  the planner handles it far better at scale. Same change in the single-row
  `get_comments_unread` fallback.

9 families → 5 before any pipelining. The statement gate quantifies the
merges: perf-budget delta 9 → 7, `propfind_depth1` 20 → 18.

**Gate:** `cargo test --lib` (batch-vs-single parity pins every merge);
perf-gate re-measured with the **lowered** budgets; bench SC=14 flat;
desktop-client prop-set replay shows unrequested families absent from the
statement log.

## Tier 3 — Structural

### T3 — Native `PgPool` enum (`nc-db/src/pool.rs`)

`sqlx::AnyPool` (`pool.rs:4`) is driver-erased — every row decodes through
`AnyValue` boxing instead of Postgres binary decode, and native `PgPool`
features (statement-cache tuning, array binding for `= ANY($1::bigint[])`) are
unreachable. This directly contradicts CLAUDE.md principle 6 (Postgres is the
uncompromised first-class target). Replace it with

```rust
pub enum DbPool { Pg(PgPool), Sqlite(SqlitePool) }
```

Decisions:

- **Call sites compile unchanged.** The enum implements sqlx's `Executor` for
  `Database = Any` by delegation (translating arguments to the native dialect,
  executing on the inner pool, mapping native rows back to `AnyRow`), so the
  refactor is the enum + delegation impls, not a sweep. `begin()` becomes an
  inherent method returning a `DbTxn` enum (the propagator's call sites).
- **Dialect checks become the variant.** T5's `backend_is_postgres()` latch and
  the propagator's `tx.backend_name()` checks become `DbPool::is_postgres()` /
  the `DbTxn` variant.
- **Shed the Any machinery.** `AnyPool`/`AnyPoolOptions`, the driver registry,
  `install_default_drivers()` — gone. The `postgres`/`sqlite` cargo features
  become explicit (`nc-db/Cargo.toml`); the `any` feature and the remaining
  Any-typed call sites migrate with T4/T7.
- **No SQL changes in this task** — the win is the driver. Verification is
  behavior-neutral + latency; the T5 pool flags carry to both arms.

**Gate:** workspace builds; `cargo test --lib` (SQLite arm); perf-gate holds;
bench SC=14 p50 ≤ the T1 numbers.

### T7 — Single-query CTE PROPFIND (Postgres)

Postgres can do the entire child-set fan-out server-side. One statement
returns the listing + extended rows + dir counts + shares/notes + comments
(count + unread) + system tags for every child in one result set keyed on
`fileid` — the `IN`-list marshalling disappears entirely. This is CLAUDE.md
principle 2 taken to its conclusion: PHP does N queries here; we do **one**.

```sql
WITH kids AS (
  SELECT fc.*, fe.metadata_etag, fe.creation_time, fe.upload_time
  FROM {prefix}filecache fc
  LEFT JOIN {prefix}filecache_extended fe ON fe.fileid = fc.fileid
  WHERE fc.parent = $1 AND fc.storage = $2
)
SELECT k.*,
       (SELECT json_build_object(
           'dirs',  count(*) FILTER (WHERE c.mimetype = $3),
           'files', count(*) FILTER (WHERE c.mimetype != $3))
        FROM {prefix}filecache c WHERE c.parent = k.fileid AND c.storage = $2) AS dir_counts,
       (SELECT json_agg(json_build_object(
           'share_type', s.share_type, 'share_with', s.share_with,
           'uid_owner', s.uid_owner, 'note', s.note, 'stime', s.stime))
        FROM {prefix}share s WHERE s.file_source = k.fileid) AS shares,
       (SELECT json_build_object(
           'n', count(*),
           'unread', count(*) FILTER (WHERE c.actor_type = 'users'
             AND c.actor_id != $4
             AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00')))
        FROM {prefix}comments c
        LEFT JOIN {prefix}comments_read_markers m
          ON m.user_id = $4 AND m.object_type = 'files' AND m.object_id = c.object_id
        WHERE c.object_type = 'files' AND c.object_id = k.fileid::text) AS comments,
       (SELECT json_agg(json_build_object('id', t.id, 'name', t.name, 'color', t.color)
           ORDER BY LOWER(t.name))
        FROM {prefix}systemtag t
        JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id
        WHERE m.objectid = k.fileid::text AND m.objecttype = 'files'
          AND t.visibility = 1) AS system_tags
FROM kids k
```

Decisions:

- The `json_agg`/`json_build_object` result decodes in Rust into the same
  per-family shapes the T6 merges produce — **the merges make this
  tractable** — and the statement preserves the per-family semantics (share
  notes' `stime` ordering per file, the unread filter, the dir-counts'
  mimetype split).
- **`custom_properties_batch` stays a separate gated statement.** The
  `>250`-char property-path hash is Rust-side, and the children's names —
  needed to build those paths — only exist after the query returns; a CTE
  sub-select would need pgcrypto or an `unnest` indirection over paths built
  from a second query. One statement either way; the CTE still collapses 5
  families → 1.
- **Postgres-only.** SQLite keeps the JOIN listing + the batched families
  behind the variant. The lazy `cache/` ensure and the depth-0 root's
  single-row fallbacks stay as-is.

**Gate:** perf-gate re-measured — the scaling delta drops hard (the whole
fan-out is one statement; delta → ≈ 1); milestone suite. The difftest cannot
distinguish (same bytes) — the budget is the gate for this stop.

### T9 — Write path: propagation in one statement (`nc-dav/src/propagator.rs`)

`try_propagate` (`propagator.rs:141-261`) does `BEGIN` (`:162-166`) →
Postgres-only pre-lock `SELECT … ORDER BY path_hash FOR UPDATE` (`:172-190`)
→ one `UPDATE` (with-size `:199-229` or etag-only `:233-256`) → `COMMIT`
(`:259-261`) — **4 round trips per PUT**. A single-statement CTE preserves the
deadlock-avoiding `ORDER BY path_hash` lock order in one implicit transaction:

```sql
WITH locked AS (
  SELECT fileid FROM {prefix}filecache
  WHERE storage = $1 AND path_hash = ANY($2::text[])
  ORDER BY path_hash FOR UPDATE
)
UPDATE {prefix}filecache fc SET … FROM locked l WHERE fc.fileid = l.fileid
```

Decisions:

- **Postgres-only.** The SQLite branch (no row locks, no `FOR UPDATE`,
  `propagator.rs:169-171`) keeps the current multi-step path behind the
  dialect check. The `ANY($2::text[])` form needs the native pool (T3/T4).
- **The filecache + `filecache_extended` upsert merge is not part of this
  task.** Section 19 already flags the PUT path as the fastest-vs-PHP area
  (1.3-2.3×), the propagation CTE is the guaranteed win, and a merged
  statement would turn the extended upsert's warn-only failure into a fatal
  one (the merged statement fails atomically). The separate writes stay.

**Gate:** perf-gate re-measured (`put_new` drops); bench SC=10 (PUT) improves
or holds; milestone suite.

### T2 — Pipelining (tokio-postgres + deadpool)

sqlx **cannot pipeline**: every query takes `&mut conn` and awaits its full
round trip. With a native driver, tokio-postgres (+ deadpool-postgres) lets us
issue queries **without awaiting between sends** — all remaining per-request
statements on **one connection**, one RTT, no pool contention. This supersedes
the `join!` (T1).

Decisions:

- This is a driver swap behind the T3 enum (the `Pg` arm becomes
  tokio-postgres + deadpool). The row access layer changes (`PgRow` vs the
  sqlx row API), so it is the **largest refactor in this section** — do it
  only after T3-T7 land and the batch surface is stable. Note that T7's CTE
  already collapses the read fan-out on Postgres; pipelining covers the
  statements that remain (custom properties, the tag prefetch, the depth-0
  fallbacks).
- Keep the sqlx `Pg` arm only if the swap proves worse.
- Verification is the bench against the T1 numbers (SC=14 must improve beyond
  the `join!` result), not the gates — statement counts are unchanged.

**Gate:** S0 — driver swap green (workspace builds, `cargo test --lib`,
perf-gate holds); S1 — pipelined batch issues the remaining families on one
connection, bench SC=14 beats the `join!`/CTE numbers; milestone suite.

## Tier 4 — Verification / guardrails

### T10 — Index audit (verification only)

Verify against the live DB (`docker exec master-database-pgsql-1 psql -U
postgres -d nextcloud`) that the claimed indexes exist and are used, then
`EXPLAIN` the hot queries (the depth-1 batch, and the unread-marker join after
T6's de-correlation): `oc_filecache(parent, storage)`, `oc_properties(userid,
propertypath)`, `oc_share(file_source)`, `oc_systemtag_object_mapping(objectid,
objecttype)`, `oc_comments(object_type, object_id)`.

Caveat stands: the schema is owned by PHP Doctrine migrations, so **adding**
an index is a schema divergence — it needs an explicit decision and an
`improvements.md` entry, never a silent migration. Verification only; no
schema change unless a missing index shows up in `EXPLAIN` on the A/B stack.

**Gate:** `EXPLAIN` plans recorded for the batch + unread-join queries; the
decision record (add or explicitly defer) written before any schema change.

### Guardrail — already exists (section 20)

The proposed "per-request query counter" is **already implemented** as the
statement-budget gate: `core-rs/perf-budget.yaml` + `make perf-gate`
(`nc-bench budget`), documented in section 20. STRICT policy, zero headroom:
`propfind_depth0` 11, `propfind_depth1` 20, `put_new` 16,
`scaling_delta_budget` 9 — the delta is exactly the fixed batch cost (7 batch
queries + JOIN listing + tag prefetch). The gate counts real statements from
the Postgres log, which is stronger than an in-process counter.

Consequence for this section: **every task that removes statements must lower
the corresponding budget** (the policy is "reducing a count is the only way to
lower a budget"), and every task must re-run `make perf-gate`. The gate
measures **counts**, not RTTs — the latency claims of T1/T2/T3 are verified
with `nc-bench` latency scenarios against the A/B stack (sections 18/16), not
with the gate.

---

## Verification (every task)

- `make diff-test` — HTTP bytes must stay identical to PHP for every touched
  path (the A/B harness is the parity gate; `join!` and SQL rewrites change
  nothing client-visible).
- `make perf-gate` — statements must not increase; **lower the budget**
  whenever a task removes statements.
- `cargo test --lib` — SQLite unit tests must stay green (any Postgres-only
  SQL must be behind the dialect check, as `propagator.rs:169-171` already
  is).
- Latency: `nc-bench` scenarios against the live stack for depth-1 PROPFIND
  (SC=14) and PUT (SC=10), SUT vs oracle, before/after each task.

## Out of scope

- Index additions without a live-DB `EXPLAIN` + `improvements.md` decision —
  Doctrine owns the schema (T10).
- The filecache + `filecache_extended` upsert merge — see the T9 decision;
  the separate writes stay.
- The depth-0 root's single-row fallbacks — the root is visited by
  dav-server-rs before `read_dir`; batching it is a vendored-handler change
  beyond this section.
