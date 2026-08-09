//! Consistent PostgreSQL snapshot (Phase 16.3).
//!
//! Enumerates the `oc_%` tables, dumps each one inside a single
//! `REPEATABLE READ` transaction (one consistent cross-table view per instance,
//! never committed), and returns the rows as text so the canonicalizer
//! (Phase 16.4/16.5) can classify and compare them.
//!
//! Ground truth for column types/forms is the **live** schema (CLAUDE.md
//! principles 3 & 6). Every value is rendered by Postgres itself via `::text`,
//! so both instances — on the same server — produce byte-identical encodings
//! for equal values.

use std::collections::BTreeMap;

use anyhow::{bail, Context, Result};
use sqlx::postgres::{PgConnection, PgPool};
use sqlx::Row;

/// Tables never snapshotted (plan §3 skip-list). `*_queue` is handled by suffix.
///
/// `oc_preferences` is skipped too: it is per-user **runtime/UI** state that the
/// PHP path writes on almost every request (`login.lastLogin`,
/// `files.lastSeenQuotaUsage`, …) while the SUT serves those paths natively and
/// never touches it. Those writes are PHP-side noise, not Rust file-behavior, so
/// diffing the table only produces false positives. File-behavior parity lives
/// in `oc_filecache`/`_extended`/`oc_files_versions`/`oc_properties`, not here.
///
/// `oc_ratelimit_entries` (Phase 16.7): per-request rate-limit bookkeeping.
/// Every rate-limited request runs a table-wide `DELETE WHERE delete_after <=
/// now` and then inserts its own attempt row with `delete_after = now + period`
/// (`lib/private/Security/RateLimiting/Backend/DatabaseBackend.php:43,83`), so
/// which rows a run removes depends purely on wall-clock timing (whether
/// earlier entries expired before the run) — timing noise, not behavior. Rate
/// limiting is a response-level concern (429) covered by the status comparison.
const SKIP_TABLES: &[&str] = &[
    "oc_sessions",
    "oc_jobs",
    "oc_preferences",
    "oc_ratelimit_entries",
    // CardDAV + Circles side effects of OCS user-account edits (Phase 16.10,
    // scenario 23: setting a quota updates the user's system-addressbook card
    // and the circles member cache). Both sides run identical proxied PHP for
    // these ops, but the rows carry per-instance identity that cannot be
    // compared without hostname masking: card URLs embed the instance hostname
    // (nextcloud.local vs oracle.local), synctokens are per-instance monotonic
    // watermarks, circle/member ids are per-instance random, cached_update is
    // wall-clock. Orthogonal to the file-behavior differential — like
    // oc_preferences. If nc-ocs ever serves user edits natively, these need
    // targeted scenarios + classification instead (as 16.7 did for oc_share).
    "oc_addressbookchanges",
    "oc_addressbooks",
    "oc_cards",
    "oc_cards_properties",
    "oc_circles_member",
];

/// One table's snapshot: column names in ordinal order + rows in PK order. Each
/// cell is the Postgres `::text` rendering (`None` = NULL).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TableData {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

/// A whole-instance snapshot: every non-skipped `oc_%` table.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Snapshot {
    pub tables: BTreeMap<String, TableData>,
}

fn skip_table(name: &str) -> bool {
    SKIP_TABLES.contains(&name) || name.ends_with("_queue")
}

/// Count rows in one table (the quiescence drain-wait polls
/// `oc_preview_generation` this way; the table name is a fixed literal at the
/// call site, never user input).
pub async fn count_table(dsn: &str, table: &str) -> Result<i64> {
    let pool = PgPool::connect(dsn)
        .await
        .with_context(|| format!("connecting to {dsn}"))?;
    let sql = format!("SELECT COUNT(*) FROM {table}");
    let n: i64 = sqlx::query_scalar(&sql)
        .fetch_one(&pool)
        .await
        .with_context(|| format!("counting {table} on {dsn}"))?;
    Ok(n)
}

/// The oc_jobs id of the previewgenerator's PreviewJob.  The snowflake ids
/// are per-instance (the SUT and oracle have independent oc_jobs tables), so
/// the quiescence drain looks each side's id up before forcing the job.
pub async fn preview_job_id(dsn: &str) -> Result<Option<i64>> {
    let pool = PgPool::connect(dsn)
        .await
        .with_context(|| format!("connecting to {dsn}"))?;
    let id: Option<i64> = sqlx::query_scalar(
        "SELECT id FROM oc_jobs WHERE class LIKE '%PreviewJob' ORDER BY id LIMIT 1",
    )
    .fetch_optional(&pool)
    .await
    .with_context(|| format!("looking up PreviewJob on {dsn}"))?;
    Ok(id)
}

