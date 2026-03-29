use sqlx::AnyPool;

/// Run all pending migrations from the `migrations/` directory.
///
/// - On a fresh DB: creates all tables from scratch.
/// - On an existing Nextcloud DB: detects applied versions via the
///   `_sqlx_migrations` table and skips them. Only new files are applied.
/// - Migrations are additive-only — no DROP COLUMN, no destructive ALTER.
///
/// This must be called at startup, before the HTTP listener opens and
/// before the startup caches are populated.
pub async fn run(pool: &AnyPool) -> anyhow::Result<()> {
    tracing::info!("Running database migrations…");

    sqlx::migrate!("../../migrations")
        .run(pool)
        .await
        .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;

    tracing::info!("Migrations complete");
    Ok(())
}
