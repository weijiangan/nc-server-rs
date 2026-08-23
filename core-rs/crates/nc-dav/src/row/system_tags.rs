use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Row};


// ─── Phase 12.7: system tags ───────────────────────────────────────────────────

/// One system tag row from `oc_systemtag` joined with `oc_systemtag_object_mapping`.
#[derive(Debug, Clone)]
pub struct SystemTagRow {
    pub id: i64,
    pub name: String,
    pub user_visible: bool,
    pub user_assignable: bool,
    pub color: Option<String>,
}


/// Return system tags for a file, matching PHP `SystemTagPlugin::getTagsForFile()`.
///
/// Tags are filtered for user visibility and sorted by natural-sort name
/// (we approximate with case-insensitive alphanumeric order).
pub async fn get_system_tags_for_file(
    pool: &DbPool,
    prefix: &str,
    fileid: i64,
) -> Vec<SystemTagRow> {
    let sql = format!(
        "SELECT t.id, t.name, t.visibility, t.editable, t.color \
         FROM {prefix}systemtag t \
         JOIN {prefix}systemtag_object_mapping m \
           ON m.systemtagid = t.id \
         WHERE m.objectid = $1 AND m.objecttype = 'files' \
         AND t.visibility = 1 \
         ORDER BY LOWER(t.name)",
        prefix = prefix
    );
    let fetched: Result<Vec<SystemTagRow>, sqlx::Error> = db_dispatch!(pool, |Db, c| {
        sqlx::query::<Db>(&sql)
            .bind(fileid.to_string())
            .fetch_all(c)
            .await
            .map(|rows| {
                rows.iter()
                    .map(|r| SystemTagRow {
                        id: r.get("id"),
                        name: r.get("name"),
                        user_visible: r.get::<i16, _>("visibility") == 1,
                        user_assignable: r.get::<i16, _>("editable") == 1,
                        color: r.get("color"),
                    })
                    .collect()
            })
    });
    match fetched {
        Ok(rows) => rows,
        Err(e) => {
            tracing::error!(fileid, error = %e, "get_system_tags_for_file: SQL error");
            vec![]
        }
    }
}


/// System tags for a **batch** of files in one query, keyed by fileid.
///
/// Mirrors `get_system_tags_for_file` (user-visible only, sorted by
/// case-insensitive name); the per-file order matches the single query.
/// Files without tags are absent from the map.
pub async fn system_tags_batch(
    pool: &DbPool,
    prefix: &str,
    fileids: &[i64],
) -> std::collections::HashMap<i64, Vec<SystemTagRow>> {
    if fileids.is_empty() {
        return std::collections::HashMap::new();
    }
    // Native text[] bind on Postgres (PHASE-22 T4): objectid is a text
    // column, so the ids bind as strings.
    let rows: Vec<(String, i64, String, i16, i16, Option<String>)> = match pool {
        DbPool::Pg(p) => {
            let sql = format!(
                "SELECT m.objectid, t.id, t.name, t.visibility, t.editable, t.color \
                 FROM {prefix}systemtag t \
                 JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id \
                 WHERE m.objectid = ANY($1::text[]) AND m.objecttype = 'files' \
                 AND t.visibility = 1 \
                 ORDER BY LOWER(t.name)",
                prefix = prefix,
            );
            let ids: Vec<String> = fileids.iter().map(i64::to_string).collect();
            sqlx::query::<Postgres>(&sql)
                .bind(&ids)
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    (
                        r.get("objectid"),
                        r.get("id"),
                        r.get("name"),
                        r.get("visibility"),
                        r.get("editable"),
                        r.get("color"),
                    )
                })
                .collect()
        }
        DbPool::Sqlite(p) => {
            let placeholders = (1..=fileids.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT m.objectid, t.id, t.name, t.visibility, t.editable, t.color \
                 FROM {prefix}systemtag t \
                 JOIN {prefix}systemtag_object_mapping m ON m.systemtagid = t.id \
                 WHERE m.objectid IN ({placeholders}) AND m.objecttype = 'files' \
                 AND t.visibility = 1 \
                 ORDER BY LOWER(t.name)",
                prefix = prefix,
            );
            let mut query = sqlx::query(&sql);
            for id in fileids {
                query = query.bind(id.to_string());
            }
            query
                .fetch_all(p)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|r| {
                    (
                        r.get("objectid"),
                        r.get("id"),
                        r.get("name"),
                        r.get("visibility"),
                        r.get("editable"),
                        r.get("color"),
                    )
                })
                .collect()
        }
    };
    let mut out: std::collections::HashMap<i64, Vec<SystemTagRow>> =
        std::collections::HashMap::new();
    for (object_id, id, name, visibility, editable, color) in rows {
        let fileid = object_id.parse::<i64>().unwrap_or(0);
        out.entry(fileid).or_default().push(SystemTagRow {
            id,
            name,
            user_visible: visibility == 1,
            user_assignable: editable == 1,
            color,
        });
    }
    out
}


/// Format system tags as XML matching PHP `SystemTagList::xmlSerialize()`.
///
/// PHP wraps in `<nc:system-tags>` with child `<nc:system-tag>` elements.
/// Each tag element contains `{oc:}id`, `{nc:}display-name`, `{nc:}user-visible`,
/// `{nc:}user-assignable`, `{nc:}can-assign`, and optionally `{nc:}color`.
pub fn format_system_tags_xml(tags: &[SystemTagRow], _can_assign_all: bool) -> String {
    if tags.is_empty() {
        return String::new();
    }
    let mut xml = String::new();
    for t in tags {
        let color_attr = t
            .color
            .as_ref()
            .filter(|c| !c.is_empty())
            .map(|c| format!("<nc:color>{c}</nc:color>"))
            .unwrap_or_default();
        xml.push_str(&format!(
            "<nc:system-tag xmlns:nc=\"http://nextcloud.org/ns\">\
             <oc:id xmlns:oc=\"http://owncloud.org/ns\">{id}</oc:id>\
             <nc:display-name>{name}</nc:display-name>\
             <nc:user-visible>{uv}</nc:user-visible>\
             <nc:user-assignable>{ua}</nc:user-assignable>\
             <nc:can-assign>{ua}</nc:can-assign>\
             {color}\
             </nc:system-tag>",
            id = t.id,
            name = t.name,
            uv = if t.user_visible { "true" } else { "false" },
            ua = if t.user_assignable { "true" } else { "false" },
            color = color_attr,
        ));
    }
    xml
}
