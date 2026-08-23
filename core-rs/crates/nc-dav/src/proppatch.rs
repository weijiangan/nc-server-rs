//! PROPPATCH.
//!
//! Standard DAV props (`getetag`, `getlastmodified`, `creationdate`) and the
//! Nextcloud time props write straight to `oc_filecache` /
//! `oc_filecache_extended`; `{oc:}favorite` and `{oc:}tags` route through the
//! tag store; everything else is stored verbatim in `oc_properties`.

use dav_server::fs::{DavProp, FsError};
use sqlx::{Postgres, Sqlite};

use nc_db::pool::DbPool;

use crate::cache_rows::ensure_files_metadata_appconfig;
use crate::path_utils::{extract_text_from_prop_xml, parse_iso8601};
use crate::row;
use crate::NcFileSystem;

impl NcFileSystem {
    /// PROPPATCH.
    pub(crate) async fn patch_props_inner(
        &self,
        path: &dav_server::davpath::DavPath,
        patch: Vec<(bool, DavProp)>,
    ) -> Result<Vec<(http::StatusCode, DavProp)>, FsError> {
        let fc_path = self.to_fc_path(path);
        let hash = row::path_hash(&fc_path);
        let mut results = Vec::new();

        for (set, prop) in patch {
            let ns = prop.namespace.as_deref().unwrap_or("");
            let name = prop.name.as_str();

            let status = if set {
                match (ns, name) {
                    // ── Standard DAV writable props (REQ §6.6) ───────────

                    // {DAV:}getetag — set custom ETag
                    ("DAV:", "getetag") => {
                        if let Some(val) = extract_text_from_prop_xml(&prop) {
                            let etag = val.trim().trim_matches('"').to_string();
                            let sql = format!(
                                "UPDATE {prefix}filecache SET etag=$1 \
                                 WHERE storage=$2 AND path_hash=$3",
                                prefix = self.state.table_prefix
                            );
                            let ok = match &self.state.pool {
                                DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
                                    .bind(&etag)
                                    .bind(self.storage_id)
                                    .bind(&hash)
                                    .execute(p)
                                    .await
                                    .map(|_| ())
                                    .is_ok(),
                                DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
                                    .bind(&etag)
                                    .bind(self.storage_id)
                                    .bind(&hash)
                                    .execute(p)
                                    .await
                                    .map(|_| ())
                                    .is_ok(),
                            };
                            if ok {
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::INTERNAL_SERVER_ERROR
                            }
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }

                    // {DAV:}getlastmodified / {DAV:}lastmodified — update mtime
                    ("DAV:", "getlastmodified") | ("DAV:", "lastmodified") => {
                        if let Some(val) = extract_text_from_prop_xml(&prop) {
                            // RFC 1123 date OR Unix timestamp integer
                            let ts_opt = val.trim().parse::<i64>().ok().or_else(|| {
                                httpdate::parse_http_date(val.trim()).ok().and_then(|st| {
                                    st.duration_since(std::time::UNIX_EPOCH)
                                        .ok()
                                        .map(|d| d.as_secs() as i64)
                                })
                            });
                            if let Some(ts) = ts_opt {
                                let sql = format!(
                                    "UPDATE {prefix}filecache SET mtime=$1, storage_mtime=$2 \
                                     WHERE storage=$3 AND path_hash=$4",
                                    prefix = self.state.table_prefix
                                );
                                let result = match &self.state.pool {
                                    DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
                                        .bind(ts)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                    DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
                                        .bind(ts)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                };
                                if let Err(e) = result {
                                    tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update mtime");
                                }
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }

                    // {DAV:}creationdate — set creation time (ISO 8601)
                    //
                    // When the oc_filecache_extended row does not yet exist we INSERT it,
                    // reading upload_time from oc_filecache so that the sibling column is
                    // preserved rather than zeroed (REQ §6.6, PHASE-4.10).
                    ("DAV:", "creationdate") => {
                        if let Some(val) = extract_text_from_prop_xml(&prop) {
                            if let Some(ts) = parse_iso8601(val.trim()) {
                                let sql = format!(
                                    "INSERT INTO {prefix}filecache_extended \
                                     (fileid, creation_time, metadata_etag, upload_time) \
                                     SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                     WHERE storage=$2 AND path_hash=$3 \
                                     ON CONFLICT(fileid) DO UPDATE SET creation_time=excluded.creation_time",
                                    prefix = self.state.table_prefix
                                );
                                let result = match &self.state.pool {
                                    DbPool::Pg(p) => sqlx::query::<Postgres>(&sql)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                    DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                };
                                if let Err(e) = result {
                                    tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update creationdate");
                                }
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }

                    // {DAV:}displayname — explicitly blocked (REQ §6.6)
                    ("DAV:", "displayname") => http::StatusCode::FORBIDDEN,

                    // ── NC writable props ─────────────────────────────────
                    //
                    // For each nc: time property, when the oc_filecache_extended row is
                    // absent we INSERT it, reading the *other* timestamp column from
                    // oc_filecache so neither value is zeroed (REQ §6.6, PHASE-4.10).
                    ("http://nextcloud.org/ns", "creation_time") => {
                        if let Some(val) = extract_text_from_prop_xml(&prop) {
                            if let Ok(ts) = val.trim().parse::<i64>() {
                                let sql_upsert = format!(
                                    "INSERT INTO {prefix}filecache_extended \
                                     (fileid, creation_time, metadata_etag, upload_time) \
                                     SELECT fileid, $1, NULL, upload_time FROM {prefix}filecache \
                                     WHERE storage = $2 AND path_hash = $3 \
                                     ON CONFLICT(fileid) DO UPDATE SET creation_time = excluded.creation_time",
                                    prefix = self.state.table_prefix,
                                );
                                let result = match &self.state.pool {
                                    DbPool::Pg(p) => sqlx::query::<Postgres>(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                    DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                };
                                if let Err(e) = result {
                                    tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update timestamp");
                                }
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }

                    // {nc:}upload_time — update upload time (unix int).
                    // Preserve creation_time from oc_filecache when inserting a new
                    // extended row (PHASE-4.10).
                    ("http://nextcloud.org/ns", "upload_time") => {
                        if let Some(val) = extract_text_from_prop_xml(&prop) {
                            if let Ok(ts) = val.trim().parse::<i64>() {
                                let sql_upsert = format!(
                                    "INSERT INTO {prefix}filecache_extended \
                                     (fileid, upload_time, metadata_etag, creation_time) \
                                     SELECT fileid, $1, NULL, creation_time FROM {prefix}filecache \
                                     WHERE storage = $2 AND path_hash = $3 \
                                     ON CONFLICT(fileid) DO UPDATE SET upload_time = excluded.upload_time",
                                    prefix = self.state.table_prefix,
                                );
                                let result = match &self.state.pool {
                                    DbPool::Pg(p) => sqlx::query::<Postgres>(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                    DbPool::Sqlite(p) => sqlx::query::<Sqlite>(&sql_upsert)
                                        .bind(ts)
                                        .bind(self.storage_id)
                                        .bind(&hash)
                                        .execute(p)
                                        .await
                                        .map(|_| ()),
                                };
                                if let Err(e) = result {
                                    tracing::warn!(path_hash = hash, error = %e, "PROPPATCH: failed to update timestamp");
                                }
                                http::StatusCode::OK
                            } else {
                                http::StatusCode::BAD_REQUEST
                            }
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }

                    // §9.5: {oc:}favorite — truthy test: (int)1 || 'true' → tagAs,
                    //                   falsy → unTag.  Returns 200.
                    ("http://owncloud.org/ns", "favorite") => {
                        // The dav-server passes the FULL serialized element
                        // in prop.xml (handle_props.rs element_to_davprop_full:
                        // `<oc:favorite xmlns:oc="…">1</oc:favorite>`), so the
                        // inner TEXT must be extracted before the truthy test —
                        // a naive parse of the whole element failed and
                        // executed an un-favorite (finding #15, the "broken
                        // text-valued PROPPATCH extraction").
                        let state = prop
                            .xml
                            .as_deref()
                            .map(|xml| crate::tags::prop_inner_text(xml));
                        let is_fav = state.as_deref().map_or(false, |s| {
                            let t = s.trim();
                            t.parse::<i64>().ok() == Some(1) || t == "true"
                        });
                        // PHP lazily registers the files_metadata appconfig
                        // on the tag/favorite PROPPATCH.
                        ensure_files_metadata_appconfig(&self.state.pool, &self.state.table_prefix)
                            .await;
                        let fileid = crate::row::lookup_by_path(
                            &self.state.pool,
                            &self.state.table_prefix,
                            self.storage_id,
                            &fc_path,
                        )
                        .await
                        .map(|r| r.fileid)
                        .unwrap_or(0);
                        if fileid != 0 {
                            let _ = crate::tags::set_favorite(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fileid,
                                is_fav,
                            )
                            .await;
                            // Invalidate cached tags for this node.
                            if let Ok(mut cache) = self.tag_cache.lock() {
                                cache.remove(&fileid);
                            }
                        }
                        http::StatusCode::OK
                    }

                    // §9.5: {oc:}tags — diff current vs requested, skip favorite sentinel.
                    ("http://owncloud.org/ns", "tags") => {
                        // PHP lazily registers the files_metadata appconfig
                        // on the tag/favorite PROPPATCH.
                        ensure_files_metadata_appconfig(&self.state.pool, &self.state.table_prefix)
                            .await;
                        let requested = prop
                            .xml
                            .as_ref()
                            .map(|xml| crate::tags::parse_tags_xml(xml))
                            .unwrap_or_default();
                        if let Some(fc_row) = crate::row::lookup_by_path(
                            &self.state.pool,
                            &self.state.table_prefix,
                            self.storage_id,
                            &fc_path,
                        )
                        .await
                        {
                            let fileid = fc_row.fileid;
                            let tag_info = crate::tags::get_tag_info(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fileid,
                                &self.tag_cache,
                            )
                            .await;
                            // Reconstruct the full tag list (including favorite sentinel if set).
                            let mut current = tag_info.tags.clone();
                            if tag_info.is_favorite {
                                current.push(crate::tags::TAG_FAVORITE.to_string());
                            }
                            crate::tags::update_tags(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fileid,
                                &current,
                                &requested,
                            )
                            .await;
                            // Invalidate cached tags.
                            if let Ok(mut cache) = self.tag_cache.lock() {
                                cache.remove(&fileid);
                            }
                        }
                        http::StatusCode::OK
                    }

                    _ => {
                        // Custom property → store in oc_properties (task §10.11).
                        let prop_name_full = format!("{{{ns}}}{name}");
                        if let Some(ref xml_bytes) = prop.xml {
                            let _ = crate::row::upsert_custom_property(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                &fc_path,
                                &prop_name_full,
                                xml_bytes,
                                2, // PROPERTY_TYPE_XML
                            )
                            .await;
                            http::StatusCode::OK
                        } else {
                            http::StatusCode::BAD_REQUEST
                        }
                    }
                }
            } else {
                // DELETE — built-in props cannot be removed; custom props are
                // deleted from oc_properties (task §10.11).
                // §9.5: {oc:}favorite and {oc:}tags are exceptions — PHP
                // handles these by clearing the tag/favorite state.
                match (ns, name) {
                    // §9.5: deleting {oc:}favorite → unTag TAG_FAVORITE → 204.
                    ("http://owncloud.org/ns", "favorite") => {
                        if let Some(fc_row) = crate::row::lookup_by_path(
                            &self.state.pool,
                            &self.state.table_prefix,
                            self.storage_id,
                            &fc_path,
                        )
                        .await
                        {
                            let _ = crate::tags::un_tag(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fc_row.fileid,
                                crate::tags::TAG_FAVORITE,
                            )
                            .await;
                            // Invalidate cached tags.
                            if let Ok(mut cache) = self.tag_cache.lock() {
                                cache.remove(&fc_row.fileid);
                            }
                        }
                        http::StatusCode::NO_CONTENT
                    }
                    // §9.5: deleting {oc:}tags → remove all non-favorite tags → 204.
                    ("http://owncloud.org/ns", "tags") => {
                        if let Some(fc_row) = crate::row::lookup_by_path(
                            &self.state.pool,
                            &self.state.table_prefix,
                            self.storage_id,
                            &fc_path,
                        )
                        .await
                        {
                            let tag_info = crate::tags::get_tag_info(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fc_row.fileid,
                                &self.tag_cache,
                            )
                            .await;
                            // Remove all non-favorite tags. Favorite status is preserved.
                            let mut current = tag_info.tags.clone();
                            if tag_info.is_favorite {
                                current.push(crate::tags::TAG_FAVORITE.to_string());
                            }
                            // Clear all tags (keep only favorite if present).
                            let keep_fav: Vec<String> = if tag_info.is_favorite {
                                vec![crate::tags::TAG_FAVORITE.to_string()]
                            } else {
                                vec![]
                            };
                            crate::tags::update_tags(
                                &self.state.pool,
                                &self.state.table_prefix,
                                &self.uid,
                                fc_row.fileid,
                                &current,
                                &keep_fav,
                            )
                            .await;
                            // Invalidate cached tags.
                            if let Ok(mut cache) = self.tag_cache.lock() {
                                cache.remove(&fc_row.fileid);
                            }
                        }
                        http::StatusCode::NO_CONTENT
                    }
                    ("DAV:", _)
                    | ("http://nextcloud.org/ns", _)
                    | ("http://owncloud.org/ns", _)
                    | ("http://open-collaboration-services.org/ns", _) => {
                        http::StatusCode::FORBIDDEN
                    }
                    _ => {
                        let prop_name_full = format!("{{{ns}}}{name}");
                        let _ = crate::row::delete_custom_property(
                            &self.state.pool,
                            &self.state.table_prefix,
                            &self.uid,
                            &fc_path,
                            &prop_name_full,
                        )
                        .await;
                        http::StatusCode::OK
                    }
                }
            };
            results.push((status, prop));
        }
        Ok(results)
    }
}
