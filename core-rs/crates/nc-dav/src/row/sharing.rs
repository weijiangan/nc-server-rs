use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use super::sql::cached_sql;
use sqlx::{Postgres, Row, Sqlite};


/// Return the MAX permissions from `oc_share` for a given file and owner/initiator.
///
/// Query: `SELECT MAX(permissions) FROM oc_share WHERE (uid_owner = ? OR
/// uid_initiator = ?) AND file_source = ? AND share_type IN (0,1,3)`.
///
/// Returns `31` (all permissions) when the file has no share rows, which
/// represents the owner's own unshared file (REQ §6.5, PHASE-7.6).
pub async fn get_share_max_permissions(pool: &DbPool, prefix: &str, uid: &str, fileid: i64) -> i32 {
    let sql = format!(
        "SELECT MAX(permissions) FROM {prefix}share \
         WHERE (uid_owner = $1 OR uid_initiator = $2) AND file_source = $3 \
         AND share_type IN (0,1,3)"
    );
    // `oc_share.permissions` is `smallint` (INT2) — `MAX()` preserves the
    // argument type, so the decode must be i16 (sqlx Postgres is strict;
    // an i32 read off INT2 throws ColumnDecode and drops the request).
    match pool {
        DbPool::Pg(p) => sqlx::query_scalar::<Postgres, Option<i16>>(&sql)
            .bind(uid)
            .bind(uid)
            .bind(fileid)
            .fetch_optional(p)
            .await
            .ok()
            .flatten()
            .flatten()
            .map(i32::from)
            .unwrap_or(31),
        DbPool::Sqlite(p) => sqlx::query_scalar::<Sqlite, Option<i32>>(&sql)
            .bind(uid)
            .bind(uid)
            .bind(fileid)
            .fetch_optional(p)
            .await
            .ok()
            .flatten()
            .flatten()
            .unwrap_or(31),
    }
}


/// Returns an empty string when no note exists (REQ §6.5, PHASE-7.6).
pub async fn get_share_note(pool: &DbPool, prefix: &str, fileid: i64) -> String {
    let sql = format!(
        "SELECT note FROM {prefix}share WHERE file_source = $1 AND note != '' \
         ORDER BY stime DESC LIMIT 1"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, Option<String>>(&sql)
            .bind(fileid)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
            .flatten()
            .unwrap_or_default()
    })
}


