use std::collections::HashSet;
use std::sync::Arc;

use anyhow::Context;
use futures::future::BoxFuture;
use futures::stream::BoxStream;
use futures::{stream, FutureExt, StreamExt, TryStreamExt};
use sqlx::any::{Any, AnyArguments, AnyQueryResult, AnyRow, AnyStatement, AnyTypeInfo};
use sqlx::postgres::PgPool;
use sqlx::sqlite::SqlitePool;
use sqlx::{
    Column as _, Describe, Either, Execute, Executor, Postgres, Sqlite, Statement as _, Transaction,
};
use sqlx_core::any::AnyColumn;
use sqlx_core::ext::ustr::UStr;
use sqlx_postgres::{PgArguments, PgPoolOptions, PgQueryResult, PgRow, PgStatement};
use sqlx_sqlite::{
    SqliteArguments, SqlitePoolOptions, SqliteQueryResult, SqliteRow, SqliteStatement,
};

use crate::config::{DbType, NcConfig};

/// The connection pool for the configured database (PHASE-22 T3).
///
/// Replaces `sqlx::AnyPool`: the pool is now a native `PgPool` / `SqlitePool`
/// behind a small enum, so migrated call sites (T4/T7) can bind native array
/// arguments and decode rows without the `Any` driver's per-cell boxing.
///
/// All call sites that are NOT yet migrated keep compiling unchanged: the
/// enum implements sqlx's `Executor` for `Database = Any` by translating the
/// query's arguments to the native dialect (`AnyArguments::convert_to`),
/// executing on the inner native pool, and mapping the native rows back to
/// `AnyRow` (`TryFrom<&PgRow>` / `TryFrom<&SqliteRow>`) — exactly the
/// translation the `Any` driver used to do internally, now owned by nc-db.
#[derive(Clone, Debug)]
pub enum DbPool {
    Pg(PgPool),
    Sqlite(SqlitePool),
}

impl DbPool {
    /// Is the pool backed by PostgreSQL?  The dialect is fixed for a running
    /// server (CLAUDE.md principle 6) — this replaces the old process-global
    /// `backend_is_postgres()` latch with the enum variant itself.
    pub fn is_postgres(&self) -> bool {
        matches!(self, DbPool::Pg(_))
    }

    /// Begin a transaction on the pool.
    ///
    /// The transaction owns its pooled connection (`Transaction<'static,
    /// _>`); `&DbPool` cannot implement sqlx's `Acquire` (that would require
    /// fabricating an `AnyConnection`), so `begin()` is an inherent method
    /// and the transaction is the `DbTxn` enum (PHASE-22 T3.2).
    pub async fn begin(&self) -> anyhow::Result<DbTxn> {
        match self {
            DbPool::Pg(pool) => {
                let tx: Transaction<'static, sqlx::Postgres> = pool.begin().await?;
                Ok(DbTxn::Pg(tx))
            }
            DbPool::Sqlite(pool) => {
                let tx: Transaction<'static, sqlx::Sqlite> = pool.begin().await?;
                Ok(DbTxn::Sqlite(tx))
            }
        }
    }
}

/// An in-flight transaction owned by [`DbPool::begin`].
///
/// Like `DbPool` it implements `Executor` for `Database = Any`, so existing
/// `query.execute(&mut tx)` call sites compile unchanged; only `commit` /
/// `rollback` / the dialect check are inherent methods.
pub enum DbTxn {
    Pg(Transaction<'static, sqlx::Postgres>),
    Sqlite(Transaction<'static, sqlx::Sqlite>),
}

impl std::fmt::Debug for DbTxn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DbTxn::Pg(_) => f.write_str("DbTxn::Pg"),
            DbTxn::Sqlite(_) => f.write_str("DbTxn::Sqlite"),
        }
    }
}

impl DbTxn {
    pub fn is_postgres(&self) -> bool {
        matches!(self, DbTxn::Pg(_))
    }

    pub async fn commit(self) -> Result<(), sqlx::Error> {
        match self {
            DbTxn::Pg(tx) => tx.commit().await,
            DbTxn::Sqlite(tx) => tx.commit().await,
        }
    }

    pub async fn rollback(self) {
        match self {
            DbTxn::Pg(tx) => {
                let _ = tx.rollback().await;
            }
            DbTxn::Sqlite(tx) => {
                let _ = tx.rollback().await;
            }
        }
    }
}

