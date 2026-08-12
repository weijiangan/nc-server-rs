#![forbid(unsafe_code)]

mod client_identity;
mod handlers;
mod middleware;
mod preview;
mod preview_gen;
mod router;
mod state;

use std::path::PathBuf;

use anyhow::Context;
use clap::Parser;

use state::AppState;

#[derive(Parser, Debug)]
#[command(name = "nc-server", about = "Nextcloud core+files Rust server")]
struct Args {
    /// Path to the Nextcloud installation root (contains config/config.php).
    /// Defaults to the current working directory.
    #[arg(long, default_value = ".")]
    root: PathBuf,

    /// Bind address.
    #[arg(long, default_value = "0.0.0.0:7000")]
    listen: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // ── Tracing ──────────────────────────────────────────────────────────────
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    // ── Config ───────────────────────────────────────────────────────────────
    let config =
        nc_db::NcConfig::load(&args.root).context("Failed to load Nextcloud configuration")?;
    tracing::info!(dbtype = ?config.dbtype, "Configuration loaded");

    // ── Database pool ────────────────────────────────────────────────────────
    let pool = nc_db::pool::build_pool(&config)
        .await
        .context("Failed to build database pool")?;

    // ── Migrations ───────────────────────────────────────────────────────────
    // Currently a no-op: sqlx migrations are disabled because PHP owns the
    // schema (see SPECS/02-specifications/improvements.md §I.3 and nc_db::migrate::run docs).
    nc_db::migrate::run(&pool)
        .await
        .context("Database migration failed")?;

    // ── Startup caches ───────────────────────────────────────────────────────
    let prefix = &config.dbtableprefix;
    let table_prefix = config.dbtableprefix.clone(); // saved before config is moved into Arc
    let mime_cache = nc_db::mime::load_mime_cache(&pool, prefix)
        .await
        .context("Failed to load mime-type cache")?;

