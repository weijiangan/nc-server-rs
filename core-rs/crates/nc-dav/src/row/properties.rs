use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use super::paths::subtree_suffix_offset;
use super::sql::cached_sql;
use sqlx::{Postgres, Row, Sqlite};


// ─── oc_properties helpers (task §10.11) ─────────────────────────────────────

/// Parse Clark notation `{namespace}name` → `("namespace", "name")`.
pub fn parse_clark_notation(s: &str) -> Option<(&str, &str)> {
    let inner = s.strip_prefix('{')?;
    let (ns, name) = inner.split_once('}')?;
    Some((ns, name))
}


/// Format a path for `oc_properties.propertypath` (VARCHAR 255).
///
/// Hashes with SHA-1 when the path exceeds 250 bytes, matching PHP's
/// `CustomPropertiesBackend::formatPath()`.
pub fn format_property_path(path: &str) -> String {
    if path.len() > 250 {
        use sha1::{Digest, Sha1};
        let mut hasher = Sha1::new();
        hasher.update(path.as_bytes());
        format!("{:x}", hasher.finalize())
    } else {
        path.to_string()
    }
}


/// List custom properties for a user + path from `oc_properties`.
pub async fn list_custom_properties(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
) -> Vec<(String, String, i16)> {
    let prop_path = format_property_path(path);
    let sql = format!(
        "SELECT propertyname, propertyvalue, valuetype \
         FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2"
    );
    let fetched: Result<Vec<(String, String, i16)>, sqlx::Error> = db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(userid)
            .bind(&prop_path)
            .fetch_all(c)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|r| {
                        (
                            r.try_get::<String, _>("propertyname").unwrap_or_default(),
                            r.try_get::<String, _>("propertyvalue").unwrap_or_default(),
                            r.try_get::<i16, _>("valuetype").unwrap_or(1),
                        )
                    })
                    .collect()
            })
    });
    match fetched {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(error = %e, "list_custom_properties query failed");
            vec![]
        }
    }
}


/// List custom properties for a user and a **batch** of paths in one query.
///
/// Same semantics as `list_custom_properties` (including the >250-char path
/// hash from `format_property_path`); the returned map is keyed by the raw
/// (unhashed) path as passed in.  Paths without properties are absent.
pub async fn custom_properties_batch(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    paths: &[String],
) -> std::collections::HashMap<String, Vec<(String, String, i16)>> {
    if paths.is_empty() {
        return std::collections::HashMap::new();
    }
    // Key the map by the caller's raw path, query by the formatted path.
    let raw_by_formatted: std::collections::HashMap<String, &str> = paths
        .iter()
        .map(|p| (format_property_path(p), p.as_str()))
        .collect();
    // Native text[] bind on Postgres (PHASE-22 T4.2) — the safe array form
    // for raw fc paths (filenames may contain commas).  $1 is the userid
    // (bound first).
    let rows: Vec<(String, String, String, i16)> = match pool {
        DbPool::Pg(p) => {
            let sql = cached_sql(prefix, |prefix| {
                format!(
                    "SELECT propertypath, propertyname, propertyvalue, valuetype \
                 FROM {prefix}properties \
                 WHERE userid = $1 AND propertypath = ANY($2::text[])",
                    prefix = prefix,
                )
            });
            let formatted: Vec<String> = paths.iter().map(|p| format_property_path(p)).collect();
            sqlx::query::<Postgres>(&sql)
                .bind(userid)
                .bind(&formatted)
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    (
                        r.get("propertypath"),
                        r.get("propertyname"),
                        r.get("propertyvalue"),
                        r.get("valuetype"),
                    )
                })
                .collect()
        }
        DbPool::Sqlite(p) => {
            // $1 is the userid (bound first); the IN list starts at $2.
            let placeholders = (2..=paths.len() + 1)
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT propertypath, propertyname, propertyvalue, valuetype \
                 FROM {prefix}properties \
                 WHERE userid = $1 AND propertypath IN ({placeholders})",
                prefix = prefix,
            );
            let mut query = sqlx::query(&sql).bind(userid);
            for p in paths {
                query = query.bind(format_property_path(p));
            }
            query
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    (
                        r.get("propertypath"),
                        r.get("propertyname"),
                        r.get("propertyvalue"),
                        r.get("valuetype"),
                    )
                })
                .collect()
        }
    };
    let mut out: std::collections::HashMap<String, Vec<(String, String, i16)>> =
        std::collections::HashMap::new();
    for (prop_path, propname, propvalue, valuetype) in rows {
        if let Some(raw) = raw_by_formatted.get(prop_path.as_str()) {
            out.entry((*raw).to_string())
                .or_default()
                .push((propname, propvalue, valuetype));
        }
    }
    out
}