// ─── Any-dialect executor delegation (PHASE-22 T3.1) ─────────────────────────
//
// The call sites build `sqlx::query(&sql)` — `Query<'_, Any, _>` — and run it
// against `&DbPool` (or `&mut DbTxn`).  These impls take the query's SQL +
// `AnyArguments`, convert the arguments to the native dialect, execute on
// the inner native pool/transaction, and map the native rows back to
// `AnyRow`.  Prepared-statement caching is preserved: the native connection
// caches are used and the query's `persistent()` flag is forwarded.

fn map_pg_step(
    step: Either<PgQueryResult, PgRow>,
) -> Result<Either<AnyQueryResult, AnyRow>, sqlx::Error> {
    Ok(match step {
        // Mirrors sqlx's own pg `any` backend (`map_result` in
        // sqlx-postgres/src/any.rs): the Any driver never exposes a
        // last-insert-id for Postgres.
        Either::Left(r) => Either::Left(AnyQueryResult {
            rows_affected: r.rows_affected(),
            last_insert_id: None,
        }),
        Either::Right(row) => Either::Right(AnyRow::try_from(&row)?),
    })
}

fn map_sqlite_step(
    step: Either<SqliteQueryResult, SqliteRow>,
) -> Result<Either<AnyQueryResult, AnyRow>, sqlx::Error> {
    Ok(match step {
        Either::Left(r) => Either::Left(AnyQueryResult {
            rows_affected: r.rows_affected(),
            last_insert_id: None,
        }),
        Either::Right(row) => Either::Right(AnyRow::try_from(&row)?),
    })
}

/// Convert the arguments off an `Execute<'q, Any>` into the native dialect,
/// returning the owned SQL copy alongside.  Error → `sqlx::Error::Encode`
/// (the argument failed to encode for the target database).
fn take_any_arguments<'q, E>(
    query: &mut E,
) -> Result<(String, Option<AnyArguments<'q>>), sqlx::Error>
where
    E: Execute<'q, Any> + 'q,
{
    let sql = query.sql().to_owned();
    let args = match query.take_arguments() {
        Ok(a) => a,
        Err(e) => return Err(sqlx::Error::Encode(e)),
    };
    Ok((sql, args))
}

/// `PgArguments`/`SqliteArguments` own their buffers in sqlx 0.8, so the
/// conversion can happen before the returned future/stream runs (the same
/// shape as sqlx's own any backends).
fn pg_column_names<'q>(stmt: &PgStatement<'q>) -> Arc<sqlx_core::HashMap<UStr, usize>> {
    Arc::new(
        stmt.columns()
            .iter()
            .enumerate()
            .map(|(i, c)| (UStr::new(c.name()), i))
            .collect(),
    )
}

fn sqlite_column_names<'q>(stmt: &SqliteStatement<'q>) -> Arc<sqlx_core::HashMap<UStr, usize>> {
    Arc::new(
        stmt.columns()
            .iter()
            .enumerate()
            .map(|(i, c)| (UStr::new(c.name()), i))
            .collect(),
    )
}