    // ── Phase 21 S3: hoisted static lookups ─────────────────────────────────
    // Resolve the directory mimetype/mimepart ids once so the DAV read path
    // (read_dir, get_props, open) never re-looks them up per request.  The
    // mime cache is warm from `load_mime_cache`; the one-time INSERT runs
    // only if the rows are missing.
    let dir_mime_id = nc_db::mime::get_or_insert_mime_id(
        &pool,
        &table_prefix,
        &mime_cache,
        "httpd/unix-directory",
    )
    .await;
    let dir_mimepart_id =
        nc_db::mime::get_or_insert_mime_id(&pool, &table_prefix, &mime_cache, "httpd").await;
    let storage_cache: nc_dav::SharedStorageCache =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashMap::new()));
    let lazy_cache_ensured: std::sync::Arc<std::sync::Mutex<std::collections::HashSet<i64>>> =
        std::sync::Arc::new(std::sync::Mutex::new(std::collections::HashSet::new()));

    let appconfig_cache = nc_db::appconfig::load_appconfig_cache(&pool, prefix)
        .await
        .context("Failed to load app config cache")?;
    let capability_cache =
        nc_ocs::load_capability_cache(&appconfig_cache, config.version.as_deref());
    let token_cache = nc_auth::new_token_cache();

    {
        let ac = appconfig_cache.read().expect("appconfig cache lock");
        if ac.is_maintenance() {
            tracing::warn!("Server starting in MAINTENANCE MODE");
        }
    }

    // ── FastCGI state ────────────────────────────────────────────────────────
    // Build FastCGI state from config (None when fastcgi_socket is absent).
    let fastcgi = nc_fastcgi::FastCgiState::from_config(&config, &args.root);
    if let Some(ref fpm) = fastcgi {
        tracing::info!(
            socket = %fpm.socket_path.display(),
            shim = %fpm.shim_path.display(),
            "PHP-FPM proxy enabled"
        );
    } else {
        tracing::info!("PHP-FPM proxy disabled (fastcgi_socket not set in config)");
    }

    // ── Route registry (Phase 7.5) ───────────────────────────────────────────
    // Scan apps/*/appinfo/routes.php to build the list of URL prefixes that
    // need to be registered as PHP-FPM-proxied routes.  This replaces the
    // static `/apps/{*path}` catch-all with explicit per-app entries.
    let php_routes = nc_fastcgi::build_route_registry(&args.root);

    // ── Phase 7.7: Merge PHP-app capabilities ────────────────────────────────
    // After FastCGI state is ready, fetch the PHP-app capability block
    // (`files_sharing`, `text`, etc.) by making one synthetic OCS request to
    // PHP-FPM with an admin identity, then shallow-merge it into the native
    // capability cache so the `/ocs/.../cloud/capabilities` response is
    // complete before the first client request arrives.
    if let Some(ref fpm) = fastcgi {
        let admin_uid: Option<String> = sqlx::query_scalar(&format!(
            "SELECT uid FROM {prefix}group_user WHERE gid = 'admin' LIMIT 1",
            prefix = &table_prefix
        ))
        .fetch_optional(&pool)
        .await
        .ok()
        .flatten();

        match admin_uid {
            Some(ref uid) => {
                tracing::info!(uid = %uid, "Fetching PHP-app capabilities from PHP-FPM");
                match nc_fastcgi::fetch_php_capabilities(fpm, uid).await {
                    Some(php_caps) => {
                        capability_cache
                            .write()
                            .expect("capability cache write lock")
                            .apply_php_capabilities(php_caps);
                        tracing::info!("PHP-app capabilities merged into capability cache");
                    }
                    None => {
                        tracing::warn!(
                            "PHP-app capabilities fetch failed; \
                             serving native-only authenticated capabilities"
                        );
                    }
                }

                // Fetch the IPublicCapability-only subset for unauthenticated requests.
                // The PHP shim whitelists /cloud/capabilities so the guard passes
                // without HTTP_X_NC_USER; PHP sees no session and calls
                // getCapabilities(true) naturally.
                match nc_fastcgi::fetch_php_public_capabilities(fpm).await {
                    Some(php_pub_caps) => {
                        capability_cache
                            .write()
                            .expect("capability cache write lock")
                            .apply_php_public_capabilities(php_pub_caps);
                        tracing::info!("PHP-app public (IPublicCapability) capabilities merged");
                    }
                    None => {
                        tracing::warn!(
                            "PHP-app public capabilities fetch failed; \
                             public capabilities will be native-only"
                        );
                    }
                }
            }
            None => {
                tracing::warn!(
                    "No admin user found in {prefix}group_user; \
                     skipping PHP-app capabilities fetch",
                    prefix = &table_prefix
                );
            }
        }
    }

    // Resolve instanceid before config is moved into the Arc (§7.9.1).
    // On an installed Nextcloud this is always present; the empty-string
    // fallback is safe for pre-install / test states where no session
    // cookie matching is needed.
    let instanceid = config.instanceid.clone().unwrap_or_default();

    // ── Phase 7.9.5: Session identity cache ───────────────────────────────────
    // Only allocated when PHP-FPM is configured — without a FastCGI backend
    // there is no `__session_resolve` to call and no session cookies to cache.
    let session_cache: Option<nc_auth::SharedSessionCache> = if fastcgi.is_some() {
        Some(nc_auth::new_session_cache())
    } else {
        None
    };

    // ── Phase 5.5: Upload state store (chunked upload v2) ───────────────────
    // Created regardless of distributed cache configuration - serves as the
    // fallback store when Redis/Memcached is not available.
    let upload_state_store: nc_dav::SharedUploadStateStore =
        std::sync::Arc::new(nc_dav::UploadStateStore::new());

    // ── Phase 11.1: preview provider registry ────────────────────────────────
    // Resolve enabledPreviewProviders gating + binary/Imaginary availability once
    // at startup (includes a one-time $PATH search for ffmpeg/LibreOffice).
    let preview_registry = std::sync::Arc::new(nc_dav::ProviderRegistry::from_config(&config));

    // ── Phase 11.4/11.5: native preview generation service ──────────────────
    // Builds the (optional) Imaginary backend, snowflake generator, admission
    // semaphore and in-flight coalescer once at startup.  When Imaginary is not
    // configured+gated the backend is `None` and misses keep proxying to PHP-FPM
    // (hit-serving stays on regardless).
    let preview_gen =
        preview_gen::PreviewGen::from_config(&config, &appconfig_cache, &preview_registry);

    let state = AppState {
        pool,
        mime_cache,
        appconfig_cache,
        capability_cache,
        token_cache,
        nc_config: std::sync::Arc::new(config),
        nc_root: args.root.clone(),
        table_prefix,
        fastcgi,
        instanceid,
        session_cache,
        upload_state_store,
        preview_registry,
        preview_gen,
        dir_mime_id,
        dir_mimepart_id,
        storage_cache,
        lazy_cache_ensured,
    };

    // ── Phase 7.7: Background capability refresh ──────────────────────────────
    // Spawn a background task that wakes every 30 seconds to reload appconfig
    // from DB and rebuild the capability payload.  This picks up any
    // `oc_appconfig` writes that went through PHP-FPM since startup so that
    // capabilities stay fresh without requiring a server restart.
    spawn_capability_refresh_task(
        state.pool.clone(),
        state.table_prefix.clone(),
        state.appconfig_cache.clone(),
        state.capability_cache.clone(),
        state.fastcgi.clone(),
    );

    // ── Phase 7.9.5: Session cache eviction task ──────────────────────────────
    // Periodically remove expired entries so the map doesn't grow unboundedly.
    if let Some(ref sc) = state.session_cache {
        spawn_session_cache_eviction_task(sc.clone());
    }

    // ── Phase 17: CPU profiling on SIGUSR2 (env-gated) ────────────────────────
    // When NC_PROFILE_DIR is set, a SIGUSR2 samples the process for
    // NC_PROFILE_SECS (default 10) at 1000 Hz and writes a flamegraph SVG +
    // a pprof protobuf (`.pb`) into the dir.  Unset (production) → no-op.
    spawn_profile_dump_task();

    // ── Router ───────────────────────────────────────────────────────────────
    let app = router::build(state, php_routes);

    // ── Listener ─────────────────────────────────────────────────────────────
    let listener = tokio::net::TcpListener::bind(&args.listen)
        .await
        .with_context(|| format!("Failed to bind to {}", args.listen))?;

    tracing::info!(listen = %args.listen, "HTTP listener ready");

    // ── Graceful shutdown on SIGTERM ─────────────────────────────────────────
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await
    .context("HTTP server error")?;

    tracing::info!("Server shut down cleanly");
    Ok(())
}

