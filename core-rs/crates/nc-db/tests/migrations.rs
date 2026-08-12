/// Migration integration tests.
///
/// These run against an in-memory SQLite database and verify that:
/// - All migrations apply cleanly to an empty DB
/// - All expected tables exist with the correct columns after migration
/// - Re-running migrations on an already-migrated DB is a no-op

#[cfg(test)]
mod tests {
    use nc_db::pool::DbPool;
    use sqlx::Row;

    async fn fresh_db() -> DbPool {
        let pool = DbPool::Sqlite(
            sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect("sqlite::memory:")
                .await
                .expect("in-memory SQLite failed"),
        );

        nc_db::migrate::run(&pool)
            .await
            .expect("migrations failed on fresh DB");

        pool
    }

    /// Check that a table exists by querying it.
    async fn table_exists(pool: &DbPool, table: &str) -> bool {
        let sql = format!("SELECT name FROM sqlite_master WHERE type='table' AND name='{table}'");
        let row = sqlx::query(&sql).fetch_optional(pool).await.unwrap();
        row.is_some()
    }

    #[tokio::test]
    async fn all_tables_created_on_fresh_db() {
        let pool = fresh_db().await;

        let expected_tables = [
            "oc_mimetypes",
            "oc_storages",
            "oc_filecache",
            "oc_filecache_extended",
            "oc_files_metadata",
            "oc_users",
            "oc_accounts",
            "oc_accounts_data",
            "oc_groups",
            "oc_group_user",
            "oc_authtoken",
            "oc_bruteforce_attempts",
            "oc_twofactor_providers",
            "oc_appconfig",
            "oc_preferences",
            "oc_properties",
            "oc_share",
            "oc_share_external",
        ];

        for table in &expected_tables {
            assert!(
                table_exists(&pool, table).await,
                "Expected table '{table}' was not created by migrations"
            );
        }
    }

    #[tokio::test]
    async fn migrations_are_idempotent() {
        let pool = fresh_db().await;

        // Running migrations a second time must be a no-op, not an error.
        nc_db::migrate::run(&pool)
            .await
            .expect("second migration run should be a no-op");
    }

    #[tokio::test]
    async fn oc_appconfig_accepts_insert() {
        let pool = fresh_db().await;

        sqlx::query(
            "INSERT INTO oc_appconfig (appid, configkey, configvalue, type, lazy)
             VALUES ('core', 'maintenance', '0', 8, 0)",
        )
        .execute(&pool)
        .await
        .expect("insert into oc_appconfig failed");

        let row = sqlx::query(
            "SELECT configvalue FROM oc_appconfig WHERE appid='core' AND configkey='maintenance'",
        )
        .fetch_one(&pool)
        .await
        .expect("row not found");

        let val: Option<String> = row.try_get("configvalue").ok();
        assert_eq!(val.as_deref(), Some("0"));
    }

    #[tokio::test]
    async fn oc_filecache_has_required_columns() {
        let pool = fresh_db().await;

        // PRAGMA table_info returns one row per column.
        let rows = sqlx::query("PRAGMA table_info(oc_filecache)")
            .fetch_all(&pool)
            .await
            .expect("PRAGMA failed");

        let col_names: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap())
            .collect();

        for expected in &[
            "fileid",
            "storage",
            "path",
            "path_hash",
            "parent",
            "name",
            "mimetype",
            "mimepart",
            "size",
            "mtime",
            "etag",
            "permissions",
            "checksum",
            "creation_time",
            "upload_time",
        ] {
            assert!(
                col_names.contains(&expected.to_string()),
                "oc_filecache missing column '{expected}'"
            );
        }
    }

    #[tokio::test]
    async fn oc_authtoken_has_required_columns() {
        let pool = fresh_db().await;

        let rows = sqlx::query("PRAGMA table_info(oc_authtoken)")
            .fetch_all(&pool)
            .await
            .expect("PRAGMA failed");

        let col_names: Vec<String> = rows
            .iter()
            .map(|r| r.try_get::<String, _>("name").unwrap())
            .collect();

        for expected in &[
            "id",
            "uid",
            "login_name",
            "token",
            "type",
            "last_activity",
            "last_check",
            "scope",
            "expires",
        ] {
            assert!(
                col_names.contains(&expected.to_string()),
                "oc_authtoken missing column '{expected}'"
            );
        }
    }
}