/// Map a native `Describe` to `Describe<Any>` (same shape sqlx's own Any
/// driver uses; the `TryFrom` impls are provided by sqlx's pg/sqlite `any`
/// integrations).
fn describe_to_any(d: Describe<Postgres>) -> Result<Describe<Any>, sqlx::Error> {
    let columns = d
        .columns
        .iter()
        .map(AnyColumn::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let parameters = match d.parameters {
        Some(Either::Left(types)) => Some(Either::Left(
            types
                .iter()
                .map(AnyTypeInfo::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Some(Either::Right(n)) => Some(Either::Right(n)),
        None => None,
    };
    Ok(Describe {
        columns,
        parameters,
        nullable: d.nullable,
    })
}

fn describe_sqlite_to_any(d: Describe<Sqlite>) -> Result<Describe<Any>, sqlx::Error> {
    let columns = d
        .columns
        .iter()
        .map(AnyColumn::try_from)
        .collect::<Result<Vec<_>, _>>()?;
    let parameters = match d.parameters {
        Some(Either::Left(types)) => Some(Either::Left(
            types
                .iter()
                .map(AnyTypeInfo::try_from)
                .collect::<Result<Vec<_>, _>>()?,
        )),
        Some(Either::Right(n)) => Some(Either::Right(n)),
        None => None,
    };
    Ok(Describe {
        columns,
        parameters,
        nullable: d.nullable,
    })
}

impl<'c> Executor<'c> for &'c DbPool {
    type Database = Any;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Any>,
    {
        let (sql, arguments) = match take_any_arguments(&mut query) {
            Ok(v) => v,
            Err(e) => return Box::pin(stream::once(futures::future::ready(Err(e)))),
        };
        let persistent = query.persistent();
        match self {
            DbPool::Pg(pool) => {
                let pool = pool.clone();
                Box::pin(
                    Box::pin(async move {
                        let args: Result<Option<PgArguments>, sqlx::Error> = arguments
                            .as_ref()
                            .map(AnyArguments::convert_to)
                            .transpose()
                            .map_err(sqlx::Error::Encode);
                        match args {
                            Err(e) => stream::iter(vec![Err(e)]).boxed(),
                            Ok(args) => {
                                let native: BoxStream<
                                    '_,
                                    Result<Either<PgQueryResult, PgRow>, sqlx::Error>,
                                > = match args {
                                    Some(a) => sqlx::query_with::<Postgres, _>(sql.as_str(), a)
                                        .persistent(persistent)
                                        .fetch_many(&pool),
                                    None => sqlx::query(sql.as_str())
                                        .persistent(persistent)
                                        .fetch_many(&pool),
                                };
                                let mut native = native;
                                // The native stream borrows the block-local `sql`; collect
                                // everything into an owned Vec first so the returned
                                // stream does not escape the borrow (no streaming
                                // consumers exist, so this is equivalent).
                                let mut out: Vec<
                                    Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>,
                                > = Vec::new();
                                loop {
                                    match native.try_next().await {
                                        Ok(Some(step)) => match map_pg_step(step) {
                                            Ok(v) => out.push(Ok(v)),
                                            Err(e) => {
                                                out.push(Err(e));
                                                break;
                                            }
                                        },
                                        Ok(None) => break,
                                        Err(e) => {
                                            out.push(Err(e));
                                            break;
                                        }
                                    }
                                }
                                stream::iter(out).boxed()
                            }
                        }
                    })
                    .flatten_stream(),
                )
            }
            DbPool::Sqlite(pool) => {
                let pool = pool.clone();
                Box::pin(
                    Box::pin(async move {
                        let args: Result<Option<SqliteArguments<'_>>, sqlx::Error> = arguments
                            .as_ref()
                            .map(AnyArguments::convert_to)
                            .transpose()
                            .map_err(sqlx::Error::Encode);
                        match args {
                            Err(e) => stream::iter(vec![Err(e)]).boxed(),
                            Ok(args) => {
                                let native: BoxStream<
                                    '_,
                                    Result<Either<SqliteQueryResult, SqliteRow>, sqlx::Error>,
                                > = match args {
                                    Some(a) => sqlx::query_with::<Sqlite, _>(sql.as_str(), a)
                                        .persistent(persistent)
                                        .fetch_many(&pool),
                                    None => sqlx::query(sql.as_str())
                                        .persistent(persistent)
                                        .fetch_many(&pool),
                                };
                                let mut native = native;
                                let mut out: Vec<
                                    Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>,
                                > = Vec::new();
                                loop {
                                    match native.try_next().await {
                                        Ok(Some(step)) => match map_sqlite_step(step) {
                                            Ok(v) => out.push(Ok(v)),
                                            Err(e) => {
                                                out.push(Err(e));
                                                break;
                                            }
                                        },
                                        Ok(None) => break,
                                        Err(e) => {
                                            out.push(Err(e));
                                            break;
                                        }
                                    }
                                }
                                stream::iter(out).boxed()
                            }
                        }
                    })
                    .flatten_stream(),
                )
            }
        }
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxFuture<'e, Result<Option<AnyRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Any>,
    {
        let (sql, arguments) = match take_any_arguments(&mut query) {
            Ok(v) => v,
            Err(e) => return Box::pin(futures::future::ready(Err(e))),
        };
        let persistent = query.persistent();
        match self {
            DbPool::Pg(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let args: Option<PgArguments> = arguments
                        .as_ref()
                        .map(AnyArguments::convert_to)
                        .transpose()
                        .map_err(sqlx::Error::Encode)?;
                    let row = match args {
                        Some(a) => {
                            sqlx::query_with::<Postgres, _>(sql.as_str(), a)
                                .persistent(persistent)
                                .fetch_optional(&pool)
                                .await?
                        }
                        None => {
                            sqlx::query(sql.as_str())
                                .persistent(persistent)
                                .fetch_optional(&pool)
                                .await?
                        }
                    };
                    match row {
                        Some(row) => Ok(Some(AnyRow::try_from(&row)?)),
                        None => Ok(None),
                    }
                })
            }
            DbPool::Sqlite(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let args: Option<SqliteArguments<'_>> = arguments
                        .as_ref()
                        .map(AnyArguments::convert_to)
                        .transpose()
                        .map_err(sqlx::Error::Encode)?;
                    let row = match args {
                        Some(a) => {
                            sqlx::query_with::<Sqlite, _>(sql.as_str(), a)
                                .persistent(persistent)
                                .fetch_optional(&pool)
                                .await?
                        }
                        None => {
                            sqlx::query(sql.as_str())
                                .persistent(persistent)
                                .fetch_optional(&pool)
                                .await?
                        }
                    };
                    match row {
                        Some(row) => Ok(Some(AnyRow::try_from(&row)?)),
                        None => Ok(None),
                    }
                })
            }
        }
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        _parameters: &[AnyTypeInfo],
    ) -> BoxFuture<'e, Result<AnyStatement<'q>, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            DbPool::Pg(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let mut conn = pool.acquire().await?;
                    let stmt = conn.prepare_with(sql, &[]).await?;
                    let names = pg_column_names(&stmt);
                    AnyStatement::try_from_statement(sql, &stmt, names)
                })
            }
            DbPool::Sqlite(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let mut conn = pool.acquire().await?;
                    let stmt = conn.prepare_with(sql, &[]).await?;
                    let names = sqlite_column_names(&stmt);
                    AnyStatement::try_from_statement(sql, &stmt, names)
                })
            }
        }
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe<Any>, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            DbPool::Pg(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let mut conn = pool.acquire().await?;
                    let d = conn.describe(sql).await?;
                    describe_to_any(d)
                })
            }
            DbPool::Sqlite(pool) => {
                let pool = pool.clone();
                Box::pin(async move {
                    let mut conn = pool.acquire().await?;
                    let d = conn.describe(sql).await?;
                    describe_sqlite_to_any(d)
                })
            }
        }
    }
}

