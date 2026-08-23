//! PROPFIND — the batched read path.
//!
//! A depth-1 PROPFIND visits the target once and every child twice (`read_dir`
//! then `get_props`).  Issuing the per-node lookups naively costs ~11 queries
//! per child, so [`NcFileSystem::read_dir_batched`] fetches every family the
//! property writer needs in a handful of batched queries (one CTE on
//! Postgres) and parks them in [`PropfindBatch`]; [`NcFileSystem::collect_props`]
//! then reads the cache instead of re-querying.  Nodes outside the batch —
//! the depth-0 root, which dav-server-rs visits before `read_dir` — fall back
//! to the single-row queries.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use dav_server::fs::{DavDirEntry, DavProp, FsError, FsStream};
use futures::stream;

use crate::cache_rows::ensure_lazy_cache_row;
use crate::metadata::{NcDirEntry, NcMetaData};
use crate::path_utils::percent_encode_path;
use crate::row;
use crate::NcFileSystem;
use nc_db::now_secs;

/// Read a value from the per-request batch, or `None` when the node is
/// outside the batch (caller falls back to the single-row query).
pub(crate) fn batch_get<K, V, Q>(
    inner: &PropfindBatchInner,
    map: impl Fn(&PropfindBatchInner) -> &HashMap<K, V>,
    key: &Q,
) -> Option<V>
where
    K: Eq + std::hash::Hash + std::borrow::Borrow<Q> + Clone,
    Q: Eq + std::hash::Hash + ?Sized,
    V: Clone,
{
    map(inner).get(key).cloned()
}

/// Is `key` part of the `read_dir` batch?  If yes, a map miss means "no data"
/// and `get_props` must NOT fall back to a single-row query; if no, the node
/// is outside the batch (depth-0 root) and the single-row query is the only
/// source.
fn batch_contains<K, Q>(
    inner: &PropfindBatchInner,
    map: impl Fn(&PropfindBatchInner) -> &std::collections::HashSet<K>,
    key: &Q,
) -> bool
where
    K: Eq + std::hash::Hash + std::borrow::Borrow<Q>,
    Q: Eq + std::hash::Hash + ?Sized,
{
    map(inner).contains(key)
}

/// Per-request cache of everything depth-1 PROPFIND needs per child.
///
/// PHASE-22 T8.2: one `Arc<Mutex<PropfindBatchInner>>` instead of nine
/// per-map mutexes — the maps were only ever touched one at a time, so the
/// consolidation removes eight lock acquisitions per access without holding
/// any lock across an await (every read clones its value out first).
///
/// The `Arc` is required because dav-server-rs clones the filesystem per
/// resource (`PropWriter`), and the clones must share one cache — a plain
/// `Mutex<…>` would be snapshotted at every clone and the batch would never
/// reach the consumers.
///
/// Populated only by `read_dir` (a pure read path) and the per-request
/// uid-only lookups in `get_props`; write requests never run `read_dir`, so
/// no stale-row risk exists within a request.
#[derive(Clone, Default)]
pub(crate) struct PropfindBatch {
    pub(crate) inner: Arc<Mutex<PropfindBatchInner>>,
}

/// The maps behind [`PropfindBatch::inner`] (see the struct doc).
#[derive(Default)]
pub(crate) struct PropfindBatchInner {
    /// The fileids `read_dir` batched.  `get_props` uses this to distinguish
    /// "child with no data" (in the set → map miss means empty, no query)
    /// from "node outside the batch" (not in the set → single-row query).
    pub(crate) children: std::collections::HashSet<i64>,
    /// The fc paths `read_dir` batched (same role as `children`, for the
    /// path-keyed `oc_properties` lookup).
    pub(crate) child_paths: std::collections::HashSet<String>,
    /// `fc_path` → metadata, keyed trailing-slash-normalized.  Serves
    /// `load_meta` so `get_props` never re-fetches a row `read_dir` holds.
    /// Arc-shared (task 23.4) — read_dir deep-clones each meta once.
    pub(crate) meta: HashMap<String, Arc<NcMetaData>>,
    /// fileid → (dir_count, file_count) for `{nc:}contained-*-count`.
    pub(crate) dir_counts: HashMap<i64, (i64, i64)>,
    /// fileid → share rows for `{oc:}share-types` / `{nc:}sharees`.
    pub(crate) share_details: HashMap<i64, Vec<row::ShareDetail>>,
    /// fileid → most-recent non-empty share note.
    pub(crate) share_notes: HashMap<i64, String>,
    /// fileid → (count, unread) for `{oc:}comments-*`.
    pub(crate) comments: HashMap<i64, (i64, i64)>,
    /// fileid → system tags for `{nc:}system-tags`.
    /// fileid → parsed `oc_files_metadata.json` for `nc:metadata-*`.
    pub(crate) metadata: HashMap<i64, serde_json::Value>,
    pub(crate) system_tags: HashMap<i64, Vec<row::SystemTagRow>>,
    /// raw `fc_path` → custom properties from `oc_properties`.
    pub(crate) custom_props: HashMap<String, Vec<(String, String, i16)>>,
}