/// Snapshot one instance. `dsn` is a `postgres://` connection string.
pub async fn snapshot(dsn: &str) -> Result<Snapshot> {
    let pool = PgPool::connect(dsn)
        .await
        .with_context(|| format!("connecting to {dsn}"))?;

    // One REPEATABLE READ transaction → a consistent cross-table view. We never
    // commit: the txn is dropped (rolled back) when `tx` goes out of scope.
    let mut tx = pool.begin().await.context("BEGIN")?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *tx)
        .await
        .context("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")?;

    let names = list_tables(&mut tx).await?;
    let mut snap = Snapshot::default();
    for name in names {
        if skip_table(&name) {
            continue;
        }
        let data = dump_table(&mut tx, &name)
            .await
            .with_context(|| format!("dumping table {name}"))?;
        snap.tables.insert(name, data);
    }
    // `tx` drops here → rollback (read-only; nothing to commit).
    Ok(snap)
}

/// Assert both snapshots expose the same set of tables (modulo the skip-list).
/// A table present on one side only is itself a divergence (e.g. an app enabled
/// on one instance created it).
pub fn assert_table_parity(sut: &Snapshot, oracle: &Snapshot) -> Result<()> {
    let sut_only: Vec<&String> = sut
        .tables
        .keys()
        .filter(|k| !oracle.tables.contains_key(*k))
        .collect();
    let oracle_only: Vec<&String> = oracle
        .tables
        .keys()
        .filter(|k| !sut.tables.contains_key(*k))
        .collect();
    if !sut_only.is_empty() || !oracle_only.is_empty() {
        bail!(
            "table-set mismatch:\n  SUT-only:    {sut_only:?}\n  oracle-only: {oracle_only:?}"
        );
    }
    Ok(())
}

/// List the `oc_%` tables in the current schema.
async fn list_tables(tx: &mut PgConnection) -> Result<Vec<String>> {
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT tablename FROM pg_catalog.pg_tables \
         WHERE schemaname = current_schema() AND tablename LIKE 'oc\\_%' ESCAPE '\\' \
         ORDER BY tablename",
    )
    .fetch_all(&mut *tx)
    .await
    .context("listing oc_% tables")?;
    Ok(rows)
}

/// Column names of a table, in ordinal order.
async fn list_columns(tx: &mut PgConnection, table: &str) -> Result<Vec<String>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT column_name FROM information_schema.columns \
         WHERE table_schema = current_schema() AND table_name = $1 \
         ORDER BY ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut *tx)
    .await
    .with_context(|| format!("listing columns of {table}"))?;
    Ok(cols)
}

/// Primary-key columns of a table (empty if it has none).
async fn pk_columns(tx: &mut PgConnection, table: &str) -> Result<Vec<String>> {
    let cols: Vec<String> = sqlx::query_scalar(
        "SELECT kcu.column_name \
         FROM information_schema.table_constraints tc \
         JOIN information_schema.key_column_usage kcu \
           ON tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema \
         WHERE tc.table_schema = current_schema() \
           AND tc.table_name = $1 \
           AND tc.constraint_type = 'PRIMARY KEY' \
         ORDER BY kcu.ordinal_position",
    )
    .bind(table)
    .fetch_all(&mut *tx)
    .await
    .with_context(|| format!("reading PK of {table}"))?;
    Ok(cols)
}

/// Dump one table: `SELECT <col>::text, … ORDER BY <pk>` (or all columns when
/// there is no PK, so ordering is still deterministic).
async fn dump_table(tx: &mut PgConnection, table: &str) -> Result<TableData> {
    let columns = list_columns(tx, table).await?;
    if columns.is_empty() {
        return Ok(TableData::default());
    }

    let pk = pk_columns(tx, table).await?;
    let order_cols = if pk.is_empty() { columns.clone() } else { pk };

    let select = columns
        .iter()
        .map(|c| format!(r#""{c}"::text"#))
        .collect::<Vec<_>>()
        .join(", ");
    // Qualify with the table name so ORDER BY uses the source column's native
    // type, not the `::text` projection.
    let order = order_cols
        .iter()
        .map(|c| format!(r#""{table}"."{c}""#))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(r#"SELECT {select} FROM "{table}" ORDER BY {order}"#);
    let rows = sqlx::query(&sql)
        .fetch_all(&mut *tx)
        .await
        .with_context(|| format!("SELECT on {table}"))?;

    let mut data = TableData {
        columns,
        rows: Vec::with_capacity(rows.len()),
    };
    for row in rows {
        let mut cells = Vec::with_capacity(row.len());
        for i in 0..row.len() {
            let v: Option<String> = row.try_get(i)?;
            cells.push(v);
        }
        data.rows.push(cells);
    }
    Ok(data)
}