impl<'c> Executor<'c> for &'c mut DbTxn {
    type Database = Any;

    fn fetch_many<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxStream<'e, Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Any>,
    {
        let (sql, arguments) = match take_any_arguments(&mut query) {
            Ok(v) => v,
            Err(e) => return Box::pin(stream::once(futures::future::ready(Err(e)))),
        };
        let persistent = query.persistent();
        match self {
            DbTxn::Pg(tx) => {
                let conn = &mut **tx;
                Box::pin(
                    Box::pin(async move {
                        let args: Result<Option<PgArguments>, sqlx::Error> = arguments
                            .as_ref()
                            .map(AnyArguments::convert_to)
                            .transpose()
                            .map_err(sqlx::Error::Encode);
                        match args {
                            Err(e) => stream::iter(vec![Err(e)]).boxed(),
                            Ok(args) => {
                                let native: BoxStream<
                                    '_,
                                    Result<Either<PgQueryResult, PgRow>, sqlx::Error>,
                                > = match args {
                                    Some(a) => sqlx::query_with::<Postgres, _>(sql.as_str(), a)
                                        .persistent(persistent)
                                        .fetch_many(&mut *conn),
                                    None => sqlx::query(sql.as_str())
                                        .persistent(persistent)
                                        .fetch_many(&mut *conn),
                                };
                                let mut native = native;
                                let mut out: Vec<
                                    Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>,
                                > = Vec::new();
                                loop {
                                    match native.try_next().await {
                                        Ok(Some(step)) => match map_pg_step(step) {
                                            Ok(v) => out.push(Ok(v)),
                                            Err(e) => {
                                                out.push(Err(e));
                                                break;
                                            }
                                        },
                                        Ok(None) => break,
                                        Err(e) => {
                                            out.push(Err(e));
                                            break;
                                        }
                                    }
                                }
                                stream::iter(out).boxed()
                            }
                        }
                    })
                    .flatten_stream(),
                )
            }
            DbTxn::Sqlite(tx) => {
                let conn = &mut **tx;
                Box::pin(
                    Box::pin(async move {
                        let args: Result<Option<SqliteArguments<'_>>, sqlx::Error> = arguments
                            .as_ref()
                            .map(AnyArguments::convert_to)
                            .transpose()
                            .map_err(sqlx::Error::Encode);
                        match args {
                            Err(e) => stream::iter(vec![Err(e)]).boxed(),
                            Ok(args) => {
                                let native: BoxStream<
                                    '_,
                                    Result<Either<SqliteQueryResult, SqliteRow>, sqlx::Error>,
                                > = match args {
                                    Some(a) => sqlx::query_with::<Sqlite, _>(sql.as_str(), a)
                                        .persistent(persistent)
                                        .fetch_many(&mut *conn),
                                    None => sqlx::query(sql.as_str())
                                        .persistent(persistent)
                                        .fetch_many(&mut *conn),
                                };
                                let mut native = native;
                                let mut out: Vec<
                                    Result<Either<AnyQueryResult, AnyRow>, sqlx::Error>,
                                > = Vec::new();
                                loop {
                                    match native.try_next().await {
                                        Ok(Some(step)) => match map_sqlite_step(step) {
                                            Ok(v) => out.push(Ok(v)),
                                            Err(e) => {
                                                out.push(Err(e));
                                                break;
                                            }
                                        },
                                        Ok(None) => break,
                                        Err(e) => {
                                            out.push(Err(e));
                                            break;
                                        }
                                    }
                                }
                                stream::iter(out).boxed()
                            }
                        }
                    })
                    .flatten_stream(),
                )
            }
        }
    }

    fn fetch_optional<'e, 'q: 'e, E>(
        self,
        mut query: E,
    ) -> BoxFuture<'e, Result<Option<AnyRow>, sqlx::Error>>
    where
        'c: 'e,
        E: 'q + Execute<'q, Any>,
    {
        let (sql, arguments) = match take_any_arguments(&mut query) {
            Ok(v) => v,
            Err(e) => return Box::pin(futures::future::ready(Err(e))),
        };
        let persistent = query.persistent();
        match self {
            DbTxn::Pg(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let args: Option<PgArguments> = arguments
                        .as_ref()
                        .map(AnyArguments::convert_to)
                        .transpose()
                        .map_err(sqlx::Error::Encode)?;
                    let row = match args {
                        Some(a) => {
                            sqlx::query_with::<Postgres, _>(sql.as_str(), a)
                                .persistent(persistent)
                                .fetch_optional(&mut *conn)
                                .await?
                        }
                        None => {
                            sqlx::query(sql.as_str())
                                .persistent(persistent)
                                .fetch_optional(&mut *conn)
                                .await?
                        }
                    };
                    match row {
                        Some(row) => Ok(Some(AnyRow::try_from(&row)?)),
                        None => Ok(None),
                    }
                })
            }
            DbTxn::Sqlite(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let args: Option<SqliteArguments<'_>> = arguments
                        .as_ref()
                        .map(AnyArguments::convert_to)
                        .transpose()
                        .map_err(sqlx::Error::Encode)?;
                    let row = match args {
                        Some(a) => {
                            sqlx::query_with::<Sqlite, _>(sql.as_str(), a)
                                .persistent(persistent)
                                .fetch_optional(&mut *conn)
                                .await?
                        }
                        None => {
                            sqlx::query(sql.as_str())
                                .persistent(persistent)
                                .fetch_optional(&mut *conn)
                                .await?
                        }
                    };
                    match row {
                        Some(row) => Ok(Some(AnyRow::try_from(&row)?)),
                        None => Ok(None),
                    }
                })
            }
        }
    }

    fn prepare_with<'e, 'q: 'e>(
        self,
        sql: &'q str,
        _parameters: &[AnyTypeInfo],
    ) -> BoxFuture<'e, Result<AnyStatement<'q>, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            DbTxn::Pg(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let stmt = conn.prepare_with(sql, &[]).await?;
                    let names = pg_column_names(&stmt);
                    AnyStatement::try_from_statement(sql, &stmt, names)
                })
            }
            DbTxn::Sqlite(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let stmt = conn.prepare_with(sql, &[]).await?;
                    let names = sqlite_column_names(&stmt);
                    AnyStatement::try_from_statement(sql, &stmt, names)
                })
            }
        }
    }

    fn describe<'e, 'q: 'e>(self, sql: &'q str) -> BoxFuture<'e, Result<Describe<Any>, sqlx::Error>>
    where
        'c: 'e,
    {
        match self {
            DbTxn::Pg(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let d = conn.describe(sql).await?;
                    describe_to_any(d)
                })
            }
            DbTxn::Sqlite(tx) => {
                let conn = &mut **tx;
                Box::pin(async move {
                    let d = conn.describe(sql).await?;
                    describe_sqlite_to_any(d)
                })
            }
        }
    }
}