impl NcFileSystem {
    /// The depth-1 PROPFIND read path: list the children and prefetch
    /// everything `collect_props` needs for each of them (see module docs).
    pub(crate) async fn read_dir_batched(
        &self,
        path: &dav_server::davpath::DavPath,
    ) -> Result<FsStream<Box<dyn DavDirEntry>>, FsError> {
        let fc_path = self.to_fc_path(path);

        // Resolve the directory itself to get its fileid.  The request
        // root is usually already in the batch (load_meta store-on-miss
        // from `fs.metadata`/`get_props`), so reuse it instead of a
        // second lookup (round-3 Task 10).
        let dir_fileid = match {
            let inner = self
                .propfind_batch
                .inner
                .lock()
                .expect("propfind batch lock");
            batch_get(&inner, |i| &i.meta, fc_path.trim_end_matches('/'))
        } {
            Some(meta) => meta.fileid,
            None => {
                row::lookup_by_path(
                    &self.state.pool,
                    &self.state.table_prefix,
                    self.storage_id,
                    &fc_path,
                )
                .await
                .ok_or(FsError::NotFound)?
                .fileid
            }
        };

        // Fetch all direct children with their extended metadata — the
        // same single-query shape as PHP's
        // `Cache::getFolderContentsById` (`selectFileCache` +
        // `selectMetadata`, Cache.php:214).
        // Resolved once at startup (phase-21 S3) — the read path never
        // re-looks it up.
        let dir_mime_id = self.state.dir_mime_id;
        // ── T6.6: which families did the client ask for? ──────────────
        // Computed before the CTE so its sub-selects can be gated with
        // `CASE WHEN $N` bind flags (22.2-C — a skipped family's SubPlan
        // is never executed); the out-of-CTE statements gate on the same
        // values below.
        let oc_ns = "http://owncloud.org/ns";
        let nc_ns = "http://nextcloud.org/ns";
        let want_dir_counts = self.prop_requested(nc_ns, "contained-folder-count")
            || self.prop_requested(nc_ns, "contained-file-count");
        // T6.1 merged the share scan, so share-types/sharees/note share
        // one gate; T6.3 merged the comments query (count + unread).
        let want_shares = self.prop_requested(oc_ns, "share-types")
            || self.prop_requested(oc_ns, "sharees")
            || self.prop_requested(nc_ns, "note");
        let want_comments = self.prop_requested(oc_ns, "comments-count")
            || self.prop_requested(oc_ns, "comments-unread");
        let want_system_tags = self.prop_requested(nc_ns, "system-tags");
        let want_metadata = self.prop_requested_prefix(nc_ns, "metadata-");
        // §9.5 prefetch — gated in `get_props` too (same predicate), or
        // skipping the prefetch would re-introduce one query per child.
        let want_tags =
            self.prop_requested(oc_ns, "favorite") || self.prop_requested(oc_ns, "tags");
        // Custom props serve any prop outside the known server namespaces
        // (the same list `get_props`' custom-prop emission uses).
        let want_custom_props = match &self.requested_props {
            None => true,
            Some(list) => list.iter().any(|(n, _)| {
                !matches!(
                    n.as_deref(),
                    Some("DAV:")
                        | Some("http://owncloud.org/ns")
                        | Some("http://nextcloud.org/ns")
                        | Some("http://open-collaboration-services.org/ns")
                )
            }),
        };

        // PHASE-22 T7: on Postgres the whole child fan-out (listing +
        // dir counts + shares/notes + comments + system tags) is ONE
        // statement — the CTE's `kids` rows are exactly these children +
        // extended rows.  SQLite keeps the JOIN listing and the batched
        // families.
        let (children, extended_map, cte) = if self.state.pool.is_postgres() {
            let cte = row::propfind_batch_cte(
                &self.state.pool,
                &self.state.table_prefix,
                dir_fileid,
                self.storage_id,
                dir_mime_id,
                &self.uid,
                &row::PropfindGates {
                    dir_counts: want_dir_counts,
                    shares: want_shares,
                    comments: want_comments,
                    system_tags: want_system_tags,
                    tags: want_tags,
                    metadata: want_metadata,
                },
            )
            .await;
            let row::PropfindCte {
                children,
                extended,
                dir_counts,
                share_details,
                share_notes,
                comments,
                system_tags,
                tags,
                dir_tags,
                metadata,
            } = cte;
            let cte = Some((
                dir_counts,
                share_details,
                share_notes,
                comments,
                system_tags,
                tags,
                dir_tags,
                metadata,
            ));
            (children, extended, cte)
        } else {
            let (children, extended_map) = row::list_children_with_ext(
                &self.state.pool,
                &self.state.table_prefix,
                dir_fileid,
                self.storage_id,
            )
            .await;
            (children, extended_map, None)
        };

        // Phase-21 milestone fix: PHP lazily materializes the user's
        // `cache/` row on the first home-root read (fresh-install
        // stacks); the delete flow already replicates it, the read path
        // must too — once per storage per process, zero steady-state
        // statements.
        tracing::debug!(fc_path = %fc_path, storage_id = self.storage_id, "read_dir lazy-cache gate");
        if fc_path.trim_end_matches('/') == "files" {
            let now = now_secs();
            ensure_lazy_cache_row(&self.state, self.storage_id, now).await;
        }

        let child_ids: Vec<i64> = children.iter().map(|c| c.fileid).collect();
        // Only directory children can have children of their own (T6.2):
        // keep the dir-count batch's parent list to directories so files
        // never enter the IN list or the GROUP BY scan.
        let dir_child_ids: Vec<i64> = children
            .iter()
            .filter(|c| c.mimetype == dir_mime_id)
            .map(|c| c.fileid)
            .collect();
        let (metas, entries): (
            Vec<(String, Arc<NcMetaData>)>,
            Vec<Result<Box<dyn DavDirEntry>, FsError>>,
        ) = {
            let cache = self.state.mime_cache.read().expect("mime cache lock");
            let mut metas = Vec::with_capacity(children.len());
            let mut entries = Vec::with_capacity(children.len());
            for child in &children {
                let mime = cache
                    .get_name(child.mimetype)
                    .unwrap_or_else(|| Arc::from("application/octet-stream"));
                // One deep clone per child (task 23.4); metas, the batch
                // map and the entries share the Arc.
                let mut meta = Arc::new(NcMetaData::from_row(child, mime, None));
                // Apply extended times from the batch map (make_mut is
                // free here — the Arc is not yet shared).
                if let Some(ext) = extended_map.get(&child.fileid) {
                    Arc::make_mut(&mut meta).apply_extended(
                        ext.creation_time,
                        ext.upload_time,
                        ext.metadata_etag.clone(),
                    );
                }
                // fc path key — exactly what `load_meta`/`get_props`
                // look up (both normalize away trailing slashes).
                let key = child
                    .name
                    .as_ref()
                    .map(|n| format!("{fc_path}/{n}"))
                    .unwrap_or_default();
                metas.push((key, meta.clone()));
                entries.push(Ok(Box::new(NcDirEntry { meta }) as Box<dyn DavDirEntry>));
            }
            (metas, entries)
        };

        let batch = &self.propfind_batch;
        {
            let mut batch_inner = batch.inner.lock().expect("propfind batch lock");
            for (key, meta) in &metas {
                batch_inner.meta.insert(key.clone(), meta.clone());
                batch_inner.child_paths.insert(key.clone());
                batch_inner.children.insert(meta.fileid);
            }
        }
        // ── Phase 18.1 / 21.1 / 22 T6.6: per-request batch, concurrently ─
        // Build every child's metadata once, then populate the per-request
        // `propfind_batch` so per-child `get_props` reads cached values
        // instead of re-issuing ~11 queries per node (load_meta, dir
        // counts, shares, comments, system tags, custom properties).
        // `get_props` runs after `read_dir` for every child, so the batch
        // is always consumed; nodes outside it (the depth-0 root, which
        // dav-server-rs visits before `read_dir`) fall back to the
        // single-row queries.
        //
        // Every family depends only on the child-id list (plus the dir
        // mime id — a cache hit after startup warmup), so the families
        // run in one `tokio::join!` instead of serial RTTs.  Each helper
        // early-returns on empty input; the results land in disjoint
        // batch maps, so the extends below are lock- and order-safe.
        //
        // PHASE-22 T6.6: gate each family on the client's explicit
        // `<prop>` set (`requested_props` = None → allprop/propname →
        // everything).  Skipped families leave their batch maps empty:
        // in-batch `get_props` consumers read the empty maps (no per-node
        // queries), and PropWriter's 12.1 filter drops the props from the
        // response anyway — identical bytes, less work.
        let child_paths: Vec<String> = metas.iter().map(|(k, _)| k.clone()).collect();
        // PHASE-22 T7: on Postgres the CTE already carried dir counts,
        // shares/notes, comments and system tags — fill the batch maps
        // directly (the per-family statements do not exist on this path).
        // 22.2: the tag prefetch folded into the CTE too — fill the tag
        // cache exactly as `prefetch_tags` did (empty vec for tagless
        // children; the dir's own tags included).
        if let Some((
            dir_counts,
            share_details,
            share_notes,
            comments,
            system_tags,
            tags,
            dir_tags,
            metadata,
        )) = cte
        {
            {
                let mut cache_guard = self.tag_cache.lock().expect("tag cache lock");
                for id in &child_ids {
                    cache_guard.insert(*id, tags.get(id).cloned().unwrap_or_default());
                }
                // Only when the CTE had rows: an empty directory's tags
                // were already cached by the target's `get_props` (which
                // runs before `read_dir`) — an empty `dir_tags` here
                // would wrongly overwrite them.
                if !child_ids.is_empty() {
                    cache_guard.insert(dir_fileid, dir_tags);
                }
            }
            if !child_ids.is_empty() {
                let mut batch_inner = batch.inner.lock().expect("propfind batch lock");
                batch_inner.dir_counts.extend(dir_counts);
                batch_inner.share_details.extend(share_details);
                batch_inner.share_notes.extend(share_notes);
                for id in &child_ids {
                    let (c, u) = comments.get(id).copied().unwrap_or((0, 0));
                    batch_inner.comments.insert(*id, (c, u));
                }
                batch_inner.system_tags.extend(system_tags);
                batch_inner.metadata.extend(metadata);
            }
        } else {
            // SQLite: the batched families (T6.1/T6.3 merges), gated per
            // T6.6 like the families above.
            let (counts, share_maps, cc_unreads, tags) = tokio::join!(
                async {
                    if want_dir_counts {
                        row::count_children_batch(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &dir_child_ids,
                            self.storage_id,
                            dir_mime_id,
                        )
                        .await
                    } else {
                        std::collections::HashMap::new()
                    }
                },
                async {
                    if want_shares {
                        row::share_details_and_notes_batch(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                            &child_ids,
                        )
                        .await
                    } else {
                        (
                            std::collections::HashMap::new(),
                            std::collections::HashMap::new(),
                        )
                    }
                },
                async {
                    if want_comments {
                        row::comments_counts_batch(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &child_ids,
                            &self.uid,
                        )
                        .await
                    } else {
                        std::collections::HashMap::new()
                    }
                },
                async {
                    if want_system_tags {
                        row::system_tags_batch(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &child_ids,
                        )
                        .await
                    } else {
                        std::collections::HashMap::new()
                    }
                },
            );
            let (details, notes) = share_maps;
            if !child_ids.is_empty() {
                let mut batch_inner = batch.inner.lock().expect("propfind batch lock");
                batch_inner.dir_counts.extend(counts);
                batch_inner.share_details.extend(details);
                batch_inner.share_notes.extend(notes);
                for id in &child_ids {
                    let (c, u) = cc_unreads.get(id).copied().unwrap_or((0, 0));
                    batch_inner.comments.insert(*id, (c, u));
                }
                batch_inner.system_tags.extend(tags);
            }
        }
        // Custom props + the tag prefetch — gated (T6.6).  Custom props
        // cannot fold into the CTE (the property-path hash is Rust-side
        // and the children's names only exist after the query); the tag
        // prefetch is folded into the CTE on Postgres (22.2 — the cache
        // was filled above), so SQLite alone keeps the batch call.
        let (props, _) = tokio::join!(
            async {
                if want_custom_props {
                    row::custom_properties_batch(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        &child_paths,
                    )
                    .await
                } else {
                    std::collections::HashMap::new()
                }
            },
            async {
                // §9.5: prefetch tags for the directory + all children
                // so {oc:}favorite/{oc:}tags are ready without N+1 DB
                // queries.  Include the directory.
                if !self.state.pool.is_postgres() && want_tags {
                    let mut prefetch_ids = child_ids.clone();
                    prefetch_ids.push(dir_fileid);
                    crate::tags::prefetch_tags(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        &prefetch_ids,
                        &self.tag_cache,
                    )
                    .await;
                }
            },
        );
        if !child_ids.is_empty() {
            batch
                .inner
                .lock()
                .expect("propfind batch lock")
                .custom_props
                .extend(props);
        }
        let s: FsStream<Box<dyn DavDirEntry>> = Box::pin(stream::iter(entries));
        Ok(s)
    }