/// Share details + most-recent share notes for a **batch** of files in one
/// `oc_share` scan (T6.1 merge of the former `share_details_batch` +
/// `share_notes_batch` pair).
///
/// One query fetches every `oc_share` row for the file ids (no WHERE beyond
/// the list — the two consumers filter differently, see below); the rows are
/// split in Rust:
///
/// - **details**: rows passing the `get_share_details` filter (share_type
///   `IN (0,1,3,4,6,7,10,12)` and the user is owner / initiator / share_with),
///   with the same display-name resolution.  Per-file row order preserves
///   the scan order — the pre-merge batch had no `ORDER BY` either, so the
///   emitted `{oc:}share-types` / `{nc:}sharees` XML bytes are unchanged.
/// - **notes**: the most-recent (`stime`-max) row with `note != ''` per
///   file — exactly `get_share_note`'s `WHERE note != '' ORDER BY stime
///   DESC LIMIT 1`.  Note rows are deliberately NOT restricted to the
///   details filter: the most-recent note may live on a share the user is
///   not a party to (the single-row query has no such filter either).
///
/// Files without shares / notes are absent from the respective maps (callers
/// fall back to the single queries, which return `[]` / `""`).
pub async fn share_details_and_notes_batch(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileids: &[i64],
) -> (
    std::collections::HashMap<i64, Vec<ShareDetail>>,
    std::collections::HashMap<i64, String>,
) {
    if fileids.is_empty() {
        return (
            std::collections::HashMap::new(),
            std::collections::HashMap::new(),
        );
    }
    // Native bigint[] bind on Postgres (PHASE-22 T4); the rows decode into
    // the shared tuple (file_source, share_type, share_with, uid_owner,
    // uid_initiator, note, stime) per arm, then the split below is common.
    let rows: Vec<(
        i64,
        i16,
        Option<String>,
        String,
        Option<String>,
        String,
        i64,
    )> = match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT file_source, share_type, share_with, uid_owner, uid_initiator, note, stime \
                 FROM {prefix}share \
                 WHERE file_source = ANY($1::bigint[])",
                prefix = prefix,
            );
            match sqlx::query::<Postgres>(&sql)
                .bind(fileids)
                .fetch_all(p)
                .await
            {
                Ok(r) => r
                    .iter()
                    .map(|r| {
                        (
                            r.get("file_source"),
                            r.get("share_type"),
                            r.get("share_with"),
                            r.get("uid_owner"),
                            r.get("uid_initiator"),
                            r.get("note"),
                            r.get("stime"),
                        )
                    })
                    .collect(),
                Err(e) => {
                    tracing::error!(uid, error = %e, "share_details_and_notes_batch: SQL error");
                    return (
                        std::collections::HashMap::new(),
                        std::collections::HashMap::new(),
                    );
                }
            }
        }
        DbPool::Sqlite(p) => {
            let placeholders = (1..=fileids.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT file_source, share_type, share_with, uid_owner, uid_initiator, note, stime \
                 FROM {prefix}share \
                 WHERE file_source IN ({placeholders})",
                prefix = prefix,
            );
            let mut query = sqlx::query(&sql);
            for id in fileids {
                query = query.bind(*id);
            }
            match query.fetch_all(p).await {
                Ok(r) => r
                    .iter()
                    .map(|r| {
                        (
                            r.get("file_source"),
                            r.get("share_type"),
                            r.get("share_with"),
                            r.get("uid_owner"),
                            r.get("uid_initiator"),
                            r.get("note"),
                            r.get("stime"),
                        )
                    })
                    .collect(),
                Err(e) => {
                    tracing::error!(uid, error = %e, "share_details_and_notes_batch: SQL error");
                    return (
                        std::collections::HashMap::new(),
                        std::collections::HashMap::new(),
                    );
                }
            }
        }
    };

    // Notes split: most-recent (max stime) non-empty note per file.  An
    // empty-note row with a newer stime must not hide an older note.
    let mut notes: std::collections::HashMap<i64, String> = std::collections::HashMap::new();
    let mut best_stime: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    for (file_source, _, _, _, _, note, stime) in &rows {
        if note.is_empty() {
            continue;
        }
        match best_stime.get(file_source) {
            Some(prev) if *prev >= *stime => continue,
            _ => {
                best_stime.insert(*file_source, *stime);
                notes.insert(*file_source, note.clone());
            }
        }
    }

    // Details split: the `get_share_details` filter, applied in Rust (the
    // scan carries no WHERE beyond the file ids so the notes split sees
    // every row).
    let filtered = rows
        .iter()
        .filter(|(_, share_type, share_with, owner, initiator, _, _)| {
            if !matches!(share_type, 0 | 1 | 3 | 4 | 6 | 7 | 10 | 12) {
                return false;
            }
            owner == uid || initiator.as_deref() == Some(uid) || share_with.as_deref() == Some(uid)
        })
        .collect::<Vec<_>>();

    // Batch-resolve display names for user-type shares (share_type = 0) —
    // one query for every user across all files, same as the single query.
    let user_withs: Vec<String> = filtered
        .iter()
        .filter(|(_, share_type, ..)| *share_type == 0)
        .filter_map(|(_, _, share_with, ..)| share_with.clone())
        .collect();
    let display_names = batch_lookup_display_names(pool, prefix, &user_withs).await;

    let mut out: std::collections::HashMap<i64, Vec<ShareDetail>> =
        std::collections::HashMap::new();
    for (file_source, share_type, share_with, _, _, _, _) in filtered {
        let displayname = match share_type {
            0 => share_with
                .as_ref()
                .and_then(|sw| display_names.get(sw.as_str()).cloned())
                .unwrap_or_else(|| share_with.clone().unwrap_or_default()),
            _ => share_with.clone().unwrap_or_default(),
        };
        out.entry(*file_source).or_default().push(ShareDetail {
            share_type: *share_type,
            share_with: share_with.clone(),
            share_with_displayname: displayname,
        });
    }
    (out, notes)
}


