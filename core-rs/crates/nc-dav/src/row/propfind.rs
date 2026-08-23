use nc_db::pool::DbPool;
use sqlx::{Postgres, Row};
use super::sharing::ShareDetail;
use super::sharing::batch_lookup_display_names;
use super::sql::cached_sql;
use super::system_tags::SystemTagRow;
use super::types::{FileCacheExtRow, FileCacheRow};


/// PHASE-22 T7: the entire depth-1 child fan-out in ONE Postgres statement.
///
/// `WITH kids AS (…)` — the children + their extended rows (the phase-21
/// JOIN listing) — plus a correlated sub-select per family keyed on the
/// child's fileid, aggregated with `json_agg` / `json_build_object`.  The
/// Rust side decodes the JSON into the same per-family shapes the batched
/// queries produce, and the share split (notes + details + display names)
/// runs unchanged.  Postgres-only: SQLite keeps the multi-query batch path
/// behind the `DbPool` variant.
///
/// Custom properties stay a separate gated statement (T7 deviation): the
/// `>250`-char property-path hash is Rust-side, and the children's names —
/// needed to build the paths — only exist after this query returns.
#[derive(serde::Deserialize)]
struct ShareJson {
    share_type: i16,
    share_with: Option<String>,
    uid_owner: String,
    uid_initiator: Option<String>,
    note: String,
    stime: i64,
}


#[derive(serde::Deserialize)]
struct TagJson {
    id: i64,
    name: String,
    visibility: i16,
    editable: i16,
    color: Option<String>,
}


/// Everything `read_dir` needs per child, in the shapes the batch maps hold.
pub struct PropfindCte {
    /// The children (filecache + extended joined), same as
    /// `list_children_with_ext`.
    pub children: Vec<FileCacheRow>,
    pub extended: std::collections::HashMap<i64, FileCacheExtRow>,
    pub dir_counts: std::collections::HashMap<i64, (i64, i64)>,
    pub share_details: std::collections::HashMap<i64, Vec<ShareDetail>>,
    pub share_notes: std::collections::HashMap<i64, String>,
    pub comments: std::collections::HashMap<i64, (i64, i64)>,
    pub system_tags: std::collections::HashMap<i64, Vec<SystemTagRow>>,
    /// Per-child `oc_vcategory` category strings (favorite sentinel included)
    /// — the tag prefetch folded into the CTE (22.2).  Missing = no tags.
    pub tags: std::collections::HashMap<i64, Vec<String>>,
    /// Per-child `oc_files_metadata.json` (parsed) — the `nc:metadata-*`
    /// family.  Missing when gated off or no metadata row.
    pub metadata: std::collections::HashMap<i64, serde_json::Value>,
    /// The directory's own tags (the prefetch covered the dir fileid too).
    /// Only populated when the dir has children (the uncorrelated sub-select
    /// rides on the kid rows); an empty dir falls back to get_props's
    /// cache-miss query — statement-neutral.
    pub dir_tags: Vec<String>,
}


/// PHASE-22 T8.1: the read_dir hot-path statement texts, built once per
/// prefix (fixed for a running server).  The strings are leaked (&'static)
/// so the hot path skips the per-call `format!`/alloc entirely.


/// Which CTE-resident families the client requested (T6.6).  The sub-selects
/// are gated with `CASE WHEN $N THEN …` on these (22.2-C) so a skipped family
/// costs nothing server-side — the SubPlan is not executed.
pub struct PropfindGates {
    pub dir_counts: bool,
    pub shares: bool,
    pub comments: bool,
    pub system_tags: bool,
    pub tags: bool,
    /// Any requested prop in the `nc:metadata-*` family (files_metadata).
    pub metadata: bool,
}