    /// Build the full `oc:`/`nc:`/custom property set for one node.
    pub(crate) async fn collect_props(
        &self,
        path: &dav_server::davpath::DavPath,
        do_content: bool,
    ) -> Result<Vec<DavProp>, FsError> {
        let fc_path = self.to_fc_path(path);
        let mut meta = match self.load_meta(&fc_path).await {
            Some(m) => m,
            None => return Ok(vec![]),
        };

        // Read data-fingerprint from appconfig (REQ §6.5)
        let data_fingerprint = {
            let cache = self.state.appconfig_cache.read().expect("appconfig lock");
            cache
                .get_string("core", "data-fingerprint")
                .unwrap_or_default()
        };

        // ── 22.1: one join for the single-row fallbacks ──────────────────────
        // The depth-0 root (any node outside the batch) used to pay ~10
        // statements strictly sequentially — every one of them depends
        // only on the fileid/path known after load_meta (or on nothing),
        // so one `tokio::join!` collapses the serial chain into ~1 RTT.
        // Same statements, same results, same bytes — only scheduling
        // changes (the A/B harness is the parity gate).  In-batch nodes
        // (read_dir children) keep their batch-map hits inside each
        // future, so they still issue no statements.
        let (
            user_state,
            (child_dirs, child_files),
            storage_string,
            note,
            tag_info,
            share_details,
            (comments_count, comments_unread),
            system_tags,
            metadata_json,
            custom_props,
        ) = tokio::join!(
            // Resolve {oc:}owner-display-name and the sharing mask from
            // the per-uid user-state cache (round-4 Task 12): the auth
            // middleware resolved the entry earlier in the request, so
            // this is a cache hit; joined so a cold request pays the
            // queries once instead of serially.
            async {
                nc_auth::cached_user_state(&self.uid, &self.state.pool, &self.state.table_prefix)
                    .await
                    .unwrap_or_else(|e| {
                        tracing::warn!(
                            uid = %self.uid,
                            error = %e,
                            "user-state resolution failed — defaulting"
                        );
                        nc_auth::UserState {
                            is_admin: false,
                            twofa_enabled: false,
                            sharing_disabled: false,
                            display_name: self.uid.clone(),
                        }
                    })
            },
            // Count direct children for directories (REQ §6.5
            // contained-*-count).  Phase 18.1: read_dir pre-computed every
            // child's counts with one GROUP BY; an in-batch miss means an
            // empty directory (0, 0), and only nodes outside the batch
            // (depth-0 root) run the single query.
            async {
                if meta.is_dir_flag && do_content {
                    if {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_contains(&inner, |i| &i.children, &meta.fileid)
                    } {
                        {
                            let inner = self
                                .propfind_batch
                                .inner
                                .lock()
                                .expect("propfind batch lock");
                            batch_get(&inner, |i| &i.dir_counts, &meta.fileid)
                        }
                        .unwrap_or((0, 0))
                    } else {
                        // Resolved once at startup (phase-21 S3).
                        row::count_children(
                            &self.state.pool,
                            &self.state.table_prefix,
                            meta.fileid,
                            self.storage_id,
                            self.state.dir_mime_id,
                        )
                        .await
                    }
                } else {
                    (0, 0)
                }
            },
            // ── Phase 7.6: is_mounted ──────────────────────────────────
            // `is_mounted`: true when the file lives on a non-home
            // storage.  Optimisation: if meta.storage == self.storage_id
            // the FS was already constructed from a home:: storage lookup,
            // so skip DB.  Phase-21 S3: process-wide cache (negative
            // entries) — the table is tiny and near-static.
            async {
                if meta.storage == self.storage_id {
                    None
                } else {
                    row::get_storage_string_id_cached(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.state.storage_cache,
                        meta.storage,
                    )
                    .await
                }
            },
            // `note`: most-recent non-empty share note for this file.
            // Phase 18.1: batched by read_dir; an in-batch miss means no
            // note, and only nodes outside the batch run the single query.
            async {
                if {
                    let inner = self
                        .propfind_batch
                        .inner
                        .lock()
                        .expect("propfind batch lock");
                    batch_contains(&inner, |i| &i.children, &meta.fileid)
                } {
                    {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_get(&inner, |i| &i.share_notes, &meta.fileid)
                    }
                    .unwrap_or_default()
                } else {
                    row::get_share_note(&self.state.pool, &self.state.table_prefix, meta.fileid)
                        .await
                }
            },
            // §9.5: resolve tags / favorite from oc_vcategory /
            // oc_vcategory_to_object.  PHASE-22 T6.6: gate on the
            // requested props like read_dir's prefetch — an ungated
            // lookup here would re-introduce one query per child whenever
            // the prefetch was skipped.
            async {
                if self.prop_requested("http://owncloud.org/ns", "favorite")
                    || self.prop_requested("http://owncloud.org/ns", "tags")
                {
                    crate::tags::get_tag_info(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        meta.fileid,
                        &self.tag_cache,
                    )
                    .await
                } else {
                    crate::tags::TagInfo {
                        tags: Vec::new(),
                        is_favorite: false,
                    }
                }
            },
            // ── PHASE-12.5: share-types / sharees ──────────────────────
            // Phase 18.1: batched by read_dir; single query for nodes
            // outside the batch (the display-name batch runs inside).
            async {
                if {
                    let inner = self
                        .propfind_batch
                        .inner
                        .lock()
                        .expect("propfind batch lock");
                    batch_contains(&inner, |i| &i.children, &meta.fileid)
                } {
                    {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_get(&inner, |i| &i.share_details, &meta.fileid)
                    }
                    .unwrap_or_default()
                } else {
                    row::get_share_details(
                        &self.state.pool,
                        &self.state.table_prefix,
                        &self.uid,
                        meta.fileid,
                    )
                    .await
                }
            },
            // ── PHASE-12.6: comments count + unread (the pair joined
            // internally — both depend only on the fileid; in-batch miss
            // means zero comments).
            async {
                if do_content {
                    if {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_contains(&inner, |i| &i.children, &meta.fileid)
                    } {
                        {
                            let inner = self
                                .propfind_batch
                                .inner
                                .lock()
                                .expect("propfind batch lock");
                            batch_get(&inner, |i| &i.comments, &meta.fileid)
                        }
                        .unwrap_or((0, 0))
                    } else {
                        let count = row::get_comments_count(
                            &self.state.pool,
                            &self.state.table_prefix,
                            meta.fileid,
                        )
                        .await;
                        let unread = row::get_comments_unread(
                            &self.state.pool,
                            &self.state.table_prefix,
                            meta.fileid,
                            &self.uid,
                        )
                        .await;
                        (count, unread)
                    }
                } else {
                    (0, 0)
                }
            },
            // ── PHASE-12.7: system tags ────────────────────────────────
            async {
                if do_content {
                    if {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_contains(&inner, |i| &i.children, &meta.fileid)
                    } {
                        {
                            let inner = self
                                .propfind_batch
                                .inner
                                .lock()
                                .expect("propfind batch lock");
                            batch_get(&inner, |i| &i.system_tags, &meta.fileid)
                        }
                        .unwrap_or_default()
                    } else {
                        row::get_system_tags_for_file(
                            &self.state.pool,
                            &self.state.table_prefix,
                            meta.fileid,
                        )
                        .await
                    }
                } else {
                    Vec::new()
                }
            },
            // ── files_metadata json (nc:metadata-* family)
            // One json row per file (files only - dirs have no row; the
            // in-batch miss means "no metadata", matching PHP).
            async {
                if do_content {
                    if {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_contains(&inner, |i| &i.children, &meta.fileid)
                    } {
                        {
                            let inner = self
                                .propfind_batch
                                .inner
                                .lock()
                                .expect("propfind batch lock");
                            batch_get(&inner, |i| &i.metadata, &meta.fileid)
                        }
                    } else {
                        row::get_metadata_json(
                            &self.state.pool,
                            &self.state.table_prefix,
                            meta.fileid,
                        )
                        .await
                    }
                } else {
                    None
                }
            },
            // Custom properties from oc_properties (task §10.11) ────
            // Needs only the fc path + uid (not the fileid); keyed by
            // path in the batch (Phase 18.1).
            async {
                if do_content {
                    if {
                        let inner = self
                            .propfind_batch
                            .inner
                            .lock()
                            .expect("propfind batch lock");
                        batch_contains(&inner, |i| &i.child_paths, fc_path.as_str())
                    } {
                        {
                            let inner = self
                                .propfind_batch
                                .inner
                                .lock()
                                .expect("propfind batch lock");
                            batch_get(&inner, |i| &i.custom_props, fc_path.as_str())
                        }
                        .unwrap_or_default()
                    } else {
                        crate::row::list_custom_properties(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                            &fc_path,
                        )
                        .await
                    }
                } else {
                    Vec::new()
                }
            },
        );
        let owner_display_name = user_state.display_name.clone();

        // is_shared: false for home-storage nodes — the file is the user's own.
        // Shared nodes (from oc_share) are detected via is_mounted/share_permissions.
        // Declared early so share_permissions computation can branch on it.
        let is_shared = false;

        // ── Determine if this is the home storage mount root.
        // Used for the displayname fallback, the is-mount-root prop, and
        // compute_share_permissions (mount roots gain DELETE|UPDATE).
        let is_mount_root = matches!(meta.path.as_deref(), Some("") | Some("files"));

        // ── Phase 12.3: sharing mask — match PHP's SetupManager sharing_mask
        // storage wrapper.  When sharing is disabled via shareapi config, the
        // SHARE bit is stripped from ALL cache reads; when sharing is enabled
        // (the normal case) this is a passthrough.
        // Round-4 Task 12: from the per-uid user-state cache (resolved
        // above with the display name).
        let sharing_disabled = user_state.sharing_disabled;
        let effective_permissions = row::apply_sharing_mask(meta.permissions, sharing_disabled);

        // NOTE (correction, 2026-07-31): an earlier revision unconditionally
        // stripped PERMISSION_SHARE (16) from the home root here, on the theory
        // that PHP's LazyUserFolder forbids sharing the user root.  That matched
        // only a cold/first-request artifact (a stale capture that seeded
        // SPECS/04-tasks/comparison.md).  Verified against live PHP — both this
        // dev instance (via the proxy's php.dev.local entry) and the reference
        // deployment — the home root reports PERMISSION_SHARE in steady state:
        // `oc:permissions` = RGDNVCK, `ocs:share-permissions` = 31,
        // `ocm:share-permissions` = ["share","read","write"].  The unconditional
        // `& !16` is therefore removed; only the genuine sharing-disabled mask
        // above applies.
        //
        // How PHP can still produce GDNVCK / 15 (observed reproducibly, but
        // transient): when `Root::getUserFolder()` runs before the user's
        // filesystem is set up (`isSetupComplete` false — cold OPCache, right
        // after php-fpm restart, or first touch of the user folder), it returns
        // an *unresolved* `LazyUserFolder`, whose constructor caches
        // `permissions = PERMISSION_ALL ^ PERMISSION_SHARE = 15`
        // (lib/private/Files/Node/LazyUserFolder.php: "Sharing user root folder
        // is not allowed").  `LazyFolder::getPermissions()` returns that cached
        // 15 *only until the folder is resolved*; the first access runs the
        // resolution closure, after which the real home-root permissions (31 →
        // RGDNVCK) are reported.  It is therefore a cold-start window, not the
        // steady state.  We deliberately target the steady state: Rust reads the
        // resolved `oc_filecache` row directly, so it cannot observe that window
        // and does not replicate the transient 15.

        // Update meta so build_props() uses the masked permissions for {oc:}permissions.
        meta.permissions = effective_permissions;

        // ── Phase 12.4: share_permissions — match PHP Node::getSharePermissions().
        // For non-shared nodes (home storage) use the node's own (masked) permissions,
        // with DELETE|UPDATE OR-ed for the mount root, and CREATE|DELETE
        // cleared for files.  For shared nodes (future) use the share's mask.
        let share_permissions = if is_shared {
            // Shared node: use the share's permissions from oc_share.
            row::get_share_max_permissions(
                &self.state.pool,
                &self.state.table_prefix,
                &self.uid,
                meta.fileid,
            )
            .await
        } else {
            // Own file: derive from the node's (masked) permissions.
            row::compute_share_permissions(effective_permissions, meta.is_dir_flag, is_mount_root)
        };

        // ── Phase 12.4: OCM share-permissions JSON ─────────────────────
        let ocm_share_permissions = row::permissions_to_ocm_json(share_permissions);

        // Phase 7.6: is_mounted from the joined storage lookup (the
        // lookup itself only runs for non-home storages).
        let is_mounted = storage_string
            .map(|id| !id.starts_with("home::"))
            .unwrap_or(false);

        // `download_url`: direct WebDAV URL for home-storage files.
        // Format: {overwrite.cli.url}/remote.php/webdav/{path-without-files-prefix}
        // Empty for non-home storage (object/S3 URLs require storage-specific
        // signed-URL support which is out of scope, PHASE-7.6).
        // Only generated for files (not directories) and when base_url is set.
        let download_url = if !is_mounted && !meta.is_dir_flag && !self.state.base_url.is_empty() {
            // `meta.path` is like "files/Photos/img.jpg"; strip "files" prefix
            // to get the WebDAV subpath "/Photos/img.jpg".
            let subpath = meta
                .path
                .as_deref()
                .unwrap_or("")
                .trim_start_matches("files");
            let base = self.state.base_url.trim_end_matches('/');
            format!("{base}/remote.php/webdav{}", percent_encode_path(subpath))
        } else {
            String::new()
        };

        let instance_id = &self.state.instance_id;

        // §10.12 / §11.1: compute {nc:}has-preview from mimetype + the resolved
        // provider registry (enabledPreviewProviders gating, Imaginary, binaries).
        let has_preview = self
            .state
            .preview_registry
            .is_available(&meta.mime_type, is_mounted);

        // ── PHASE-12.5: share-types / sharees XML from the joined rows ─
        let mut share_types: Vec<i32> = share_details
            .iter()
            .map(|d| d.share_type as i32)
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        share_types.sort_unstable();
        let share_types_xml = row::format_share_types_xml(&share_types);
        let sharees_xml = row::format_sharees_xml(&share_details);

        // ── PHASE-12.6: comments href ──────────────────────────────────
        let comments_href = if do_content && !self.state.base_url.is_empty() {
            row::build_comments_href(&self.state.base_url, meta.fileid)
        } else {
            String::new()
        };

        // ── PHASE-12.7: system-tags XML from the joined rows ───────────
        let system_tags_xml = row::format_system_tags_xml(&system_tags, true);

        let mut props = crate::props::build_props(
            &meta,
            instance_id,
            &self.uid,
            &owner_display_name,
            do_content,
            &data_fingerprint,
            child_dirs,
            child_files,
            is_mounted,
            is_shared,
            share_permissions,
            &download_url,
            &note,
            has_preview,
            &tag_info.tags,
            tag_info.is_favorite,
        );

        // ── Append Phase 12 extended properties ──────────────────────────
        if do_content {
            crate::props::add_metadata_props(&mut props, metadata_json.as_ref());

            // nc:reminder-due-date — the files_reminders app registers the
            // prop for EVERY node, empty when no reminder is set (verified
            // against the web files app's propfind, 2026-08-14).  Gated on
            // the app's enabled state: with the app disabled PHP's plugin
            // is not registered and the prop answers 404.
            if self
                .state
                .appconfig_cache
                .read()
                .expect("appconfig lock")
                .get_raw("files_reminders", "enabled")
                == Some("yes")
            {
                props.push(crate::props::make_prop(
                    "reminder-due-date",
                    "nc",
                    crate::props::NC_NS,
                    "",
                ));
            }

            // nc:rich-workspace-flat / nc:rich-workspace-file-flat — the
            // text app's WorkspacePlugin (2026-08-14), gated exactly as
            // PHP gates it: the app enabled, the admin config (default
            // true), the user's preference (default true), Directory
            // nodes only, the requested set, and the depth-skip (children
            // of a depth>0 flat-only request answer '' — only the target
            // gets content).
            let want_workspace = meta.is_dir_flag
                && (self.prop_requested(crate::props::NC_NS, "rich-workspace")
                    || self.prop_requested(crate::props::NC_NS, "rich-workspace-flat")
                    || self.prop_requested(crate::props::NC_NS, "rich-workspace-file")
                    || self.prop_requested(crate::props::NC_NS, "rich-workspace-file-flat"));
            if want_workspace {
                let text_enabled = self
                    .state
                    .appconfig_cache
                    .read()
                    .expect("appconfig lock")
                    .get_raw("text", "enabled")
                    == Some("yes");
                let workspace_available = self
                    .state
                    .appconfig_cache
                    .read()
                    .expect("appconfig lock")
                    .get_string("text", "workspace_available")
                    .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                    .unwrap_or(true);
                if text_enabled && workspace_available {
                    let non_flat = self.prop_requested(crate::props::NC_NS, "rich-workspace")
                        || self.prop_requested(crate::props::NC_NS, "rich-workspace-file");
                    if self.propfind_depth > 0 && !non_flat && self.propfind_target != fc_path {
                        // Depth-skip: children answer '' without touching
                        // the workspace file.
                        crate::props::add_workspace_props(&mut props, "", None);
                    } else {
                        let user_enabled = row::get_user_preference(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                            "text",
                            "workspace_enabled",
                        )
                        .await
                        .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                        .unwrap_or(true);
                        let (content, ws_fileid) = if user_enabled {
                            if let Some((ws_fileid, ws_path)) = row::get_workspace_file(
                                &self.state.pool,
                                &self.state.table_prefix,
                                meta.fileid,
                                self.storage_id,
                                self.state.dir_mime_id,
                            )
                            .await
                            {
                                let rel = ws_path.strip_prefix("files/").unwrap_or(&ws_path);
                                let full = self.state.data_directory.join(&self.uid).join(rel);
                                let content = tokio::fs::read(&full)
                                    .await
                                    .map(|b| String::from_utf8_lossy(&b).into_owned())
                                    .unwrap_or_default();
                                (content, Some(ws_fileid))
                            } else {
                                (String::new(), None)
                            }
                        } else {
                            (String::new(), None)
                        };
                        crate::props::add_workspace_props(&mut props, &content, ws_fileid);
                    }
                }
            }

            crate::props::add_phase12_props(
                &mut props,
                &crate::props::Phase12PropCtx {
                    ocm_share_permissions: &ocm_share_permissions,
                    share_types_xml: &share_types_xml,
                    sharees_xml: &sharees_xml,
                    comments_count,
                    comments_unread,
                    comments_href: &comments_href,
                    system_tags_xml: &system_tags_xml,
                },
            );
        }

        // ── Append custom properties from oc_properties (task §10.11) ─────
        // Resolved in the 22.1 join above (batch-hit or single query);
        // only the push loop remains here.
        if do_content {
            for (propname, propvalue, _valuetype) in custom_props {
                if let Some((ns, name)) = crate::row::parse_clark_notation(&propname) {
                    // Skip known-namespace props — they are handled above or
                    // by the dav-server framework.
                    if ns == "DAV:"
                        || ns == "http://owncloud.org/ns"
                        || ns == "http://nextcloud.org/ns"
                        || ns == "http://open-collaboration-services.org/ns"
                    {
                        continue;
                    }
                    props.push(DavProp::new(
                        name.to_string(),
                        String::new(),
                        ns.to_string(),
                        propvalue,
                    ));
                }
            }
        }

        Ok(props)
    }
}
