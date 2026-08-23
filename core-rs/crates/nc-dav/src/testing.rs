//! Shared fixtures for the DB-backed unit tests (in-memory SQLite + a temp
//! data dir).  Lives outside `#[cfg(test)] mod tests` so the trashbin and
//! mutation suites can share one seeded home tree.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use sqlx::{Row as _, Sqlite};

use nc_db::appconfig::AppConfigCache;
use nc_db::config::NcConfig;
use nc_db::filename_validator::FilenameValidator;
use nc_db::mime::MimeCache;
use nc_db::pool::DbPool;

use crate::preview::ProviderRegistry;
use crate::row;
use crate::upload::UploadStateStore;
use crate::{NcFileSystem, SharedWriteResult, WriteResult};

/// The in-memory test DB is always SQLite; unwrap the variant for the
/// native queries below (tests never construct a Pg pool).
pub(crate) fn test_pool(pool: &DbPool) -> &sqlx::SqlitePool {
    match pool {
        DbPool::Sqlite(p) => p,
        DbPool::Pg(_) => panic!("test pools are sqlite"),
    }
}

static TEST_DIR_COUNTER: AtomicU64 = AtomicU64::new(0);

pub(crate) fn fresh_data_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "nc-dav-trash-test-{}-{}",
        std::process::id(),
        TEST_DIR_COUNTER.fetch_add(1, Ordering::SeqCst)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

pub(crate) fn touch(path: &Path) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, vec![b'x'; 26]).unwrap();
}

/// The `.d{timestamp}` of the most recent trash operation (from the
/// `oc_files_trash` row — the naming is derived from the deletion second).
pub(crate) async fn trash_ts(pool: &DbPool) -> String {
    sqlx::query_scalar::<Sqlite, String>("SELECT \"timestamp\" FROM oc_files_trash LIMIT 1")
        .fetch_one(test_pool(pool))
        .await
        .unwrap()
}

/// In-memory SQLite with the delete-path tables and a seeded home tree.
///
/// Seed (fileids fixed, matching the propagator test convention):
/// ```text
/// 1  "" (root, size -1)        2  "files" (size 100)
/// 6  "files_versions" (0)      4  "files/hello.txt" (26)
/// ```
pub(crate) async fn fresh_delete_db() -> (DbPool, String, i64) {
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite"),
    );

    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_filecache (
            fileid           INTEGER NOT NULL PRIMARY KEY,
            storage          BIGINT  NOT NULL DEFAULT 0,
            path             VARCHAR(4000),
            path_hash        VARCHAR(32) NOT NULL DEFAULT '',
            parent           BIGINT  NOT NULL DEFAULT 0,
            name             VARCHAR(250),
            mimetype         BIGINT  NOT NULL DEFAULT 0,
            mimepart         BIGINT  NOT NULL DEFAULT 0,
            size             BIGINT  NOT NULL DEFAULT 0,
            mtime            INTEGER NOT NULL DEFAULT 0,
            storage_mtime    INTEGER NOT NULL DEFAULT 0,
            etag             VARCHAR(40),
            permissions      INTEGER DEFAULT 0,
            checksum         VARCHAR(255)
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create filecache");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_filecache_extended (
            fileid         INTEGER NOT NULL PRIMARY KEY,
            metadata_etag  VARCHAR(40) NOT NULL DEFAULT '',
            creation_time  BIGINT NOT NULL DEFAULT 0,
            upload_time    BIGINT NOT NULL DEFAULT 0
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create filecache_extended");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_files_versions (
            id         INTEGER NOT NULL PRIMARY KEY,
            file_id    BIGINT NOT NULL,
            \"timestamp\" BIGINT NOT NULL,
            size       BIGINT NOT NULL,
            mimetype   BIGINT NOT NULL,
            metadata   TEXT
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create files_versions");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_files_trash (
            id         VARCHAR(250) NOT NULL,
            \"user\"      VARCHAR(64) NOT NULL,
            \"timestamp\" VARCHAR(12) NOT NULL,
            location   VARCHAR(512) NOT NULL,
            deleted_by VARCHAR(64) NOT NULL
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create files_trash");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_preview_generation (
            id        INTEGER NOT NULL PRIMARY KEY,
            uid       VARCHAR(64) NOT NULL,
            file_id   BIGINT NOT NULL,
            queued_at BIGINT NOT NULL
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create preview_generation");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_mimetypes (
            id       BIGINT NOT NULL PRIMARY KEY,
            mimetype VARCHAR(255) NOT NULL
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create mimetypes");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_appconfig (
            appid      VARCHAR(32) NOT NULL,
            configkey  VARCHAR(64) NOT NULL,
            configvalue VARCHAR(4000) NOT NULL,
            type       INTEGER NOT NULL DEFAULT 0,
            lazy       INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create appconfig");

    // files_trashbin enabled.
    sqlx::query::<Sqlite>(
        "INSERT INTO oc_appconfig (appid, configkey, configvalue, type, lazy) \
         VALUES ('files_trashbin', 'enabled', 'yes', 0, 0)",
    )
    .execute(test_pool(&pool))
    .await
    .expect("seed appconfig");

    let prefix = "oc_".to_string();
    let storage_id = 1i64;
    for (fid, path, parent, size, name) in [
        (1i64, "", -1i64, -1i64, ""),
        (2, "files", 1, 100, "files"),
        (6, "files_versions", 1, 0, "files_versions"),
        (4, "files/hello.txt", 2, 26, "hello.txt"),
    ] {
        sqlx::query::<Sqlite>(&format!(
            "INSERT INTO {prefix}filecache \
             (fileid, storage, path, path_hash, parent, name, mimetype, mimepart, \
              size, mtime, storage_mtime, etag, permissions, checksum) \
             VALUES ($1, $2, $3, $4, $5, $6, 0, 0, $7, 100, 100, 'etag', 27, '')"
        ))
        .bind(fid)
        .bind(storage_id)
        .bind(path)
        .bind(row::path_hash(path))
        .bind(parent)
        .bind(name)
        .bind(size)
        .execute(test_pool(&pool))
        .await
        .expect("seed filecache");
    }

    (pool, prefix, storage_id)
}

