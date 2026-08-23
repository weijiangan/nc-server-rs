//! Schema-validation integration tests (phase-9 9.1 / 9.9).
//!
//! These run against an in-memory SQLite database that mirrors the
//! PHP-created schema so Rust-only test harnesses can spin up a DB without a
//! full PHP install:
//!
//! - the **core+files tables** come from the `core-rs/migrations/` SQL, kept
//!   purely as schema docs + this isolated test DB (never applied at runtime
//!   — PHP owns the schema, improvements.md §I.3);
//! - the **PHP-app-owned tables** (`oc_files_trash`, `oc_vcategory`,
//!   `oc_vcategory_to_object`, `oc_comments`, `oc_comments_read_markers`,
//!   `oc_systemtag`, `oc_systemtag_object_mapping`, `oc_files_versions`) are
//!   created by explicit DDL matching the Doctrine migrations in
//!   `workspace/server/` — exactly the §9.1 gap tables plus the delegated-app
//!   read-only tables the validation covers.

use nc_db::pool::DbPool;

/// `CREATE TABLE` statements for the PHP-app-owned tables, columns matching
/// the Doctrine migrations byte-for-byte (file:line cited per table).
const APP_OWNED_DDL: &[&str] = &[
    // apps/files_trashbin Version1010Date20200630192639 + Version1020Date20240403003535
    "CREATE TABLE oc_files_trash (
        auto_id    INTEGER      PRIMARY KEY AUTOINCREMENT,
        id         VARCHAR(250) NOT NULL DEFAULT '',
        user       VARCHAR(64)  NOT NULL DEFAULT '',
        timestamp  VARCHAR(12)  NOT NULL DEFAULT '',
        location   VARCHAR(512) NOT NULL DEFAULT '',
        type       VARCHAR(4),
        mime       VARCHAR(255),
        deleted_by VARCHAR(64)
    )",
    // core Version13000Date20170718121200 (:636)
    "CREATE TABLE oc_vcategory (
        id       INTEGER      PRIMARY KEY AUTOINCREMENT,
        uid      VARCHAR(64)  NOT NULL DEFAULT '',
        type     VARCHAR(64)  NOT NULL DEFAULT '',
        category VARCHAR(255) NOT NULL DEFAULT ''
    )",
    // core Version13000Date20170718121200 (:666)
    "CREATE TABLE oc_vcategory_to_object (
        objid      INTEGER      NOT NULL DEFAULT 0,
        categoryid INTEGER      NOT NULL DEFAULT 0,
        type       VARCHAR(64)  NOT NULL DEFAULT '',
        PRIMARY KEY (categoryid, objid, type)
    )",
    // core Version13000Date20170718121200 (:785)
    "CREATE TABLE oc_comments (
        id                     INTEGER PRIMARY KEY AUTOINCREMENT,
        parent_id              INTEGER NOT NULL DEFAULT 0,
        topmost_parent_id      INTEGER NOT NULL DEFAULT 0,
        children_count         INTEGER NOT NULL DEFAULT 0,
        actor_type             VARCHAR(64) NOT NULL DEFAULT '',
        actor_id               VARCHAR(64) NOT NULL DEFAULT '',
        message                TEXT,
        verb                   VARCHAR(64),
        creation_timestamp     DATETIME,
        latest_child_timestamp DATETIME,
        object_type            VARCHAR(64) NOT NULL DEFAULT '',
        object_id              VARCHAR(64) NOT NULL DEFAULT '',
        reference_id           VARCHAR(64)
    )",
    // core Version13000Date20170718121200 (:855)
    "CREATE TABLE oc_comments_read_markers (
        user_id         VARCHAR(64) NOT NULL DEFAULT '',
        marker_datetime DATETIME,
        object_type     VARCHAR(64) NOT NULL DEFAULT '',
        object_id       VARCHAR(64) NOT NULL DEFAULT '',
        PRIMARY KEY (user_id, object_type, object_id)
    )",
    // core Version13000Date20170718121200 (:690) + apps/systemtags color column
    "CREATE TABLE oc_systemtag (
        id         INTEGER     PRIMARY KEY AUTOINCREMENT,
        name       VARCHAR(64) NOT NULL DEFAULT '',
        visibility SMALLINT    NOT NULL DEFAULT 1,
        editable   SMALLINT    NOT NULL DEFAULT 1,
        color      VARCHAR(6)
    )",
    // core Version13000Date20170718121200 (:717)
    "CREATE TABLE oc_systemtag_object_mapping (
        objectid    VARCHAR(64) NOT NULL DEFAULT '',
        objecttype  VARCHAR(64) NOT NULL DEFAULT '',
        systemtagid INTEGER     NOT NULL DEFAULT 0,
        PRIMARY KEY (objecttype, objectid, systemtagid)
    )",
    // apps/files_versions Version1020Date20221114144058 (:27)
    "CREATE TABLE oc_files_versions (
        id        INTEGER PRIMARY KEY AUTOINCREMENT,
        file_id   INTEGER NOT NULL,
        timestamp INTEGER NOT NULL,
        size      INTEGER NOT NULL,
        mimetype  INTEGER NOT NULL,
        metadata  TEXT    NOT NULL
    )",
    // previewgenerator app (external — shape documented in nc-dav preview_queue.rs)
    "CREATE TABLE oc_preview_generation (
        id        INTEGER      PRIMARY KEY AUTOINCREMENT,
        uid       VARCHAR(256) NOT NULL,
        file_id   BIGINT       NOT NULL,
        queued_at BIGINT       NOT NULL
    )",
];

