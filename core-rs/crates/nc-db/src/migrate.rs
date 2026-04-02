use sqlx::AnyPool;

/// Schema migration entry point.
///
/// # DISABLED — see SPECS/IMPROVEMENTS.md §I.3
///
/// `sqlx::migrate!()` tracks applied migrations in `_sqlx_migrations`
/// independently of PHP's Doctrine migration tracker (`oc_migrations`).
/// Running both against the same DB after a `occ upgrade` risks silent schema
/// divergence or double-application of conflicting changes.
///
/// The deployment model is "PHP installs and upgrades, Rust serves" — PHP owns
/// the schema entirely. Rust migrations are therefore disabled until replaced
/// with a schema validation check (verify expected tables/columns exist, bail
/// with a clear error if not).
///
/// TO RE-ENABLE: uncomment the `sqlx::migrate!()` block below and delete this
/// comment. Only appropriate for a future "Rust-only install" path where PHP
/// is never involved in schema management.
pub async fn run(pool: &AnyPool) -> anyhow::Result<()> {
    // ── DISABLED: see doc comment above ──────────────────────────────────────
    // sqlx::migrate!("../../migrations")
    //     .run(pool)
    //     .await
    //     .map_err(|e| anyhow::anyhow!("Migration failed: {e}"))?;
    // tracing::info!("Migrations complete");

    let _ = pool; // suppress unused warning while migrations are disabled
    tracing::debug!(
        "Database migrations disabled (PHP owns the schema — see SPECS/IMPROVEMENTS.md §I.3)"
    );
    Ok(())
}