/// Upsert a custom property — delete-then-insert to avoid PK / composite-key
/// complexity across SQLite and PostgreSQL.
pub async fn upsert_custom_property(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
    propname: &str,
    value_xml: &[u8],
    valuetype: i16,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let val_str = std::str::from_utf8(value_xml).unwrap_or("");
    let del_sql = format!(
        "DELETE FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2 AND propertyname=$3"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&del_sql)
            .bind(userid)
            .bind(&prop_path)
            .bind(propname)
            .execute(c)
            .await
            .map(|_| ())?
    });
    let ins_sql = format!(
        "INSERT INTO {prefix}properties \
         (userid, propertypath, propertyname, propertyvalue, valuetype) \
         VALUES ($1,$2,$3,$4,$5)"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&ins_sql)
            .bind(userid)
            .bind(&prop_path)
            .bind(propname)
            .bind(val_str)
            .bind(valuetype)
            .execute(c)
            .await
            .map(|_| ())?
    });
    Ok(())
}


/// Delete a single custom property by name.
pub async fn delete_custom_property(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
    propname: &str,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let sql = format!(
        "DELETE FROM {prefix}properties \
         WHERE userid=$1 AND propertypath=$2 AND propertyname=$3"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(userid)
            .bind(&prop_path)
            .bind(propname)
            .execute(c)
            .await
            .map(|_| ())?
    });
    Ok(())
}


/// Delete all custom properties for an exact path (single file/node delete).
pub async fn delete_custom_properties_for_path(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    path: &str,
) -> anyhow::Result<()> {
    let prop_path = format_property_path(path);
    let sql = format!("DELETE FROM {prefix}properties WHERE userid=$1 AND propertypath=$2");
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(userid)
            .bind(&prop_path)
            .execute(c)
            .await
            .map(|_| ())?
    });
    Ok(())
}


/// Delete custom properties for a directory and all its descendants.
///
/// On Postgres one `DELETE … propertypath = $dir OR propertypath LIKE $dir/%`
/// clears every raw (unhashed) descendant path; only the >250-byte paths,
/// which `format_property_path` stores as a SHA-1 digest and so cannot be
/// matched by prefix, still need the per-path fetch-and-delete (task 24.6).
/// SQLite keeps the fetch-and-loop for the whole subtree.
pub async fn delete_custom_properties_for_dir(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    storage_id: i64,
    dir_fc_path: &str,
) {
    let like_pat = format!("{dir_fc_path}/%");

    if let DbPool::Pg(p) = pool {
        let sql = format!(
            "DELETE FROM {prefix}properties \
             WHERE userid = $1 AND (propertypath = $2 OR propertypath LIKE $3)"
        );
        if let Err(e) = sqlx::query::<Postgres>(&sql)
            .bind(userid)
            .bind(format_property_path(dir_fc_path))
            .bind(&like_pat)
            .execute(p)
            .await
        {
            tracing::warn!(dir = %dir_fc_path, error = %e, "delete_custom_properties_for_dir: bulk DELETE failed");
        }
    }

    // Postgres: only the hashed (>250-byte) descendants are left; SQLite: all
    // of them.
    let child_paths: Vec<String> = match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT path FROM {prefix}filecache \
                 WHERE storage = $1 AND path LIKE $2 AND octet_length(path) > 250"
            );
            match sqlx::query_scalar::<Postgres, String>(&sql)
                .bind(storage_id)
                .bind(&like_pat)
                .fetch_all(p)
                .await
            {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(dir = %dir_fc_path, error = %e, "delete_custom_properties_for_dir: hashed-path fetch failed");
                    return;
                }
            }
        }
        DbPool::Sqlite(p) => {
            let sql = format!(
                "SELECT path FROM {prefix}filecache \
                 WHERE storage = $1 AND (path = $2 OR path LIKE $3)"
            );
            match sqlx::query_scalar::<Sqlite, String>(&sql)
                .bind(storage_id)
                .bind(dir_fc_path)
                .bind(&like_pat)
                .fetch_all(p)
                .await
            {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(dir = %dir_fc_path, error = %e, "delete_custom_properties_for_dir: descendant fetch failed");
                    return;
                }
            }
        }
    };
    for child_path in child_paths {
        if let Err(e) = delete_custom_properties_for_path(pool, prefix, userid, &child_path).await {
            tracing::warn!(path = %child_path, error = %e, "delete_custom_properties_for_dir: per-path DELETE failed");
        }
    }
}


