use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use crate::pool::DbPool;

/// Process-lifetime cache of `oc_mimetypes`.
///
/// Populated once at startup, invalidated and rebuilt on any insert.
/// All PROPFIND rows resolve MIME types from this cache — no per-row
/// JOIN to `oc_mimetypes` is needed.
#[derive(Debug, Default, Clone)]
pub struct MimeCache {
    /// mimetype string → numeric id
    by_name: HashMap<String, i64>,
    /// numeric id → mimetype string (Arc-shared so per-child PROPFIND
    /// metadata builds share one allocation — task 23.4).
    by_id: HashMap<i64, Arc<str>>,
}

pub type SharedMimeCache = Arc<RwLock<MimeCache>>;

impl MimeCache {
    pub fn get_id(&self, mime: &str) -> Option<i64> {
        self.by_name.get(mime).copied()
    }

    pub fn get_name(&self, id: i64) -> Option<Arc<str>> {
        self.by_id.get(&id).cloned()
    }

    /// Insert a mimetype → ID mapping into the in‑memory cache.
    ///
    /// Called after a successful DB insert so that future lookups hit
    /// the cache without a DB round‑trip.
    pub(crate) fn insert(&mut self, id: i64, mime: String) {
        self.by_name.insert(mime.clone(), id);
        self.by_id.insert(id, Arc::from(mime));
    }
}

/// Resolve a mimetype string to its `oc_mimetypes` ID, inserting it into the
/// database and updating the in‑memory cache if it isn't already present.
///
/// Mirrors PHP `IMimeTypeLoader::getId()` — loads the full table into memory
/// on first use and auto‑inserts any unknown type.
///
/// Returns the mimetype ID, or `1` (`application/octet-stream`) as a last‑resort
/// fallback when the DB is unreachable.
pub async fn get_or_insert_mime_id(
    pool: &DbPool,
    table_prefix: &str,
    cache: &SharedMimeCache,
    mime: &str,
) -> i64 {
    // ── Fast path: cache hit ──────────────────────────────────────────────
    {
        let guard = cache.read().expect("mime cache lock");
        if let Some(id) = guard.get_id(mime) {
            return id;
        }
    }

    // ── Slow path: insert into DB + update cache ──────────────────────────
    let table = format!("{table_prefix}mimetypes");

    // Try INSERT first.  The unique index on `mimetype` ensures we never
    // insert a duplicate; if a concurrent request already inserted it we
    // just fall through to the SELECT below.
    let insert_sql = format!("INSERT INTO {table} (mimetype) VALUES ($1)");
    let _ = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&insert_sql)
            .bind(mime)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&insert_sql)
            .bind(mime)
            .execute(p)
            .await
            .map(|_| ()),
    };

    // Read back the ID (ours or the concurrent winner's).
    let select_sql = format!("SELECT id FROM {table} WHERE mimetype = $1");
    let id: i64 = match pool {
        DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(&select_sql)
            .bind(mime)
            .fetch_optional(p)
            .await,
        DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(&select_sql)
            .bind(mime)
            .fetch_optional(p)
            .await,
    }
    .ok()
        .flatten()
        .unwrap_or(1_i64);

    // Update the in‑memory cache so future readers don't repeat the DB trip.
    {
        let mut guard = cache.write().expect("mime cache write lock");
        if guard.get_id(mime).is_none() {
            guard.insert(id, mime.to_string());
        }
    }

    id
}

/// Load `oc_mimetypes` from the DB and return a shared, writable cache.
pub async fn load_mime_cache(pool: &DbPool, table_prefix: &str) -> anyhow::Result<SharedMimeCache> {
    let table = format!("{table_prefix}mimetypes");
    let rows: Vec<(i64, String)> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<sqlx::Postgres, (i64, String)>(
            &format!("SELECT id, mimetype FROM {table}"),
        )
        .fetch_all(p)
        .await?,
        DbPool::Sqlite(p) => sqlx::query_as::<sqlx::Sqlite, (i64, String)>(
            &format!("SELECT id, mimetype FROM {table}"),
        )
        .fetch_all(p)
        .await?,
    };

    let mut cache = MimeCache::default();
    for (id, mime) in rows {
        cache.by_name.insert(mime.clone(), id);
        cache.by_id.insert(id, Arc::from(mime));
    }

    tracing::info!(count = cache.by_name.len(), "Mime-type cache loaded");
    Ok(Arc::new(RwLock::new(cache)))
}

/// Rebuild the mime-type cache in place.
///
/// Called after any insert into `oc_mimetypes`.
pub async fn refresh_mime_cache(
    pool: &DbPool,
    table_prefix: &str,
    cache: &SharedMimeCache,
) -> anyhow::Result<()> {
    let fresh = load_mime_cache(pool, table_prefix).await?;
    let fresh_inner = Arc::try_unwrap(fresh)
        .expect("fresh cache has single owner")
        .into_inner()
        .expect("no poisoning");

    let mut guard = cache.write().expect("mime cache write lock poisoned");
    *guard = fresh_inner;
    tracing::debug!("Mime-type cache refreshed");
    Ok(())
}

// ── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_cache(pairs: &[(i64, &str)]) -> SharedMimeCache {
        let mut c = MimeCache::default();
        for (id, mime) in pairs {
            c.by_name.insert(mime.to_string(), *id);
            c.by_id.insert(*id, Arc::from(*mime));
        }
        Arc::new(RwLock::new(c))
    }

    #[test]
    fn lookup_by_name() {
        let cache = make_cache(&[
            (1, "application/octet-stream"),
            (2, "image/jpeg"),
            (3, "text/plain"),
        ]);
        let guard = cache.read().unwrap();
        assert_eq!(guard.get_id("image/jpeg"), Some(2));
        assert_eq!(guard.get_id("video/mp4"), None);
    }

    #[test]
    fn lookup_by_id() {
        let cache = make_cache(&[(1, "application/octet-stream"), (2, "image/jpeg")]);
        let guard = cache.read().unwrap();
        assert_eq!(guard.get_name(1).as_deref(), Some("application/octet-stream"));
        assert_eq!(guard.get_name(99), None);
    }

    #[test]
    fn concurrent_reads_do_not_block_each_other() {
        use std::thread;
        let cache = make_cache(&[(1, "text/html")]);
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let c = Arc::clone(&cache);
                thread::spawn(move || {
                    let guard = c.read().unwrap();
                    assert_eq!(guard.get_id("text/html"), Some(1));
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
    }
}
