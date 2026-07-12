//! Chunked upload v2 state management.
//!
//! This module provides an in-process store for chunked upload v2 metadata.
//! When a distributed cache is not configured, this serves as a fallback
//! to enable basic chunked upload functionality.
//!
//! Per PHASE-5.5: Store `(upload_id, target_path)` in distributed cache or
//! local in-process store. If no distributed cache is configured, proceed with
//! in-process map.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Metadata for a chunked upload v2 session.
#[derive(Debug, Clone)]
pub struct UploadMetadata {
    /// Unique upload identifier (upload_id)
    pub upload_id: String,
    /// Target path where the file will be assembled (e.g., `files/user/file.txt`)
    pub target_path: String,
    /// Target file ID (if already exists)
    pub target_id: Option<i64>,
    /// Map of part_id -> size (in bytes)
    pub chunks: HashMap<i64, u64>,
    /// Total size from OC-Total-Length header (if provided)
    pub expected_size: Option<u64>,
}

/// In-process store for upload session metadata.
///
/// This serves as a fallback when no distributed cache (Redis/Memcached) is
/// configured. The store holds upload metadata keyed by upload_id.
#[derive(Clone)]
pub struct UploadStateStore {
    inner: Arc<RwLock<HashMap<String, UploadMetadata>>>,
}

impl Default for UploadStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl UploadStateStore {
    /// Create a new empty upload state store.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Store upload metadata for a new session.
    ///
    /// Called on MKCOL to create an upload slot.
    pub async fn create_session(
        &self,
        upload_id: &str,
        target_path: String,
        target_id: Option<i64>,
    ) {
        let mut guard = self.inner.write().await;
        guard.insert(
            upload_id.to_string(),
            UploadMetadata {
                upload_id: upload_id.to_string(),
                target_path,
                target_id,
                chunks: HashMap::new(),
                expected_size: None,
            },
        );
    }

    /// Add chunk info to an existing session.
    pub async fn add_chunk(&self, upload_id: &str, part_id: i64, size: u64) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(meta) = guard.get_mut(upload_id) {
            meta.chunks.insert(part_id, size);
            true
        } else {
            false
        }
    }

    /// Get total size of all uploaded chunks.
    pub async fn get_total_chunk_size(&self, upload_id: &str) -> Option<u64> {
        let guard = self.inner.read().await;
        guard.get(upload_id).map(|m| m.chunks.values().sum())
    }

    /// Get the list of part IDs sorted in ascending order.
    pub async fn get_sorted_part_ids(&self, upload_id: &str) -> Option<Vec<i64>> {
        let guard = self.inner.read().await;
        guard.get(upload_id).map(|m| {
            let mut ids: Vec<i64> = m.chunks.keys().copied().collect();
            ids.sort();
            ids
        })
    }

    /// Set the expected total size (from OC-Total-Length header).
    pub async fn set_expected_size(&self, upload_id: &str, size: u64) -> bool {
        let mut guard = self.inner.write().await;
        if let Some(meta) = guard.get_mut(upload_id) {
            meta.expected_size = Some(size);
            true
        } else {
            false
        }
    }

    /// Retrieve upload metadata for an existing session.
    ///
    /// Returns `None` if the upload session does not exist or has expired.
    pub async fn get_session(&self, upload_id: &str) -> Option<UploadMetadata> {
        let guard = self.inner.read().await;
        guard.get(upload_id).cloned()
    }

    /// Remove an upload session (called on DELETE to abort).
    pub async fn remove_session(&self, upload_id: &str) -> Option<UploadMetadata> {
        let mut guard = self.inner.write().await;
        guard.remove(upload_id)
    }

    /// Check if a session exists.
    pub async fn session_exists(&self, upload_id: &str) -> bool {
        let guard = self.inner.read().await;
        guard.contains_key(upload_id)
    }

    /// Get the count of active sessions (for debugging/monitoring).
    pub async fn session_count(&self) -> usize {
        let guard = self.inner.read().await;
        guard.len()
    }
}

/// Shared reference to the upload state store.
pub type SharedUploadStateStore = Arc<UploadStateStore>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_create_and_get_session() {
        let store = UploadStateStore::new();

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), Some(42))
            .await;

        let meta = store.get_session("upload123").await;
        assert!(meta.is_some());
        assert_eq!(meta.unwrap().target_path, "/files/user/test.txt");
    }

    #[tokio::test]
    async fn test_remove_session() {
        let store = UploadStateStore::new();

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), None)
            .await;
        let removed = store.remove_session("upload123").await;
        assert!(removed.is_some());

        let meta = store.get_session("upload123").await;
        assert!(meta.is_none());
    }

    #[tokio::test]
    async fn test_session_exists() {
        let store = UploadStateStore::new();

        assert!(!store.session_exists("upload123").await);

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), None)
            .await;

        assert!(store.session_exists("upload123").await);
    }

    #[tokio::test]
    async fn test_add_chunk() {
        let store = UploadStateStore::new();

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), None)
            .await;

        assert!(store.add_chunk("upload123", 1, 1000).await);
        assert!(store.add_chunk("upload123", 2, 2000).await);
        assert!(store.add_chunk("upload123", 3, 3000).await);

        let total = store.get_total_chunk_size("upload123").await;
        assert_eq!(total, Some(6000));
    }

    #[tokio::test]
    async fn test_get_sorted_part_ids() {
        let store = UploadStateStore::new();

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), None)
            .await;

        store.add_chunk("upload123", 3, 3000).await;
        store.add_chunk("upload123", 1, 1000).await;
        store.add_chunk("upload123", 2, 2000).await;

        let ids = store.get_sorted_part_ids("upload123").await;
        assert_eq!(ids, Some(vec![1, 2, 3]));
    }

    #[tokio::test]
    async fn test_set_expected_size() {
        let store = UploadStateStore::new();

        store
            .create_session("upload123", "/files/user/test.txt".to_string(), None)
            .await;

        assert!(store.set_expected_size("upload123", 10000).await);

        let meta = store.get_session("upload123").await;
        assert_eq!(meta.unwrap().expected_size, Some(10000));
    }

    #[tokio::test]
    async fn test_add_chunk_nonexistent_session() {
        let store = UploadStateStore::new();
        assert!(!store.add_chunk("nonexistent", 1, 1000).await);
    }

    #[tokio::test]
    async fn test_get_total_chunk_size_nonexistent() {
        let store = UploadStateStore::new();
        assert_eq!(store.get_total_chunk_size("nonexistent").await, None);
    }

    #[tokio::test]
    async fn test_get_sorted_part_ids_nonexistent() {
        let store = UploadStateStore::new();
        assert_eq!(store.get_sorted_part_ids("nonexistent").await, None);
    }
}
