//! Favorites & personal tags — backed by `oc_vcategory` / `oc_vcategory_to_object`.
//!
//! PHP reference: `lib/private/Tags.php` (ITags impl), `apps/dav/lib/Connector/Sabre/TagsPlugin.php`.
//!
//! ## Data model
//!
//! - `oc_vcategory`: one row per (uid, type, category). The type for files is
//!   `"files"`.  The favorite sentinel is `TAG_FAVORITE = "_$!<Favorite>!$_"`,
//!   stored as a regular category row.
//! - `oc_vcategory_to_object`: maps `objid` (filecache fileid) ↔ `categoryid`.
//!   PK is `(categoryid, objid, type)`.
//!
//! ## Read path (PROPFIND)
//!
//! `{oc:}favorite` = `1` / `0` based on whether `TAG_FAVORITE` is among the
//! node's tags.  `{oc:}tags` = tag names excluding the favorite sentinel.
//! Both are served inline during PROPFIND; depth-1 prefetch batches the lookup
//! for all children into a single DB query.
//!
//! ## Write path (PROPPATCH)
//!
//! - `{oc:}favorite`: truthy test `(int)state === 1 || state === 'true'` —
//!   `tagAs(TAG_FAVORITE)` or `unTag(TAG_FAVORITE)`.  Returns 200 (or 204 on
//!   delete, i.e. `null` body).
//! - `{oc:}tags`: diffs current vs requested tag names, skipping the favorite
//!   sentinel (`TagsPlugin.php:180-200`).
//!
//! ## Deviations from PHP
//!
//! 1. **No event dispatch.** PHP dispatches `NodeAddedToFavorite` /
//!    `NodeRemovedFromFavorite` events.  Rust has no event system; verified zero
//!    external listeners affect the DAV response.
//! 2. **Shared-owner tags not merged.** PHP's `TagManager::load('files')` may
//!    include shared owners' tags.  Rust tags are always scoped to the
//!    requesting user.
//! 3. **No `path` argument for favorite events.** The PHP `tagAs`/`unTag` accept
//!    an optional `$path` for event dispatch.  Rust skips event dispatch, so
//!    path resolution is unnecessary.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use nc_db::pool::DbPool;
use sqlx::Row;

/// Sentinel tag name that marks a file as favorited.
pub const TAG_FAVORITE: &str = "_$!<Favorite>!$_";

/// The object type for file tags (matches PHP `ITags` type `'files'`).
const OBJ_TYPE: &str = "files";

// ─── Tag cache ─────────────────────────────────────────────────────────────────

/// Per-request tag cache, keyed by fileid.
///
/// Values are raw tag names INCLUDING the favorite sentinel if present.
/// Callers filter it out when building the `{oc:}tags` property.
pub type TagCache = Arc<Mutex<HashMap<i64, Vec<String>>>>;

pub fn new_tag_cache() -> TagCache {
    Arc::new(Mutex::new(HashMap::new()))
}

// ─── Read path ─────────────────────────────────────────────────────────────────

/// Result of fetching tags for a set of file IDs: `(tags, is_favorite)`.
/// `tags` excludes the favorite sentinel.
#[derive(Debug, Clone)]
pub struct TagInfo {
    pub tags: Vec<String>,
    pub is_favorite: bool,
}