/// Update `propertypath` for a single node (rename).
pub async fn update_custom_properties_path(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    old_path: &str,
    new_path: &str,
) -> anyhow::Result<()> {
    let old_prop = format_property_path(old_path);
    let new_prop = format_property_path(new_path);
    let sql = format!(
        "UPDATE {prefix}properties SET propertypath=$1 \
         WHERE userid=$2 AND propertypath=$3"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(&new_prop)
            .bind(userid)
            .bind(&old_prop)
            .execute(c)
            .await
            .map(|_| ())?
    });
    Ok(())
}


/// Update `propertypath` for a directory subtree (rename).
///
/// On Postgres one set-based UPDATE rekeys every descendant whose old **and**
/// new `propertypath` are raw paths; rows on either side of
/// `format_property_path`'s 250-byte SHA-1 threshold cannot be expressed in
/// SQL (the digest is Rust-side) and fall back to the per-path update
/// (task 24.2).  SQLite keeps the fetch-and-loop for the whole subtree.
pub async fn update_custom_properties_path_subtree(
    pool: &DbPool,
    prefix: &str,
    userid: &str,
    storage_id: i64,
    old_prefix: &str,
    new_prefix: &str,
) {
    let like_pat = format!("{old_prefix}/%");
    let offset = subtree_suffix_offset(old_prefix);

    let old_child_paths: Vec<String> = match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "UPDATE {prefix}properties \
                 SET propertypath = $1::text || SUBSTRING(propertypath FROM $2::int) \
                 WHERE userid = $3 AND propertypath LIKE $4 \
                   AND octet_length(propertypath) <= 250 \
                   AND octet_length($1::text || SUBSTRING(propertypath FROM $2::int)) <= 250"
            );
            if let Err(e) = sqlx::query::<Postgres>(&sql)
                .bind(new_prefix)
                .bind(offset)
                .bind(userid)
                .bind(&like_pat)
                .execute(p)
                .await
            {
                tracing::warn!(old_prefix, new_prefix, error = %e, "update_custom_properties_path_subtree: bulk UPDATE failed");
            }
            // Whatever the bulk UPDATE could not express: a descendant whose
            // old path is hashed, or whose new path crosses the threshold.
            let sql_rest = format!(
                "SELECT path FROM {prefix}filecache \
                 WHERE storage = $1 AND path LIKE $2 \
                   AND (octet_length(path) > 250 \
                        OR octet_length($3::text || SUBSTRING(path FROM $4::int)) > 250)"
            );
            match sqlx::query_scalar::<Postgres, String>(&sql_rest)
                .bind(storage_id)
                .bind(&like_pat)
                .bind(new_prefix)
                .bind(offset)
                .fetch_all(p)
                .await
            {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(old_prefix, error = %e, "update_custom_properties_path_subtree: hashed-path fetch failed");
                    return;
                }
            }
        }
        DbPool::Sqlite(p) => {
            let sql =
                format!("SELECT path FROM {prefix}filecache WHERE storage = $1 AND path LIKE $2");
            match sqlx::query_scalar::<Sqlite, String>(&sql)
                .bind(storage_id)
                .bind(&like_pat)
                .fetch_all(p)
                .await
            {
                Ok(paths) => paths,
                Err(e) => {
                    tracing::warn!(old_prefix, error = %e, "update_custom_properties_path_subtree: descendant fetch failed");
                    return;
                }
            }
        }
    };

    for old_child_path in old_child_paths {
        let new_child_path = old_child_path.replacen(old_prefix, new_prefix, 1);
        if let Err(e) =
            update_custom_properties_path(pool, prefix, userid, &old_child_path, &new_child_path)
                .await
        {
            tracing::warn!(path = %old_child_path, error = %e, "update_custom_properties_path_subtree: per-path UPDATE failed");
        }
    }
}