pub(crate) fn test_fs(
    pool: DbPool,
    prefix: String,
    storage_id: i64,
    data_dir: PathBuf,
) -> NcFileSystem {
    let cfg = NcConfig::from_php_config("<?php\n$CONFIG = ['dbtype' => 'sqlite3'];").unwrap();
    let state = crate::NcDavState {
        pool,
        file_io_permits: Arc::new(tokio::sync::Semaphore::new(4)),
        mime_cache: Arc::new(RwLock::new(MimeCache::default())),
        appconfig_cache: Arc::new(RwLock::new(AppConfigCache::default())),
        table_prefix: prefix,
        data_directory: data_dir,
        instance_id: Arc::new("testinst".to_string()),
        filename_validator: Arc::new(FilenameValidator::from_config(&cfg)),
        base_url: Arc::new(String::new()),
        upload_state_store: Arc::new(UploadStateStore::new()),
        preview_registry: Arc::new(ProviderRegistry::build(
            false,
            None,
            false,
            false,
            false,
            &[],
        )),
        dir_mime_id: 1,
        dir_mimepart_id: 1,
        storage_cache: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        lazy_cache_ensured: Arc::new(std::sync::Mutex::new(std::collections::HashSet::new())),
        media_mtime_ctime_fallback: true,
    };
    let write_result: SharedWriteResult = Arc::new(std::sync::Mutex::new(None::<WriteResult>));
    let put_error: crate::SharedPutError = Arc::new(std::sync::Mutex::new(None));
    NcFileSystem::new(
        state,
        "admin".to_string(),
        storage_id,
        None,
        None,
        write_result,
        put_error,
        false,
    )
}

pub(crate) async fn fc_row(
    pool: &DbPool,
    prefix: &str,
    path: &str,
) -> Option<(i64, i64, i64, i64, i64, String)> {
    // (fileid, size, mtime, storage_mtime, parent, path)
    let hash = row::path_hash(path);
    let sql = format!(
        "SELECT fileid, size, mtime, storage_mtime, parent, path FROM {prefix}filecache \
         WHERE storage = $1 AND path_hash = $2",
        prefix = prefix
    );
    sqlx::query::<Sqlite>(&sql)
        .bind(1i64)
        .bind(&hash)
        .fetch_optional(test_pool(pool))
        .await
        .ok()?
        .map(|r| {
            (
                r.get::<i64, _>("fileid"),
                r.get::<i64, _>("size"),
                r.get::<i64, _>("mtime"),
                r.get::<i64, _>("storage_mtime"),
                r.get::<i64, _>("parent"),
                r.get::<String, _>("path"),
            )
        })
}

pub(crate) async fn extended_count(pool: &DbPool) -> i64 {
    let sql = "SELECT COUNT(*) FROM oc_filecache_extended";
    sqlx::query_scalar::<Sqlite, i64>(sql)
        .fetch_one(test_pool(pool))
        .await
        .unwrap()
}

pub(crate) async fn etag_of(pool: &DbPool, prefix: &str, path: &str) -> Option<String> {
    let hash = row::path_hash(path);
    let sql = format!(
        "SELECT etag FROM {prefix}filecache WHERE storage = $1 AND path_hash = $2",
        prefix = prefix
    );
    sqlx::query_scalar::<Sqlite, String>(&sql)
        .bind(1i64)
        .bind(&hash)
        .fetch_optional(test_pool(pool))
        .await
        .unwrap()
}