/// Fetch tags for a **batch** of file IDs in a single query.
///
/// Returns a map from fileid → list of tag names (INCLUDING the favorite
/// sentinel if present).  File IDs with no tags are absent from the map.
///
/// Matches PHP `ITags::getTagsForObjects()` (`lib/private/Tags.php:145-180`).
pub async fn get_tags_batch(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileids: &[i64],
) -> HashMap<i64, Vec<String>> {
    if fileids.is_empty() {
        return HashMap::new();
    }

    let mut result: HashMap<i64, Vec<String>> = HashMap::new();

    // PHP chunks at 900; we do a single query since Rust has no 65k placeholder limit.
    // Parameters 1-3 are uid, type, type — IN-clause placeholders start at $4.
    let placeholders: Vec<String> = (4..=fileids.len() + 3).map(|i| format!("${i}")).collect();
    let ph_str = placeholders.join(", ");

    let sql = format!(
        "SELECT vco.objid, vc.category \
         FROM {prefix}vcategory_to_object vco \
         JOIN {prefix}vcategory vc ON vco.categoryid = vc.id \
         WHERE vc.uid = $1 \
           AND vc.type = $2 \
           AND vco.type = $3 \
           AND vco.objid IN ({ph_str})"
    );

    let fetched: Result<Vec<(i64, String)>, sqlx::Error> = match pool {
        DbPool::Pg(p) => {
            let mut query = sqlx::query::<sqlx::Postgres>(&sql)
                .bind(uid)
                .bind(OBJ_TYPE)
                .bind(OBJ_TYPE);
            for id in fileids {
                query = query.bind(*id);
            }
            query.fetch_all(p).await.map(|rows| {
                rows.iter()
                    .map(|r| {
                        let objid: i64 = r.get("objid");
                        let category: String = r.get("category");
                        (objid, category)
                    })
                    .collect()
            })
        }
        DbPool::Sqlite(p) => {
            let mut query = sqlx::query::<sqlx::Sqlite>(&sql)
                .bind(uid)
                .bind(OBJ_TYPE)
                .bind(OBJ_TYPE);
            for id in fileids {
                query = query.bind(*id);
            }
            query.fetch_all(p).await.map(|rows| {
                rows.iter()
                    .map(|r| {
                        let objid: i64 = r.get("objid");
                        let category: String = r.get("category");
                        (objid, category)
                    })
                    .collect()
            })
        }
    };

    let rows = match fetched {
        Ok(r) => r,
        Err(e) => {
            tracing::error!(error = %e, uid = %uid, "get_tags_batch: SQL error");
            return result;
        }
    };

    for (objid, category) in rows {
        result.entry(objid).or_default().push(category);
    }

    result
}

/// Extract `TagInfo` from a raw tag list (which may include the favorite sentinel).
pub fn parse_tag_info(tags: &[String]) -> TagInfo {
    let mut is_favorite = false;
    let mut clean = Vec::with_capacity(tags.len());
    for t in tags {
        if t == TAG_FAVORITE {
            is_favorite = true;
        } else {
            clean.push(t.clone());
        }
    }
    TagInfo {
        tags: clean,
        is_favorite,
    }
}

// ─── Prefetch helper ───────────────────────────────────────────────────────────

/// Prefetch tags for a set of file IDs and store in the cache.
///
/// Called during `read_dir` to batch-load tags before `get_props` runs for each
/// child.  The cache is populated even for file IDs that have no tags (so that
/// subsequent `get_props` calls find them and don't re-query).
pub async fn prefetch_tags(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileids: &[i64],
    cache: &TagCache,
) {
    if fileids.is_empty() {
        return;
    }

    let tags_map = get_tags_batch(pool, prefix, uid, fileids).await;

    let mut cache_guard = cache.lock().expect("tag cache lock");
    for &fid in fileids {
        let tags = tags_map.get(&fid).cloned().unwrap_or_default();
        cache_guard.insert(fid, tags);
    }
}

/// Get tag info for a single fileid, reading from cache if available, otherwise
/// querying the DB and populating the cache.
pub async fn get_tag_info(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
    cache: &TagCache,
) -> TagInfo {
    // Check cache first.
    {
        let cache_guard = cache.lock().expect("tag cache lock");
        if let Some(tags) = cache_guard.get(&fileid) {
            return parse_tag_info(tags);
        }
    }

    // Cache miss — query and store.
    let tags_map = get_tags_batch(pool, prefix, uid, &[fileid]).await;
    let tags = tags_map.get(&fileid).cloned().unwrap_or_default();

    {
        let mut cache_guard = cache.lock().expect("tag cache lock");
        cache_guard.entry(fileid).or_insert(tags.clone());
    }

    parse_tag_info(&tags)
}

// ─── Write path ────────────────────────────────────────────────────────────────