/// Spawn a background task that periodically refreshes the capability cache
/// (Phase 7.7 — "refresh on `oc_appconfig` writes").
///
/// All `oc_appconfig` writes during normal operation go through PHP-FPM, so
/// the Rust process cannot intercept them inline.  Instead, this task wakes
/// task that wakes every 30 seconds, reloads the whole `oc_appconfig` table from the database,
/// rebuilds the native capability payload, and — if PHP-FPM is configured —
/// re-fetches the PHP-app capability block from PHP-FPM and merges it.  This
/// ensures the served `/ocs/…/cloud/capabilities` response reflects config
/// changes (e.g. enabling an app, changing quota defaults, updating forbidden
/// filename lists) within one refresh interval.
fn spawn_capability_refresh_task(
    pool: nc_db::pool::DbPool,
    table_prefix: String,
    appconfig_cache: nc_db::appconfig::SharedAppConfigCache,
    capability_cache: nc_ocs::SharedCapabilityCache,
    fastcgi: Option<nc_fastcgi::FastCgiState>,
) {
    const INTERVAL: std::time::Duration = std::time::Duration::from_secs(30); // 30 seconds

    tokio::spawn(async move {
        loop {
            tokio::time::sleep(INTERVAL).await;

            // 1. Reload appconfig from DB so PHP-FPM writes are visible.
            if let Err(e) =
                nc_db::appconfig::reload_appconfig_cache(&pool, &table_prefix, &appconfig_cache)
                    .await
            {
                tracing::warn!(
                    error = %e,
                    "capability-refresh: failed to reload appconfig cache; skipping cycle"
                );
                continue;
            }

            // 2. Rebuild native capability payload from fresh appconfig,
            //    re-merging existing PHP-app capabilities so they are not lost.
            nc_ocs::handlers::rebuild_capability_cache(&appconfig_cache, &capability_cache).await;

            // 3. If PHP-FPM is available, re-fetch the PHP-app capability block.
            if let Some(ref fpm) = fastcgi {
                // Re-query admin UID in case group membership changed since startup.
                let admin_uid: Option<String> = sqlx::query_scalar(&format!(
                    "SELECT uid FROM {p}group_user WHERE gid = 'admin' LIMIT 1",
                    p = &table_prefix
                ))
                .fetch_optional(&pool)
                .await
                .ok()
                .flatten();

                match admin_uid {
                    Some(ref uid) => {
                        match nc_fastcgi::fetch_php_capabilities(fpm, uid).await {
                            Some(php_caps) => {
                                capability_cache
                                    .write()
                                    .expect("capability cache write lock")
                                    .apply_php_capabilities(php_caps);
                                tracing::debug!("capability-refresh: PHP-app authenticated capabilities updated");
                            }
                            None => {
                                tracing::warn!(
                                    "capability-refresh: PHP-app capabilities fetch failed; \
                                     retaining existing cached PHP caps"
                                );
                            }
                        }
                    }
                    None => {
                        tracing::warn!(
                            "capability-refresh: no admin user found in {p}group_user; \
                             skipping PHP-app authenticated capabilities refresh",
                            p = &table_prefix
                        );
                    }
                }

                // The public (IPublicCapability-only) fetch is unauthenticated —
                // it does not require an admin UID and always runs independently.
                match nc_fastcgi::fetch_php_public_capabilities(fpm).await {
                    Some(php_pub_caps) => {
                        capability_cache
                            .write()
                            .expect("capability cache write lock")
                            .apply_php_public_capabilities(php_pub_caps);
                        tracing::debug!("capability-refresh: PHP-app public capabilities updated");
                    }
                    None => {
                        tracing::warn!(
                            "capability-refresh: PHP-app public capabilities fetch failed; \
                             retaining existing cached public caps"
                        );
                    }
                }
            }

            tracing::debug!("capability-refresh: cycle complete");
        }
    });
}