// ─── Phase 12.3: sharing mask (PHP SetupManager sharing_mask wrapper) ─────────

/// Apply the sharing mask to raw `oc_filecache.permissions`, matching PHP's
/// `PermissionsMask` storage wrapper (`SetupManager.php:176-189`).
///
/// When sharing is disabled for the user, the SHARE bit (16) is stripped.
/// `PERMISSION_ALL - PERMISSION_SHARE = 31 - 16 = 15`.
pub fn apply_sharing_mask(raw_permissions: i32, sharing_disabled: bool) -> i32 {
    if sharing_disabled {
        raw_permissions & 15 // PERMISSION_ALL - PERMISSION_SHARE
    } else {
        raw_permissions
    }
}


// ─── Phase 12.4: share-permissions (PHP Node::getSharePermissions) ────────────

/// Compute the `{ocs:}share-permissions` value matching PHP
/// `Node::getSharePermissions()` (`apps/dav/lib/Connector/Sabre/Node.php:235-276`).
///
/// For non-shared storage (the only kind Rust supports today): returns the node's
/// own `oc_filecache.permissions`, with DELETE|UPDATE OR-ed in for a non-moveable,
/// non-readonly mount root, and CREATE|DELETE cleared for files.
///
/// Constants (from `\OCP\Constants`): READ=1, UPDATE=2, CREATE=4, DELETE=8, SHARE=16.
pub fn compute_share_permissions(raw_permissions: i32, is_dir: bool, is_mount_root: bool) -> i32 {
    let mut perms = raw_permissions;

    // PHP lines 261-275: mount roots of non-moveable, non-readonly mounts
    // always gain DELETE|UPDATE.  Home storage's "files" root satisfies this.
    if is_mount_root {
        perms |= 8 | 2; // PERMISSION_DELETE | PERMISSION_UPDATE
    }

    // PHP lines 280-282: files can't have CREATE or DELETE
    if !is_dir {
        perms &= !(4 | 8); // clear PERMISSION_CREATE | PERMISSION_DELETE
    }

    perms
}


/// Map an NC permission bitmask to OCM share-permissions JSON array string,
/// matching PHP `FilesPlugin::ncPermissions2ocmPermissions()`.
///
/// SHARE(16) → "share", READ(1) → "read", CREATE(4)|UPDATE(2) → "write".
pub fn permissions_to_ocm_json(permissions: i32) -> String {
    let mut parts: Vec<&str> = Vec::new();
    if permissions & 16 != 0 {
        parts.push("\"share\"");
    }
    if permissions & 1 != 0 {
        parts.push("\"read\"");
    }
    if permissions & 4 != 0 || permissions & 2 != 0 {
        parts.push("\"write\"");
    }
    format!("[{}]", parts.join(","))
}


// ─── Phase 12.5: share-types / sharees ─────────────────────────────────────────

/// One share row for the `{oc:}share-types` and `{nc:}sharees` properties.
#[derive(Debug, Clone)]
pub struct ShareDetail {
    pub share_type: i16,
    pub share_with: Option<String>,
    /// Resolved display name; falls back to `share_with` for non-user types.
    pub share_with_displayname: String,
}