/// Get or create a category row for the given tag name, returning its `id`.
///
/// Matches PHP `Tags::add()` — trims the name, inserts if not present.
/// Returns `None` on empty name or database error.
async fn get_or_create_category(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    tag_name: &str,
) -> Option<i64> {
    let name = tag_name.trim();
    if name.is_empty() {
        return None;
    }

    // Try INSERT … ON CONFLICT DO NOTHING, then SELECT the id.
    // PostgreSQL supports ON CONFLICT; SQLite supports ON CONFLICT DO NOTHING as of 3.24.
    let insert_sql = format!(
        "INSERT INTO {prefix}vcategory (uid, type, category) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (uid, type, category) DO NOTHING",
    );
    let result = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&insert_sql)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&insert_sql)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .execute(p)
            .await
            .map(|_| ()),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, uid = %uid, tag = %name, "Failed to insert vcategory row");
    }

    // Read back the ID (whether we inserted it or it already existed).
    let select_sql =
        format!("SELECT id FROM {prefix}vcategory WHERE uid = $1 AND type = $2 AND category = $3");
    let fetched = match pool {
        DbPool::Pg(p) => sqlx::query_scalar::<sqlx::Postgres, _>(&select_sql)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .fetch_optional(p)
            .await,
        DbPool::Sqlite(p) => sqlx::query_scalar::<sqlx::Sqlite, _>(&select_sql)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .fetch_optional(p)
            .await,
    };
    match fetched {
        Ok(Some(id)) => Some(id),
        Ok(None) => {
            tracing::warn!(uid = %uid, tag = %name, "vcategory row not found after insert");
            None
        }
        Err(e) => {
            tracing::error!(error = %e, uid = %uid, tag = %name, "Failed to select vcategory id");
            None
        }
    }
}

/// Create a tag→object relation.
///
/// Matches PHP `Tags::tagAs()` — creates the category if needed, then inserts
/// the relation row.  Idempotent (ON CONFLICT DO NOTHING).
pub async fn tag_as(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
    tag: &str,
) -> Result<(), ()> {
    let category_id = get_or_create_category(pool, prefix, uid, tag)
        .await
        .ok_or(())?;

    let insert_sql = format!(
        "INSERT INTO {prefix}vcategory_to_object (objid, categoryid, type) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (categoryid, objid, type) DO NOTHING"
    );
    let result = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&insert_sql)
            .bind(fileid)
            .bind(category_id)
            .bind(OBJ_TYPE)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&insert_sql)
            .bind(fileid)
            .bind(category_id)
            .bind(OBJ_TYPE)
            .execute(p)
            .await
            .map(|_| ()),
    };
    if let Err(e) = result {
        tracing::warn!(error = %e, fileid = fileid, tag = %tag, "Failed to insert vcategory_to_object row");
        return Err(());
    }

    Ok(())
}

/// Remove a tag→object relation.
///
/// Matches PHP `Tags::unTag()` — deletes the relation row by objid + category
/// resolved from tag name.
pub async fn un_tag(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
    tag: &str,
) -> Result<(), ()> {
    let name = tag.trim();
    if name.is_empty() {
        return Err(());
    }

    let delete_sql = format!(
        "DELETE FROM {prefix}vcategory_to_object \
         WHERE objid = $1 AND type = $2 \
         AND categoryid = (\
             SELECT id FROM {prefix}vcategory \
             WHERE uid = $3 AND type = $4 AND category = $5\
         )"
    );
    let fetched = match pool {
        DbPool::Pg(p) => sqlx::query::<sqlx::Postgres>(&delete_sql)
            .bind(fileid)
            .bind(OBJ_TYPE)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .execute(p)
            .await
            .map(|_| ()),
        DbPool::Sqlite(p) => sqlx::query::<sqlx::Sqlite>(&delete_sql)
            .bind(fileid)
            .bind(OBJ_TYPE)
            .bind(uid)
            .bind(OBJ_TYPE)
            .bind(name)
            .execute(p)
            .await
            .map(|_| ()),
    };
    match fetched {
        Ok(()) => Ok(()),
        Err(e) => {
            tracing::warn!(error = %e, fileid = fileid, tag = %name, "Failed to delete vcategory_to_object row");
            Err(())
        }
    }
}

