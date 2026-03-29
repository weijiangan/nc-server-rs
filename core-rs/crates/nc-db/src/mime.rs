use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use sqlx::AnyPool;

/// Process-lifetime cache of `oc_mimetypes`.
///
/// Populated once at startup, invalidated and rebuilt on any insert.
/// All PROPFIND rows resolve MIME types from this cache — no per-row
/// JOIN to `oc_mimetypes` is needed.
#[derive(Debug, Default, Clone)]
pub struct MimeCache {
    /// mimetype string → numeric id
    by_name: HashMap<String, i64>,
    /// numeric id → mimetype string
    by_id: HashMap<i64, String>,
}

pub type SharedMimeCache = Arc<RwLock<MimeCache>>;

impl MimeCache {
    pub fn get_id(&self, mime: &str) -> Option<i64> {
        self.by_name.get(mime).copied()
    }

    pub fn get_name(&self, id: i64) -> Option<&str> {
        self.by_id.get(&id).map(String::as_str)
    }
}

/// Load `oc_mimetypes` from the DB and return a shared, writable cache.
pub async fn load_mime_cache(
    pool: &AnyPool,
    table_prefix: &str,
) -> anyhow::Result<SharedMimeCache> {
    let table = format!("{table_prefix}mimetypes");
    let rows: Vec<(i64, String)> =
        sqlx::query_as(&format!("SELECT id, mimetype FROM {table}"))
            .fetch_all(pool)
            .await?;

    let mut cache = MimeCache::default();
    for (id, mime) in rows {
        cache.by_name.insert(mime.clone(), id);
        cache.by_id.insert(id, mime);
    }

    tracing::info!(count = cache.by_name.len(), "Mime-type cache loaded");
    Ok(Arc::new(RwLock::new(cache)))
}

/// Rebuild the mime-type cache in place.
///
/// Called after any insert into `oc_mimetypes`.
pub async fn refresh_mime_cache(
    pool: &AnyPool,
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
            c.by_id.insert(*id, mime.to_string());
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
        assert_eq!(guard.get_name(1), Some("application/octet-stream"));
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