/// Return share details for a file node, matching PHP `SharesPlugin::getShare()`.
///
/// Queries shares **by** the user (`uid_owner` / `uid_initiator`) plus shares
/// **with** the user (`share_with`), for all PHP share types (USER, GROUP, LINK,
/// EMAIL, REMOTE, CIRCLE, ROOM, DECK).
pub async fn get_share_details(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
) -> Vec<ShareDetail> {
    let sql = format!(
        "SELECT DISTINCT share_type, share_with \
         FROM {prefix}share \
         WHERE file_source = $1 \
         AND share_type IN (0,1,3,4,6,7,10,12) \
         AND (uid_owner = $2 OR uid_initiator = $3 OR share_with = $4)",
        prefix = prefix
    );
    let rows: Vec<(i16, Option<String>)> = db_dispatch!(pool, |Db, c| {
        match sqlx::query::<Db>(&sql)
            .bind(fileid)
            .bind(uid)
            .bind(uid)
            .bind(uid)
            .fetch_all(c)
            .await
        {
            Ok(r) => r
                .iter()
                .map(|r| (r.get("share_type"), r.get("share_with")))
                .collect(),
            Err(e) => {
                tracing::error!(fileid, uid, error = %e, "get_share_details: SQL error");
                return vec![];
            }
        }
    });

    // Batch-resolve display names for user-type shares (share_type = 0).
    let user_withs: Vec<String> = rows
        .iter()
        .filter(|(st, _)| *st == 0)
        .filter_map(|(_, sw)| sw.clone())
        .collect();
    let display_names = if !user_withs.is_empty() {
        batch_lookup_display_names(pool, prefix, &user_withs).await
    } else {
        std::collections::HashMap::new()
    };

    rows.iter()
        .map(|(share_type, share_with)| {
            let displayname = match *share_type {
                0 => share_with
                    .as_ref()
                    .and_then(|sw| display_names.get(sw.as_str()).cloned())
                    .unwrap_or_else(|| share_with.clone().unwrap_or_default()),
                _ => share_with.clone().unwrap_or_default(),
            };
            ShareDetail {
                share_type: *share_type,
                share_with: share_with.clone(),
                share_with_displayname: displayname,
            }
        })
        .collect()
}