/// Set or unset the favorite sentinel for a file.
///
/// Matches PHP `TagsPlugin::handleUpdateProperties` for `{oc:}favorite`.
/// - `state == true`  → tagAs(TAG_FAVORITE)
/// - `state == false` → unTag(TAG_FAVORITE)
pub async fn set_favorite(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
    state: bool,
) -> Result<(), ()> {
    if state {
        tag_as(pool, prefix, uid, fileid, TAG_FAVORITE).await
    } else {
        un_tag(pool, prefix, uid, fileid, TAG_FAVORITE).await
    }
}

/// Apply a new set of tags to a file, computing the diff vs the current tags.
///
/// Matches PHP `TagsPlugin::updateTags()` (`TagsPlugin.php:180-200`):
/// - New tags = requested \ current → tagAs each (skip TAG_FAVORITE)
/// - Deleted tags = current \ requested → unTag each (skip TAG_FAVORITE)
///
/// `current_tags` should be the FULL list including the favorite sentinel.
/// `requested_tags` should be from the PROPPATCH body (serialized tag names).
pub async fn update_tags(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    fileid: i64,
    current_tags: &[String],
    requested_tags: &[String],
) {
    // Compute set differences manually (tags lists are small — O(n²) is fine).
    let to_add: Vec<&str> = requested_tags
        .iter()
        .map(|s| s.as_str())
        .filter(|t| !current_tags.iter().any(|c| c == t))
        .collect();
    let to_remove: Vec<&str> = current_tags
        .iter()
        .map(|s| s.as_str())
        .filter(|t| !requested_tags.iter().any(|r| r == t))
        .collect();

    for tag in &to_add {
        if *tag == TAG_FAVORITE {
            continue;
        }
        let _ = tag_as(pool, prefix, uid, fileid, tag).await;
    }
    for tag in &to_remove {
        if *tag == TAG_FAVORITE {
            continue;
        }
        let _ = un_tag(pool, prefix, uid, fileid, tag).await;
    }
}

// ─── PROPPATCH XML parsing ────────────────────────────────────────────────────

/// Parse the `{oc:}tags` XML value from a PROPPATCH body into a list of tag
/// names.
///
/// The PROPPATCH body for `{oc:}tags` looks like:
/// ```xml
/// <oc:tags xmlns:oc="http://owncloud.org/ns">
///   <oc:tag>tag1</oc:tag>
///   <oc:tag>tag2</oc:tag>
/// </oc:tags>
/// ```
///
/// Matches PHP `TagList::xmlDeserialize()`.
/// Extract the inner TEXT of a PROPPATCH prop element.
///
/// The dav-server passes the FULL serialized element in `DavProp.xml`
/// (`handle_props.rs::element_to_davprop_full`) — e.g.
/// `<oc:favorite xmlns:oc="http://owncloud.org/ns">1</oc:favorite>` — while
/// PHP's `TagsPlugin` compares the inner text (`"1"` / `"true"`).  Falls back
/// to the raw bytes as a string when the element doesn't parse.
pub fn prop_inner_text(xml: &[u8]) -> String {
    if let Ok(elem) = xmltree::Element::parse(std::io::Cursor::new(xml)) {
        let mut text = String::new();
        for child in &elem.children {
            if let xmltree::XMLNode::Text(t) = child {
                text.push_str(t);
            }
        }
        return text;
    }
    String::from_utf8_lossy(xml).to_string()
}

pub fn parse_tags_xml(xml: &[u8]) -> Vec<String> {
    let s = match std::str::from_utf8(xml) {
        Ok(s) => s,
        Err(_) => return vec![],
    };

    let mut tags = Vec::new();
    let tag_open = "<oc:tag>";
    let tag_close = "</oc:tag>";

    let mut rest = s;
    while let Some(start) = rest.find(tag_open) {
        let after_open = start + tag_open.len();
        rest = &rest[after_open..];
        if let Some(end) = rest.find(tag_close) {
            let tag_value = rest[..end].to_string();
            tags.push(tag_value);
            rest = &rest[end + tag_close.len()..];
        } else {
            break;
        }
    }

    tags
}