/// Build the isolated SQLite fixture mirroring the PHP-created schema.
async fn fresh_schema_db() -> DbPool {
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite"),
    );

    // Core tables from the compile-time-embedded migration SQL (test-only).
    let migrator = sqlx::migrate!("../../migrations");
    match &pool {
        DbPool::Sqlite(p) => migrator
            .run(p)
            .await
            .expect("migrations failed on fresh DB"),
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    }

    // PHP-app-owned tables (§9.1 gap tables + delegated-app read-only tables).
    for ddl in APP_OWNED_DDL {
        match &pool {
            DbPool::Sqlite(p) => sqlx::query(ddl)
                .execute(p)
                .await
                .unwrap_or_else(|e| panic!("fixture DDL failed: {e}\n{ddl}")),
            DbPool::Pg(_) => unreachable!("test pools are sqlite"),
        };
    }

    pool
}

#[tokio::test]
async fn full_fixture_passes_validation() {
    let pool = fresh_schema_db().await;
    let report = nc_db::schema::validate_schema(&pool, "oc_")
        .await
        .expect("catalog query failed");
    assert!(
        report.is_ok(),
        "complete PHP-style schema should validate, got: {report:?}"
    );
    assert!(report.missing_tables.is_empty());
    assert!(report.missing_columns.is_empty());
}

#[tokio::test]
async fn missing_gap_table_is_reported() {
    let pool = fresh_schema_db().await;
    // Simulate a DB where the `tags` app rows were never created.
    match &pool {
        DbPool::Sqlite(p) => sqlx::query("DROP TABLE oc_vcategory")
            .execute(p)
            .await
            .unwrap(),
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    };
    let report = nc_db::schema::validate_schema(&pool, "oc_")
        .await
        .expect("catalog query failed");
    assert!(
        report.missing_tables.iter().any(|t| t == "oc_vcategory"),
        "expected oc_vcategory to be reported missing, got: {report:?}"
    );
    assert!(!report.is_ok());
}

#[tokio::test]
async fn missing_critical_column_is_reported() {
    let pool = fresh_schema_db().await;
    // Simulate a pre-NC-31 files_trashbin schema (deleted_by added later).
    match &pool {
        DbPool::Sqlite(p) => sqlx::query("ALTER TABLE oc_files_trash DROP COLUMN deleted_by")
            .execute(p)
            .await
            .unwrap(),
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    };
    let report = nc_db::schema::validate_schema(&pool, "oc_")
        .await
        .expect("catalog query failed");
    assert!(
        report
            .missing_columns
            .iter()
            .any(|(t, c)| t == "oc_files_trash" && c == "deleted_by"),
        "expected oc_files_trash.deleted_by to be reported missing, got: {report:?}"
    );
    assert!(!report.is_ok());
}

#[tokio::test]
async fn tables_are_readable_via_dispatch() {
    // Smoke-test that the fixture tables accept the same row shapes the
    // runtime queries use (nc-dav tags/trash paths on SQLite).
    let pool = fresh_schema_db().await;
    let table = "oc_vcategory".to_string();
    let sql = format!(
        "INSERT INTO {table} (uid, type, category) VALUES ('alice', 'files', '_$!<Favorite>!$_')"
    );
    match &pool {
        DbPool::Sqlite(p) => sqlx::query(&sql).execute(p).await.unwrap(),
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    };
    match &pool {
        DbPool::Sqlite(p) => {
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM oc_vcategory")
                .fetch_one(p)
                .await
                .unwrap();
            assert_eq!(n, 1);
        }
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    }
}

/// Sanity-check that the migration fixture produced the core tables (guards
/// against the disabled-migration situation silently producing an empty DB).
#[tokio::test]
async fn core_tables_present_in_fixture() {
    let pool = fresh_schema_db().await;
    match &pool {
        DbPool::Sqlite(p) => {
            let rows = sqlx::query(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'oc_filecache'",
            )
            .fetch_all(p)
            .await
            .unwrap();
            assert_eq!(rows.len(), 1, "oc_filecache missing from fixture");
        }
        DbPool::Pg(_) => unreachable!("test pools are sqlite"),
    }
}