// ─── pool construction ───────────────────────────────────────────────────────

/// Count physical cores, excluding hyperthreads.
///
/// On Linux, physical cores are the unique `(physical_package_id, core_id)`
/// pairs in sysfs — hyperthreads share a `core_id` and must not inflate the
/// pool size (the production server reports 2 physical cores where
/// `nproc`/`available_parallelism` would say 4 logical).  Falls back to
/// logical CPUs where sysfs is unavailable.
fn physical_cores() -> usize {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut i = 0usize;
    loop {
        let core =
            std::fs::read_to_string(format!("/sys/devices/system/cpu/cpu{i}/topology/core_id"));
        let pkg = std::fs::read_to_string(format!(
            "/sys/devices/system/cpu/cpu{i}/topology/physical_package_id"
        ));
        match (core, pkg) {
            (Ok(c), Ok(p)) => {
                seen.insert((p.trim().to_string(), c.trim().to_string()));
                i += 1;
            }
            _ => break,
        }
    }
    if !seen.is_empty() {
        seen.len()
    } else {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    }
}

/// Build a connection pool for the database described in `config`.
///
/// The pool is created but not yet used — the caller drives the
/// `sqlx::migrate!()` step before opening the HTTP listener.  The pool is a
/// native `PgPool` / `SqlitePool` behind the `DbPool` enum (PHASE-22 T3);
/// no `Any` driver registry is involved.
pub async fn build_pool(config: &NcConfig) -> anyhow::Result<DbPool> {
    let min = 5u32;
    // 4× physical cores (hyperthreads excluded), floored at 16 so the
    // 6-query depth-1 PROPFIND batch (`read_dir` join, phase-21 S1 / T6)
    // plus concurrent traffic never queues on the pool, capped at 64 so big
    // hosts don't thrash Postgres backends.  A Rust server actually
    // saturates its DB — 50 fixed backends was arbitrary.
    // 2-core prod → 16; 6-core dev → 24; 16-core → 64.
    let cores = physical_cores() as u32;
    let max = (cores * 4).clamp(16, 64);

    // No ping on acquire: with ~9 sequential fetch_*(pool) calls per
    // PROPFIND that is ~9 pure-overhead RTTs (sqlx pings every idle
    // acquire by default; the Postgres ping is a full flush round trip).
    // Dead connections are detected on first use and discarded;
    // max_lifetime/idle_timeout prune idle ones.
    let pool = match config.dbtype {
        DbType::Pgsql => {
            let host = config.dbhost.as_deref().unwrap_or("localhost");
            let name = config
                .dbname
                .as_deref()
                .context("dbname is required for pgsql")?;
            let user = config
                .dbuser
                .as_deref()
                .context("dbuser is required for pgsql")?;
            let pass = config.dbpassword.as_deref().unwrap_or("");
            let url = format!("postgresql://{user}:{pass}@{host}/{name}");
            let pool = PgPoolOptions::new()
                .min_connections(min)
                .max_connections(max)
                .test_before_acquire(false)
                .connect(&url)
                .await
                .with_context(|| format!("Failed to connect to database at {}", redact(&url)))?;
            // Verify connectivity.
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .context("Database health-check query failed")?;
            DbPool::Pg(pool)
        }
        DbType::Sqlite => {
            // For SQLite the "host" is a file path in dbname.
            let path = config.dbname.as_deref().unwrap_or("nextcloud.db");
            let url = format!("sqlite://{path}?mode=rwc");
            let pool = SqlitePoolOptions::new()
                .min_connections(min)
                .max_connections(max)
                .test_before_acquire(false)
                .connect(&url)
                .await
                .with_context(|| format!("Failed to connect to database at {}", redact(&url)))?;
            // Verify connectivity.
            sqlx::query("SELECT 1")
                .execute(&pool)
                .await
                .context("Database health-check query failed")?;
            DbPool::Sqlite(pool)
        }
    };

    tracing::info!(
        driver = %match config.dbtype {
            DbType::Pgsql => "postgres",
            DbType::Sqlite => "sqlite",
        },
        "Database pool ready (min={min}, max={max})"
    );

    Ok(pool)
}

/// Redact password from a connection URL for safe logging.
fn redact(url: &str) -> String {
    if let Some(at) = url.rfind('@') {
        if let Some(scheme_end) = url.find("://") {
            let scheme = &url[..scheme_end + 3];
            let after_at = &url[at..];
            return format!("{scheme}***{after_at}");
        }
    }
    url.to_string()
}