pub(crate) async fn batch_lookup_display_names(
    pool: &DbPool,
    prefix: &str,
    uids: &[String],
) -> std::collections::HashMap<String, String> {
    if uids.is_empty() {
        return std::collections::HashMap::new();
    }

    let mut display_names: std::collections::HashMap<String, String> =
        std::collections::HashMap::new();
    let mut unresolved: Vec<&String> = Vec::new();

    // 1. Batch-query oc_users.displayname first — PHP's primary source
    //    (User::getDisplayName → backend); see lookup_user_display_name for
    //    why oc_accounts is only a (potentially stale) fallback.  Native
    //    text[] bind on Postgres (PHASE-22 T4).
    let user_rows: Vec<(String, Option<String>)> = match pool {
        DbPool::Pg(p) => {
            let sql = cached_sql(prefix, |prefix| {
                format!(
                    "SELECT uid, displayname FROM {prefix}users \
                 WHERE uid = ANY($1::text[])",
                    prefix = prefix
                )
            });
            sqlx::query::<Postgres>(&sql)
                .bind(uids)
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.get("uid"), r.get("displayname")))
                .collect()
        }
        DbPool::Sqlite(p) => {
            let placeholders = uids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("${}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT uid, displayname FROM {prefix}users WHERE uid IN ({placeholders})",
                prefix = prefix
            );
            let mut query = sqlx::query(&sql);
            for uid in uids {
                query = query.bind(uid);
            }
            query
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.get("uid"), r.get("displayname")))
                .collect()
        }
    };
    for (uid, dn) in user_rows {
        if let Some(dn) = dn.filter(|s| !s.is_empty()) {
            display_names.insert(uid, dn);
        }
    }

    // Collect UIDs with no oc_users displayname for the oc_accounts fallback.
    for uid in uids {
        if !display_names.contains_key(uid.as_str()) {
            unresolved.push(uid);
        }
    }

    // 2. Batch-query oc_accounts for the remaining UIDs (display names in
    //    JSON under data->'displayname'->>'value').  Native text[] bind on
    //    Postgres (PHASE-22 T4).
    if !unresolved.is_empty() {
        let account_rows: Vec<(String, String)> = match pool {
            DbPool::Pg(p) => {
                let sql = cached_sql(prefix, |prefix| {
                    format!(
                        "SELECT uid, data FROM {prefix}accounts \
                     WHERE uid = ANY($1::text[])",
                        prefix = prefix
                    )
                });
                let uids: Vec<String> = unresolved.iter().map(|s| s.as_str().to_string()).collect();
                sqlx::query::<Postgres>(&sql)
                    .bind(&uids)
                    .fetch_all(p)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        let uid: String = r.get("uid");
                        let data: String = r.get("data");
                        (uid, data)
                    })
                    .collect()
            }
            DbPool::Sqlite(p) => {
                let users_placeholders = unresolved
                    .iter()
                    .enumerate()
                    .map(|(i, _)| format!("${}", i + 1))
                    .collect::<Vec<_>>()
                    .join(", ");
                let sql = format!(
                    "SELECT uid, data FROM {prefix}accounts WHERE uid IN ({users_placeholders})",
                    prefix = prefix
                );
                let mut query = sqlx::query(&sql);
                for uid in &unresolved {
                    query = query.bind(uid);
                }
                query
                    .fetch_all(p)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|r| {
                        let uid: String = r.get("uid");
                        let data: String = r.get("data");
                        (uid, data)
                    })
                    .collect()
            }
        };
        for (uid, data) in &account_rows {
            if let Some(dn) = extract_displayname_from_accounts_json(data) {
                display_names.entry(uid.clone()).or_insert(dn);
            }
        }
        // 3. For any UIDs still unresolved, fall back to the UID itself.
        for uid in &unresolved {
            display_names
                .entry((*uid).clone())
                .or_insert_with(|| (*uid).clone());
        }
    }

    // 4. For UIDs that had no oc_users row and no oc_accounts row, fall back to UID.
    for uid in uids {
        display_names
            .entry(uid.clone())
            .or_insert_with(|| uid.clone());
    }

    display_names
}


/// Extract the display name from an `oc_accounts.data` JSON value.
///
/// PHP stores: `{"displayname":{"value":"Tan Siew Kin","scope":"...","verified":"0"},...}`
pub(crate) fn extract_displayname_from_accounts_json(data: &str) -> Option<String> {
    // Use simple JSON parsing via serde_json.
    let parsed: serde_json::Value = serde_json::from_str(data).ok()?;
    parsed
        .get("displayname")?
        .get("value")?
        .as_str()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}


/// Format share types as XML matching PHP `ShareTypeList::xmlSerialize()`.
///
/// Empty when no shares → `<oc:share-types/>` (self-closing, handled by the
/// fact we emit content as raw inner XML).
pub fn format_share_types_xml(types: &[i32]) -> String {
    if types.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for t in types {
        xml.push_str(&format!(
            "<oc:share-type xmlns:oc=\"http://owncloud.org/ns\">{t}</oc:share-type>"
        ));
    }
    xml
}


/// Format sharees as XML matching PHP `ShareeList::xmlSerialize()`.
pub fn format_sharees_xml(details: &[ShareDetail]) -> String {
    if details.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for d in details {
        xml.push_str(&format!(
            "<nc:sharee xmlns:nc=\"http://nextcloud.org/ns\">\
             <nc:id>{}</nc:id>\
             <nc:display-name>{}</nc:display-name>\
             <nc:type>{}</nc:type>\
             </nc:sharee>",
            d.share_with.as_deref().unwrap_or(""),
            d.share_with_displayname,
            d.share_type,
        ));
    }
    xml
}