/// Format tags as the `{oc:}tags` XML **inner** content for PROPFIND responses.
///
/// This returns only the child `<oc:tag>` elements — the outer `<oc:tags>` wrapper
/// is added by `make_prop` (same pattern as `{oc:}checksums` / `<oc:checksum>`).
///
/// Produces:
/// ```xml
/// <oc:tag>t1</oc:tag><oc:tag>t2</oc:tag>
/// ```
pub fn format_tags_xml(tags: &[String]) -> String {
    let mut xml = String::new();
    for t in tags {
        // Simple escaping: & < >
        let escaped = t
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        xml.push_str("<oc:tag>");
        xml.push_str(&escaped);
        xml.push_str("</oc:tag>");
    }
    xml
}

// ─── Tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prop_inner_text_extracts_text_value() {
        // The dav-server passes the FULL serialized element (finding #15).
        let xml = br#"<oc:favorite xmlns:oc="http://owncloud.org/ns">1</oc:favorite>"#;
        assert_eq!(prop_inner_text(xml), "1");
        let xml = br#"<oc:tags xmlns:oc="http://owncloud.org/ns"><oc:tag>a</oc:tag></oc:tags>"#;
        assert_eq!(prop_inner_text(xml), "");
        assert_eq!(prop_inner_text(b"not xml at all"), "not xml at all");
    }

    #[test]
    fn parse_tag_info_no_favorite() {
        let tags = vec!["work".to_string(), "personal".to_string()];
        let info = parse_tag_info(&tags);
        assert_eq!(info.tags, vec!["work", "personal"]);
        assert!(!info.is_favorite);
    }

    #[test]
    fn parse_tag_info_with_favorite() {
        let tags = vec![
            "work".to_string(),
            TAG_FAVORITE.to_string(),
            "personal".to_string(),
        ];
        let info = parse_tag_info(&tags);
        assert_eq!(info.tags, vec!["work", "personal"]);
        assert!(info.is_favorite);
    }

    #[test]
    fn parse_tag_info_favorite_only() {
        let tags = vec![TAG_FAVORITE.to_string()];
        let info = parse_tag_info(&tags);
        assert!(info.tags.is_empty());
        assert!(info.is_favorite);
    }

    #[test]
    fn parse_tag_info_empty() {
        let tags: Vec<String> = vec![];
        let info = parse_tag_info(&tags);
        assert!(info.tags.is_empty());
        assert!(!info.is_favorite);
    }

    #[test]
    fn parse_tags_xml_basic() {
        let xml = b"<oc:tags xmlns:oc=\"http://owncloud.org/ns\"><oc:tag>work</oc:tag><oc:tag>personal</oc:tag></oc:tags>";
        let tags = parse_tags_xml(xml);
        assert_eq!(tags, vec!["work", "personal"]);
    }

    #[test]
    fn parse_tags_xml_empty() {
        let xml = b"<oc:tags xmlns:oc=\"http://owncloud.org/ns\"></oc:tags>";
        let tags = parse_tags_xml(xml);
        assert!(tags.is_empty());
    }

    #[test]
    fn parse_tags_xml_single() {
        let xml = b"<oc:tags xmlns:oc=\"http://owncloud.org/ns\"><oc:tag>only</oc:tag></oc:tags>";
        let tags = parse_tags_xml(xml);
        assert_eq!(tags, vec!["only"]);
    }

    #[test]
    fn format_tags_xml_basic() {
        let xml = format_tags_xml(&["work".to_string(), "personal".to_string()]);
        assert_eq!(xml, "<oc:tag>work</oc:tag><oc:tag>personal</oc:tag>");
    }

    #[test]
    fn format_tags_xml_empty() {
        let xml = format_tags_xml(&[]);
        assert_eq!(xml, "");
    }

    #[test]
    fn format_tags_xml_escapes_special_chars() {
        let xml = format_tags_xml(&["a<b".to_string(), "c&d".to_string()]);
        assert!(xml.contains("a&lt;b"));
        assert!(xml.contains("c&amp;d"));
    }

    #[test]
    fn tag_cache_new_is_empty() {
        let cache = new_tag_cache();
        let guard = cache.lock().unwrap();
        assert!(guard.is_empty());
    }
}