/// Spawn a background task that periodically evicts expired entries from the
/// session identity cache (Phase 7.9.5).
///
/// Runs every [`nc_auth::SESSION_CACHE_EVICT_INTERVAL`] (5 minutes).
/// Each eviction pass calls [`nc_auth::cache_evict_expired`] which removes all
/// entries whose `inserted_at` age exceeds [`nc_auth::SESSION_CACHE_TTL`] (60 s).
///
/// This prevents unbounded growth: a warm Nextcloud instance may accumulate one
/// entry per active browser session, and without periodic eviction those entries
/// would never be freed.
fn spawn_session_cache_eviction_task(cache: nc_auth::SharedSessionCache) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(nc_auth::SESSION_CACHE_EVICT_INTERVAL).await;
            let before = cache.len();
            nc_auth::cache_evict_expired(&cache);
            let after = cache.len();
            if before > after {
                tracing::debug!(
                    removed = before - after,
                    remaining = after,
                    "session-cache: evicted expired entries"
                );
            }
        }
    });
}

/// CPU-profiling on SIGUSR2 (Phase 17).  Installed only when `NC_PROFILE_DIR`
/// is set — production behavior is byte-identical without it.
///
/// The signal listener lives on the async runtime; the sampling window
/// (sleep + symbolication) runs on a blocking thread so the runtime stays
/// responsive to the (possibly concurrent) benchmark load.
///
/// Note: PID 1 in the dev container is `bootstrap.sh`, so the trigger is
/// `docker exec master-nextcloud-1 bash -c 'pkill -USR2 -x nc-server'`, not
/// `docker kill -s USR2`.
fn spawn_profile_dump_task() {
    let Some(dir) = std::env::var_os("NC_PROFILE_DIR") else {
        return;
    };
    let dir = std::path::PathBuf::from(dir);
    let secs = std::env::var("NC_PROFILE_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(10);

    tokio::spawn(async move {
        let Ok(mut sigusr2) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined2())
        else {
            tracing::warn!(dir = %dir.display(), "profiling: cannot install SIGUSR2 handler");
            return;
        };
        loop {
            sigusr2.recv().await;
            tracing::info!(
                dir = %dir.display(),
                secs,
                "profiling: starting {secs}s CPU profile at 1000 Hz (SIGUSR2)"
            );
            let guard = match pprof::ProfilerGuardBuilder::default()
                .frequency(1000)
                .blocklist(&["libc", "libgcc", "pthread", "vdso"])
                .build()
            {
                Ok(g) => g,
                Err(e) => {
                    tracing::error!(error = %e, "profiling: failed to start profiler");
                    continue;
                }
            };
            let dir = dir.clone();
            let res = tokio::task::spawn_blocking(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                let report = guard.report().build()?;
                let stamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let svg_path = dir.join(format!("profile-{stamp}.svg"));
                let pb_path = dir.join(format!("profile-{stamp}.pb"));
                report.flamegraph(std::fs::File::create(&svg_path)?)?;
                let profile = report.pprof()?;
                let mut buf = Vec::new();
                prost::Message::encode(&profile, &mut buf)?;
                std::fs::write(&pb_path, buf)?;
                anyhow::Ok((svg_path, pb_path))
            })
            .await;
            match res {
                Ok(Ok((svg_path, pb_path))) => {
                    tracing::info!(
                        svg = %svg_path.display(),
                        protobuf = %pb_path.display(),
                        "profiling: dump written"
                    );
                }
                Ok(Err(e)) => {
                    tracing::error!(error = %e, "profiling: failed to write dump");
                }
                Err(e) => {
                    tracing::error!(error = %e, "profiling: dump task panicked");
                }
            }
        }
    });
}

/// Resolves when SIGTERM or Ctrl-C is received.
/// Drains in-flight requests for up to 30 s (axum handles the timeout).
async fn shutdown_signal() {
    use tokio::signal;

    let ctrl_c = async {
        signal::ctrl_c()
            .await
            .expect("failed to install Ctrl-C handler");
    };

    #[cfg(unix)]
    let sigterm = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("failed to install SIGTERM handler")
            .recv()
            .await;
    };

    #[cfg(not(unix))]
    let sigterm = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => { tracing::info!("Received Ctrl-C, shutting down"); },
        _ = sigterm => { tracing::info!("Received SIGTERM, shutting down"); },
    }
}