pub async fn propfind_batch_cte(
    pool: &DbPool,
    prefix: &str,
    parent: i64,
    storage: i64,
    dir_mime_id: i64,
    uid: &str,
    gates: &PropfindGates,
) -> PropfindCte {
    let sql = cached_sql(prefix, |prefix| {
        format!(
        "WITH kids AS ( \
             SELECT fc.fileid, fc.storage, fc.path, fc.path_hash, fc.parent, fc.name, \
                    fc.mimetype, fc.mimepart, fc.size, fc.mtime, fc.storage_mtime, \
                    fc.etag, fc.permissions, fc.checksum, \
                    fe.metadata_etag, fe.creation_time, fe.upload_time \
             FROM {prefix}filecache fc \
             LEFT JOIN {prefix}filecache_extended fe ON fe.fileid = fc.fileid \
             WHERE fc.parent = $1 AND fc.storage = $2 \
         ) \
         SELECT k.fileid, k.storage, k.path, k.path_hash, k.parent, k.name, \
                k.mimetype, k.mimepart, k.size, k.mtime, k.storage_mtime, \
                k.etag, k.permissions, k.checksum, k.metadata_etag, \
                k.creation_time, k.upload_time, \
                (CASE WHEN $5 AND k.mimetype = $3 THEN (SELECT json_build_object( \
                    'dirs', count(*) FILTER (WHERE c.mimetype = $3), \
                    'files', count(*) FILTER (WHERE c.mimetype != $3)) \
                 FROM {prefix}filecache c WHERE c.parent = k.fileid AND c.storage = $2)  END) AS dir_counts, \
                (CASE WHEN $6 THEN (SELECT json_agg(json_build_object( \
                    'file_source', s.file_source, 'share_type', s.share_type, \
                    'share_with', s.share_with, 'uid_owner', s.uid_owner, \
                    'uid_initiator', s.uid_initiator, 'note', s.note, 'stime', s.stime)) \
                 FROM {prefix}share s WHERE s.file_source = k.fileid)  END) AS shares, \
                (CASE WHEN $7 THEN (SELECT json_build_object( \
                    'n', count(*), \
                    'unread', count(*) FILTER (WHERE c.actor_type = 'users' AND c.actor_id != $4 \
                        AND c.creation_timestamp > COALESCE(m.marker_datetime, '1970-01-01 00:00:00'))) \
                 FROM {prefix}comments c \
                 LEFT JOIN {prefix}comments_read_markers m \
                   ON m.user_id = $4 AND m.object_type = 'files' AND m.object_id = c.object_id \
                 WHERE c.object_type = 'files' AND c.object_id = k.fileid::text)  END) AS comments, \
                (CASE WHEN $8 THEN (SELECT json_agg(json_build_object( \
                    'id', t.id, 'name', t.name, 'visibility', t.visibility, \
                    'editable', t.editable, 'color', t.color) \
                    ORDER BY LOWER(t.name)) \
                 FROM {prefix}systemtag t \
                 JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id \
                 WHERE m.objectid = k.fileid::text AND m.objecttype = 'files' \
                   AND t.visibility = 1)  END) AS system_tags, \
                (CASE WHEN $9 THEN (SELECT json_agg(vc.category) \
                 FROM {prefix}vcategory_to_object vco \
                 JOIN {prefix}vcategory vc ON vc.id = vco.categoryid \
                 WHERE vco.objid = k.fileid AND vco.type = 'files' \
                   AND vc.uid = $4 AND vc.type = 'files')  END) AS tags, \
                (CASE WHEN $9 THEN (SELECT json_agg(vc.category) \
                 FROM {prefix}vcategory_to_object vco \
                 JOIN {prefix}vcategory vc ON vc.id = vco.categoryid \
                 WHERE vco.objid = $1 AND vco.type = 'files' \
                   AND vc.uid = $4 AND vc.type = 'files')  END) AS dir_tags, \
                (CASE WHEN $10 THEN (SELECT fm.json FROM {prefix}files_metadata fm \
                 WHERE fm.file_id = k.fileid)  END) AS metadata \
         FROM kids k",
        prefix = prefix,
    )
    });

    let rows = match pool {
        DbPool::Pg(p) => match sqlx::query::<Postgres>(&sql)
            .bind(parent)
            .bind(storage)
            .bind(dir_mime_id)
            .bind(uid)
            .bind(gates.dir_counts)
            .bind(gates.shares)
            .bind(gates.comments)
            .bind(gates.system_tags)
            .bind(gates.tags)
            .bind(gates.metadata)
            .fetch_all(p)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::error!(parent, error = %e, "propfind_batch_cte: SQL error");
                return PropfindCte {
                    children: Vec::new(),
                    extended: std::collections::HashMap::new(),
                    dir_counts: std::collections::HashMap::new(),
                    share_details: std::collections::HashMap::new(),
                    share_notes: std::collections::HashMap::new(),
                    comments: std::collections::HashMap::new(),
                    system_tags: std::collections::HashMap::new(),
                    tags: std::collections::HashMap::new(),
                    dir_tags: Vec::new(),
                    metadata: std::collections::HashMap::new(),
                };
            }
        },
        // Postgres-only (T7); SQLite keeps the batch path.
        DbPool::Sqlite(_) => {
            return PropfindCte {
                children: Vec::new(),
                extended: std::collections::HashMap::new(),
                dir_counts: std::collections::HashMap::new(),
                share_details: std::collections::HashMap::new(),
                share_notes: std::collections::HashMap::new(),
                comments: std::collections::HashMap::new(),
                system_tags: std::collections::HashMap::new(),
                tags: std::collections::HashMap::new(),
                dir_tags: Vec::new(),
                metadata: std::collections::HashMap::new(),
            };
        }
    };

    let mut out = PropfindCte {
        children: Vec::with_capacity(rows.len()),
        extended: std::collections::HashMap::new(),
        dir_counts: std::collections::HashMap::new(),
        share_details: std::collections::HashMap::new(),
        share_notes: std::collections::HashMap::new(),
        comments: std::collections::HashMap::new(),
        system_tags: std::collections::HashMap::new(),
        tags: std::collections::HashMap::new(),
        dir_tags: Vec::new(),
        metadata: std::collections::HashMap::new(),
    };

    // Shares across all kids: the notes + details split needs every row
    // before the display-name batch, so collect first.
    let mut all_shares: Vec<(i64, ShareJson)> = Vec::new();
    for r in &rows {
        let fileid: i64 = r.get("fileid");
        let child = FileCacheRow {
            fileid,
            storage: r.get("storage"),
            path: r.get("path"),
            path_hash: r.get("path_hash"),
            parent: r.get("parent"),
            name: r.get("name"),
            mimetype: r.get("mimetype"),
            mimepart: r.get("mimepart"),
            size: r.get("size"),
            mtime: r.get("mtime"),
            storage_mtime: r.get("storage_mtime"),
            etag: r.get("etag"),
            permissions: r.get::<Option<i32>, _>("permissions").unwrap_or(0),
            checksum: r.get("checksum"),
            creation_time: 0,
            upload_time: 0,
        };
        out.extended.insert(
            fileid,
            FileCacheExtRow {
                metadata_etag: r.get("metadata_etag"),
                // NULL for children without an extended row (the LEFT JOIN).
                creation_time: r.get::<Option<i64>, _>("creation_time").unwrap_or(0),
                upload_time: r.get::<Option<i64>, _>("upload_time").unwrap_or(0),
            },
        );
        out.children.push(child);

        // Dir counts — only meaningful for directory children (mirrors the
        // dir-only parent list the batch path uses).  NULL when the
        // dir-counts gate ($5) is off — decode Option (22.2-C).
        if r.get::<i64, _>("mimetype") == dir_mime_id {
            if let Ok(Some(serde_json::Value::Object(v))) =
                r.try_get::<Option<serde_json::Value>, _>("dir_counts")
            {
                let dirs = v.get("dirs").and_then(|x| x.as_i64()).unwrap_or(0);
                let files = v.get("files").and_then(|x| x.as_i64()).unwrap_or(0);
                out.dir_counts.insert(fileid, (dirs, files));
            }
        }

        // Comments (count, unread) — NULL when the comments gate ($7) is off
        // (22.2-C) — decode Option.
        if let Ok(Some(serde_json::Value::Object(v))) =
            r.try_get::<Option<serde_json::Value>, _>("comments")
        {
            let n = v.get("n").and_then(|x| x.as_i64()).unwrap_or(0);
            let unread = v.get("unread").and_then(|x| x.as_i64()).unwrap_or(0);
            out.comments.insert(fileid, (n, unread));
        }

        // System tags.
        let tags: Option<Vec<TagJson>> = match r.get::<Option<serde_json::Value>, _>("system_tags")
        {
            Some(v) => serde_json::from_value(v).ok().flatten(),
            None => None,
        };
        if let Some(tags) = tags {
            let rows: Vec<SystemTagRow> = tags
                .into_iter()
                .map(|t| SystemTagRow {
                    id: t.id,
                    name: t.name,
                    user_visible: t.visibility == 1,
                    user_assignable: t.editable == 1,
                    color: t.color,
                })
                .collect();
            out.system_tags.insert(fileid, rows);
        }

        // Tags (22.2) — the prefetch folded into the CTE: the category
        // strings per child, favorite sentinel included, exactly what
        // `get_tags_batch` produced for `prefetch_tags`.
        let tags: Option<Vec<String>> = match r.get::<Option<serde_json::Value>, _>("tags") {
            Some(v) => serde_json::from_value(v).ok().flatten(),
            None => None,
        };
        if let Some(tags) = tags {
            out.tags.insert(fileid, tags);
        }

        // Metadata (files_metadata) — NULL when the gate is off or the file
        // has no metadata row.  The column is TEXT on Postgres, so decode as
        // a string and parse in Rust (sqlx rejects a direct Value decode of
        // a text column).
        if let Ok(Some(v)) = r.try_get::<Option<String>, _>("metadata") {
            if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&v) {
                out.metadata.insert(fileid, parsed);
            }
        }

        // Shares — collect for the split below (needs the whole set for the
        // display-name batch).
        let shares: Option<Vec<ShareJson>> = match r.get::<Option<serde_json::Value>, _>("shares") {
            Some(v) => serde_json::from_value(v).ok().flatten(),
            None => None,
        };
        if let Some(shares) = shares {
            for s in shares {
                all_shares.push((fileid, s));
            }
        }
    }

    // The directory's own tags (22.2): the uncorrelated sub-select repeats
    // the same value on every row — take it from the first (absent when the
    // directory has no children; get_props's cache-miss query covers that).
    if let Some(first) = rows.first() {
        if let Some(v) = first.get::<Option<serde_json::Value>, _>("dir_tags") {
            if let Ok(tags) = serde_json::from_value::<Vec<String>>(v) {
                out.dir_tags = tags;
            }
        }
    }

    // Share split — identical semantics to `share_details_and_notes_batch`:
    // notes = max-stime non-empty note per file, filter-free; details = the
    // `get_share_details` filter with the display-name batch.
    let mut notes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut best_stime: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (fileid, s) in &all_shares {
        if s.note.is_empty() {
            continue;
        }
        match best_stime.get(fileid) {
            Some(prev) if *prev >= s.stime => continue,
            _ => {
                best_stime.insert(*fileid, s.stime);
                notes.insert(*fileid, s.note.clone());
            }
        }
    }
    let filtered: Vec<(&i64, &ShareJson)> = all_shares
        .iter()
        .filter(|(_, s)| {
            matches!(s.share_type, 0 | 1 | 3 | 4 | 6 | 7 | 10 | 12)
                && (s.uid_owner == uid
                    || s.uid_initiator.as_deref() == Some(uid)
                    || s.share_with.as_deref() == Some(uid))
        })
        .map(|(fileid, s)| (fileid, s))
        .collect();
    let user_withs: Vec<String> = filtered
        .iter()
        .filter(|(_, s)| s.share_type == 0)
        .filter_map(|(_, s)| s.share_with.clone())
        .collect();
    let display_names = batch_lookup_display_names(pool, prefix, &user_withs).await;
    for (fileid, s) in filtered {
        let displayname = match s.share_type {
            0 => s
                .share_with
                .as_ref()
                .and_then(|sw| display_names.get(sw.as_str()).cloned())
                .unwrap_or_else(|| s.share_with.clone().unwrap_or_default()),
            _ => s.share_with.clone().unwrap_or_default(),
        };
        out.share_details
            .entry(*fileid)
            .or_default()
            .push(ShareDetail {
                share_type: s.share_type,
                share_with: s.share_with.clone(),
                share_with_displayname: displayname,
            });
    }
    out.share_notes = notes;
    out
}
