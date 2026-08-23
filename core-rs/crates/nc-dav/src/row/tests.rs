use nc_db::pool::DbPool;
use sqlx::Sqlite;

use super::*;
use super::paths::subtree_suffix_offset;

/// The in-memory test DB is always SQLite; unwrap the variant for the
/// native queries below (tests never construct a Pg pool).
fn test_pool(pool: &DbPool) -> &sqlx::SqlitePool {
    match pool {
        DbPool::Sqlite(p) => p,
        DbPool::Pg(_) => panic!("test pools are sqlite"),
    }
}

// ── parse_clark_notation ──────────────────────────────────────────────

#[test]
fn parse_clark_notation_basic() {
    assert_eq!(
        parse_clark_notation("{urn:example}state"),
        Some(("urn:example", "state"))
    );
}

#[test]
fn parse_clark_notation_dav_namespace() {
    assert_eq!(
        parse_clark_notation("{DAV:}getetag"),
        Some(("DAV:", "getetag"))
    );
}

#[test]
fn parse_clark_notation_nc_namespace() {
    assert_eq!(
        parse_clark_notation("{http://nextcloud.org/ns}creation_time"),
        Some(("http://nextcloud.org/ns", "creation_time"))
    );
}

#[test]
fn parse_clark_notation_no_brace_returns_none() {
    assert_eq!(parse_clark_notation("no-brace"), None);
}

#[test]
fn parse_clark_notation_no_closing_brace_returns_none() {
    assert_eq!(parse_clark_notation("{nsname"), None);
}

#[test]
fn parse_clark_notation_empty_returns_none() {
    assert_eq!(parse_clark_notation(""), None);
}

// ── format_property_path ──────────────────────────────────────────────

#[test]
fn format_property_path_short_path_is_unchanged() {
    let path = "files/Documents/note.txt";
    assert_eq!(format_property_path(path), path);
}

#[test]
fn format_property_path_exactly_250_chars_is_unchanged() {
    let path = "f".repeat(250);
    assert_eq!(format_property_path(&path), path);
}

#[test]
fn format_property_path_251_chars_is_hashed() {
    let path = "f".repeat(251);
    let result = format_property_path(&path);
    // SHA-1 hex digest is 40 chars
    assert_eq!(result.len(), 40);
    assert_ne!(result, path);
}

#[test]
fn format_property_path_very_long_path_is_hashed() {
    let path = "files/".to_string() + &"x".repeat(500);
    let result = format_property_path(&path);
    assert_eq!(result.len(), 40);
}

#[test]
fn format_property_path_consistent_hash() {
    let path = "a".repeat(300);
    let a = format_property_path(&path);
    let b = format_property_path(&path);
    assert_eq!(a, b);
}

// ── Batch-vs-single consistency (Phase 18.1) ──────────────────────────
//
// The `*_batch` queries must return exactly what the per-node queries
// return; the difftest suite is the real gate, these pin the mapping.

/// In-memory SQLite with the tables the batch PROPFIND queries read.
async fn fresh_batch_db() -> DbPool {
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite"),
    );
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_filecache (
            fileid BIGINT NOT NULL PRIMARY KEY, storage BIGINT NOT NULL,
            path VARCHAR(4000) NOT NULL DEFAULT '', path_hash VARCHAR(32) NOT NULL DEFAULT '',
            parent BIGINT NOT NULL DEFAULT 0, name VARCHAR(250),
            mimetype BIGINT NOT NULL DEFAULT 0, mimepart BIGINT NOT NULL DEFAULT 0,
            size BIGINT NOT NULL DEFAULT 0, mtime BIGINT NOT NULL DEFAULT 0,
            storage_mtime BIGINT NOT NULL DEFAULT 0, etag VARCHAR(40),
            permissions INTEGER NOT NULL DEFAULT 0, checksum VARCHAR(255)
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("filecache");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_share (
            id BIGINT NOT NULL PRIMARY KEY, share_type SMALLINT NOT NULL DEFAULT 0,
            share_with VARCHAR(255), uid_owner VARCHAR(64) NOT NULL DEFAULT '',
            uid_initiator VARCHAR(64), file_source BIGINT, stime BIGINT NOT NULL DEFAULT 0,
            note TEXT NOT NULL DEFAULT ''
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("share");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_comments (
            id BIGINT NOT NULL PRIMARY KEY, object_type VARCHAR(64) NOT NULL DEFAULT '',
            object_id VARCHAR(64) NOT NULL DEFAULT '', actor_type VARCHAR(64) NOT NULL DEFAULT '',
            actor_id VARCHAR(64) NOT NULL DEFAULT '', creation_timestamp TIMESTAMP
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("comments");
    sqlx::query::<Sqlite>(
        // PK mirrors the live PostgreSQL schema (verified 2026-08-13) —
        // the de-correlated LEFT JOIN (T6.4) relies on it for at-most-one
        // marker row per (user, object).
        "CREATE TABLE oc_comments_read_markers (
            user_id VARCHAR(64) NOT NULL, object_type VARCHAR(64) NOT NULL DEFAULT '',
            object_id VARCHAR(64) NOT NULL DEFAULT '', marker_datetime TIMESTAMP,
            PRIMARY KEY (user_id, object_type, object_id)
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("markers");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_systemtag (
            id BIGINT NOT NULL PRIMARY KEY, name VARCHAR(255) NOT NULL DEFAULT '',
            visibility SMALLINT NOT NULL DEFAULT 1, editable SMALLINT NOT NULL DEFAULT 1,
            color VARCHAR(255)
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("systemtag");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_systemtag_object_mapping (
            objectid VARCHAR(64) NOT NULL DEFAULT '', objecttype VARCHAR(64) NOT NULL DEFAULT '',
            systemtagid BIGINT NOT NULL DEFAULT 0
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("mapping");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_users (uid VARCHAR(64) NOT NULL PRIMARY KEY, displayname VARCHAR(64))",
    )
    .execute(test_pool(&pool))
    .await
    .expect("users");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_properties (
            id INTEGER NOT NULL PRIMARY KEY, userid VARCHAR(64) NOT NULL DEFAULT '',
            propertypath VARCHAR(255) NOT NULL DEFAULT '', propertyname VARCHAR(255) NOT NULL DEFAULT '',
            propertyvalue TEXT NOT NULL DEFAULT '', valuetype SMALLINT NOT NULL DEFAULT 1
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("properties");
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_filecache_extended (
            fileid BIGINT NOT NULL PRIMARY KEY, metadata_etag VARCHAR(40),
            creation_time INTEGER NOT NULL DEFAULT 0, upload_time INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("filecache_extended");
    pool
}

#[tokio::test]
async fn with_ext_variants_match_singles() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    // dir (1) with two children: a.txt (has an extended row), b.txt (none).
    for (id, parent, name, mime) in [(1, 0, "files", 2), (2, 1, "a.txt", 1), (3, 1, "b.txt", 1)]
    {
        let path = format!("files/{name}");
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_filecache (fileid, storage, path, path_hash, parent, name, mimetype) \
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(1)
        .bind(&path)
        .bind(path_hash(&path))
        .bind(parent)
        .bind(name)
        .bind(mime)
        .execute(test_pool(&pool))
        .await
        .expect("insert");
    }
    sqlx::query::<Sqlite>(
        "INSERT INTO oc_filecache_extended (fileid, metadata_etag, creation_time, upload_time) \
         VALUES (?, ?, ?, ?)",
    )
    .bind(2)
    .bind("etag-42")
    .bind(42)
    .bind(43)
    .execute(test_pool(&pool))
    .await
    .expect("ext insert");

    // lookup variant: extended row present → real values; absent → zeros.
    let (row, ext) = lookup_by_path_with_ext(&pool, prefix, 1, "files/a.txt")
        .await
        .expect("a.txt");
    assert_eq!(row.fileid, 2);
    assert_eq!(ext.creation_time, 42);
    assert_eq!(ext.upload_time, 43);
    assert_eq!(ext.metadata_etag.as_deref(), Some("etag-42"));
    let (row, ext) = lookup_by_path_with_ext(&pool, prefix, 1, "files/b.txt")
        .await
        .expect("b.txt");
    assert_eq!(row.fileid, 3);
    assert_eq!(ext.creation_time, 0, "absent extended row → zero times");
    assert_eq!(ext.upload_time, 0);
    assert_eq!(ext.metadata_etag, None);
    assert!(
        lookup_by_path_with_ext(&pool, prefix, 1, "files/missing.txt")
            .await
            .is_none()
    );

    // list variant: same values through the fileid-keyed map, consistent
    // with the single-query pair for every child.
    let (rows, map) = list_children_with_ext(&pool, prefix, 1, 1).await;
    assert_eq!(rows.len(), 2);
    assert_eq!(map.get(&2).unwrap().creation_time, 42);
    assert_eq!(map.get(&3).unwrap().creation_time, 0);
    for r in &rows {
        let single_row = lookup_by_path(&pool, prefix, 1, r.path.as_deref().unwrap())
            .await
            .expect("single row");
        let single_ext = get_extended(&pool, prefix, r.fileid).await;
        let joined_ext = map.get(&r.fileid).expect("joined ext");
        assert_eq!(
            joined_ext.creation_time, single_ext.creation_time,
            "fileid {}",
            r.fileid
        );
        assert_eq!(joined_ext.upload_time, single_ext.upload_time);
        assert_eq!(joined_ext.metadata_etag, single_ext.metadata_etag);
        assert_eq!(single_row.fileid, r.fileid);
    }
}

#[tokio::test]
async fn count_children_batch_matches_single() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    // mimetype 2 = directory, 1 = file, all on storage 1.
    for (id, parent, name, mime) in [
        (1, 0, "files", 2),
        (2, 1, "a", 2),     // dir with one subdir + one file
        (3, 1, "b", 2),     // empty dir
        (4, 1, "c.txt", 1), // file
        (5, 2, "x.txt", 1),
        (6, 2, "sub", 2),
    ] {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_filecache (fileid, storage, path, parent, name, mimetype) \
             VALUES (?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(1)
        .bind(format!("files/{name}"))
        .bind(parent)
        .bind(name)
        .bind(mime)
        .execute(test_pool(&pool))
        .await
        .expect("insert");
    }
    let batch = count_children_batch(&pool, prefix, &[2, 3, 4], 1, 2).await;
    assert_eq!(batch.get(&2), Some(&(1, 1)), "a: sub + x.txt");
    assert_eq!(batch.get(&3), None, "empty dir absent");
    assert_eq!(batch.get(&4), None, "file absent");
    for id in [2, 3, 4] {
        let single = count_children(&pool, prefix, id, 1, 2).await;
        assert_eq!(
            batch.get(&id).copied().unwrap_or((0, 0)),
            single,
            "fileid {id}"
        );
    }
}

#[tokio::test]
async fn share_details_and_notes_batch_matches_singles() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    // alice owns files 10/11/12; bob shares his file 11 WITH alice.
    // (id, share_type, share_with, uid_owner, uid_initiator, file_source, stime, note)
    for (id, stype, swith, owner, init, fs, stime, note) in [
        (1, 0, "bob", "alice", "alice", 10, 100, ""), // detail, no note
        (2, 1, "staff", "alice", "alice", 10, 200, "staff-note"), // detail + note
        (3, 0, "erin", "alice", "alice", 10, 300, "erin-note"), // detail + most-recent note
        (4, 0, "alice", "bob", "bob", 11, 100, "bob-note"), // detail + note
        (5, 5, "x", "carol", "carol", 11, 500, "carol-note"), // outside details filter,
        // but the most-recent note on file 11 — notes must still see it
        (6, 0, "dave", "alice", "alice", 12, 500, ""), // detail, empty note at
        // the highest stime — must not hide the older note below
        (7, 0, "frank", "alice", "alice", 12, 400, "frank-note"), // detail + note
        (8, 1, "staff", "alice", "alice", 12, 300, ""),           // detail, no note
    ] {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_share (id, share_type, share_with, uid_owner, uid_initiator, file_source, stime, note) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(id)
        .bind(stype)
        .bind(swith)
        .bind(owner)
        .bind(init)
        .bind(fs)
        .bind(stime)
        .bind(note)
        .execute(test_pool(&pool))
        .await
        .expect("insert");
    }
    sqlx::query::<Sqlite>("INSERT INTO oc_users (uid, displayname) VALUES (?, ?)")
        .bind("bob")
        .bind("Robert")
        .execute(test_pool(&pool))
        .await
        .expect("user");
    let (details, notes) =
        share_details_and_notes_batch(&pool, prefix, "alice", &[10, 11, 12]).await;

    // Notes: max-stime non-empty note per file, filter-free (carol-note
    // lives on a share_type-5 row alice is not a party to).
    assert_eq!(notes.get(&10).map(String::as_str), Some("erin-note"));
    assert_eq!(notes.get(&11).map(String::as_str), Some("carol-note"));
    assert_eq!(notes.get(&12).map(String::as_str), Some("frank-note"));
    for id in [10, 11, 12] {
        assert_eq!(
            notes.get(&id).cloned().unwrap_or_default(),
            get_share_note(&pool, prefix, id).await,
            "note fileid {id}"
        );
    }

    // Details: same filter + display-name resolution as the single
    // query; SQL row order is unspecified, so compare as sorted sets.
    for id in [10, 11, 12] {
        let mut single = get_share_details(&pool, prefix, "alice", id).await;
        let mut batched = details.get(&id).cloned().unwrap_or_default();
        let key = |d: &ShareDetail| {
            (
                d.share_type,
                d.share_with.clone(),
                d.share_with_displayname.clone(),
            )
        };
        single.sort_by_key(key);
        batched.sort_by_key(key);
        assert_eq!(batched.len(), single.len(), "fileid {id} len");
        for (b, s) in batched.iter().zip(single.iter()) {
            assert_eq!(b.share_type, s.share_type, "fileid {id} type");
            assert_eq!(b.share_with, s.share_with, "fileid {id} with");
            assert_eq!(
                b.share_with_displayname, s.share_with_displayname,
                "fileid {id} displayname"
            );
        }
    }
    // The type-5 row and carol as owner/initiator never reach details.
    assert!(details.get(&11).unwrap().iter().all(|d| d.share_type != 5));
    // bob's user-share resolves the oc_users displayname; unknown dave
    // falls back to the uid.
    let t10 = details.get(&10).unwrap();
    assert!(t10.iter().any(|d| d.share_with_displayname == "Robert"));
    assert!(details
        .get(&12)
        .unwrap()
        .iter()
        .any(|d| d.share_with_displayname == "dave"));
}

#[tokio::test]
async fn comments_batches_match_singles() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    for (id, obj, actor, ts) in [
        (1, 10, "alice", "2024-01-01 10:00:00"),
        (2, 10, "bob", "2024-01-02 10:00:00"),
        (3, 10, "bob", "2024-01-03 10:00:00"),
        (4, 11, "alice", "2024-01-01 10:00:00"),
        (5, 12, "bob", "2024-01-05 10:00:00"),
    ] {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_comments (id, object_type, object_id, actor_type, actor_id, creation_timestamp) \
             VALUES (?, 'files', ?, 'users', ?, ?)",
        )
        .bind(id)
        .bind(obj.to_string())
        .bind(actor)
        .bind(ts)
        .execute(test_pool(&pool))
        .await
        .expect("insert");
    }
    // alice has read file 10 up to the day-02 marker: bob's day-03
    // comment is unread; her own comments are excluded either way.
    sqlx::query::<Sqlite>(
        "INSERT INTO oc_comments_read_markers (user_id, object_type, object_id, marker_datetime) \
         VALUES (?, 'files', ?, ?)",
    )
    .bind("alice")
    .bind("10")
    .bind("2024-01-02 10:00:00")
    .execute(test_pool(&pool))
    .await
    .expect("marker");
    let merged = comments_counts_batch(&pool, prefix, &[10, 11, 12], "alice").await;
    for id in [10, 11, 12] {
        let (c, u) = merged.get(&id).copied().unwrap_or((0, 0));
        assert_eq!(c, get_comments_count(&pool, prefix, id).await, "count {id}");
        assert_eq!(
            u,
            get_comments_unread(&pool, prefix, id, "alice").await,
            "unread {id}"
        );
    }
    assert_eq!(
        merged.get(&10),
        Some(&(3, 1)),
        "file 10: 3 comments, bob@day03 unread"
    );
    assert_eq!(
        merged.get(&11),
        Some(&(1, 0)),
        "file 11: alice's own comment, nothing unread"
    );
    assert_eq!(
        merged.get(&12),
        Some(&(1, 1)),
        "file 12: bob's comment, no marker"
    );
}

#[tokio::test]
async fn system_tags_batch_matches_single() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    for (id, name, vis) in [(1, "Beta", 1), (2, "alpha", 1), (3, "hidden", 0)] {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_systemtag (id, name, visibility, editable) VALUES (?, ?, ?, 1)",
        )
        .bind(id)
        .bind(name)
        .bind(vis)
        .execute(test_pool(&pool))
        .await
        .expect("tag");
    }
    for (obj, tag) in [("10", 1), ("10", 2), ("11", 3)] {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_systemtag_object_mapping (objectid, objecttype, systemtagid) \
             VALUES (?, 'files', ?)",
        )
        .bind(obj)
        .bind(tag)
        .execute(test_pool(&pool))
        .await
        .expect("map");
    }
    let batch = system_tags_batch(&pool, prefix, &[10, 11]).await;
    for id in [10, 11] {
        let b = batch.get(&id).cloned().unwrap_or_default();
        let s = get_system_tags_for_file(&pool, prefix, id).await;
        assert_eq!(b.len(), s.len(), "len {id}");
        for (x, y) in b.iter().zip(s.iter()) {
            assert_eq!(x.id, y.id, "id {id}");
            assert_eq!(x.name, y.name, "name {id}");
        }
    }
    // file 10: alpha then Beta (LOWER-sorted), hidden tag excluded; file
    // 11's only tag is hidden → absent from the batch map.
    let t10 = batch.get(&10).unwrap();
    assert_eq!(
        t10.iter().map(|t| t.name.as_str()).collect::<Vec<_>>(),
        vec!["alpha", "Beta"]
    );
    assert!(batch.get(&11).is_none());
}

#[tokio::test]
async fn custom_properties_batch_matches_single() {
    let pool = fresh_batch_db().await;
    let prefix = "oc_";
    for (p, name, val) in [
        ("files/a.txt", "{urn:x}one", "<v>1</v>"),
        ("files/a.txt", "{urn:x}two", "<v>2</v>"),
        ("files/b.txt", "{urn:x}one", "<v>3</v>"),
    ] {
        upsert_custom_property(&pool, prefix, "alice", p, name, val.as_bytes(), 1)
            .await
            .expect("upsert");
    }
    let paths = vec![
        "files/a.txt".to_string(),
        "files/b.txt".to_string(),
        "files/c.txt".to_string(),
    ];
    let batch = custom_properties_batch(&pool, prefix, "alice", &paths).await;
    for p in ["files/a.txt", "files/b.txt", "files/c.txt"] {
        let b = batch.get(p).cloned().unwrap_or_default();
        let s = list_custom_properties(&pool, prefix, "alice", p).await;
        assert_eq!(b, s, "{p}");
    }
    assert_eq!(batch.get("files/a.txt").unwrap().len(), 2);
    assert!(batch.get("files/c.txt").is_none());
}

// ── oc_properties CRUD smoke test (SQLite in-memory) ─────────────────

async fn fresh_props_db() -> DbPool {
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite"),
    );
    // Create the table matching 0013_properties.sql
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_properties (
            id            INTEGER NOT NULL PRIMARY KEY,
            userid        VARCHAR(64) NOT NULL DEFAULT '',
            propertypath  VARCHAR(255) NOT NULL DEFAULT '',
            propertyname  VARCHAR(255) NOT NULL DEFAULT '',
            propertyvalue TEXT NOT NULL DEFAULT '',
            valuetype     SMALLINT NOT NULL DEFAULT 1
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create table");
    sqlx::query::<Sqlite>("CREATE INDEX IF NOT EXISTS properties_path_uid ON oc_properties (userid, propertypath)")
        .execute(test_pool(&pool))
        .await
        .expect("create index");
    pool
}

#[tokio::test]
async fn custom_props_roundtrip_upsert_and_list() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";
    let xml = b"<ok xmlns=\"urn:example\">hello</ok>";

    // Insert
    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/notes.txt",
        "{urn:example}state",
        xml,
        2,
    )
    .await
    .expect("upsert");

    // Read back
    let props = list_custom_properties(&pool, prefix, "alice", "files/notes.txt").await;
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].0, "{urn:example}state");
    assert_eq!(props[0].1, "<ok xmlns=\"urn:example\">hello</ok>");
    assert_eq!(props[0].2, 2);
}

#[tokio::test]
async fn custom_props_upsert_overwrites_existing() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";

    // First write
    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/x.txt",
        "{urn:a}v",
        b"<a/>",
        2,
    )
    .await
    .expect("upsert 1");

    // Second write with different value
    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/x.txt",
        "{urn:a}v",
        b"<b/>",
        2,
    )
    .await
    .expect("upsert 2");

    // Should have exactly one row with the latest value
    let props = list_custom_properties(&pool, prefix, "alice", "files/x.txt").await;
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].1, "<b/>");
}

#[tokio::test]
async fn custom_props_delete_single() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";

    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/a.txt",
        "{urn:x}p",
        b"<p/>",
        2,
    )
    .await
    .expect("upsert");

    // Delete it
    delete_custom_property(&pool, prefix, "alice", "files/a.txt", "{urn:x}p")
        .await
        .expect("delete");

    let props = list_custom_properties(&pool, prefix, "alice", "files/a.txt").await;
    assert_eq!(props.len(), 0);
}

#[tokio::test]
async fn custom_props_delete_path_removes_all() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";

    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/b.txt",
        "{urn:a}p1",
        b"<p1/>",
        2,
    )
    .await
    .expect("upsert p1");
    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/b.txt",
        "{urn:a}p2",
        b"<p2/>",
        2,
    )
    .await
    .expect("upsert p2");

    // Delete all for this path
    delete_custom_properties_for_path(&pool, prefix, "alice", "files/b.txt")
        .await
        .expect("delete path");

    let props = list_custom_properties(&pool, prefix, "alice", "files/b.txt").await;
    assert_eq!(props.len(), 0);
}

#[tokio::test]
async fn custom_props_user_isolation() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";

    upsert_custom_property(
        &pool,
        prefix,
        "alice",
        "files/shared.txt",
        "{urn:x}p",
        b"<alice/>",
        2,
    )
    .await
    .expect("upsert alice");
    upsert_custom_property(
        &pool,
        prefix,
        "bob",
        "files/shared.txt",
        "{urn:x}p",
        b"<bob/>",
        2,
    )
    .await
    .expect("upsert bob");

    let alice_props = list_custom_properties(&pool, prefix, "alice", "files/shared.txt").await;
    assert_eq!(alice_props.len(), 1);
    assert_eq!(alice_props[0].1, "<alice/>");

    let bob_props = list_custom_properties(&pool, prefix, "bob", "files/shared.txt").await;
    assert_eq!(bob_props.len(), 1);
    assert_eq!(bob_props[0].1, "<bob/>");
}

#[tokio::test]
async fn custom_props_path_format_hashes_long_paths() {
    let pool = fresh_props_db().await;
    let prefix = "oc_";
    let long_path = "files/".to_string() + &"d".repeat(260);

    upsert_custom_property(&pool, prefix, "alice", &long_path, "{urn:x}p", b"<p/>", 2)
        .await
        .expect("upsert long");

    // The stored path should be hashed, but lookups use the same hash so
    // it round-trips correctly.
    let props = list_custom_properties(&pool, prefix, "alice", &long_path).await;
    assert_eq!(props.len(), 1);
    assert_eq!(props[0].1, "<p/>");
}

// ── Phase 12.3 / 12.4: permission masking pipeline ──────────────────────
//
// The only SHARE-bit stripping Rust performs is `apply_sharing_mask`, which
// mirrors PHP's SetupManager `sharing_mask` storage wrapper and fires ONLY
// when sharing is disabled via shareapi config.  In the normal (sharing
// enabled) case permissions pass through unchanged, so the home storage
// root keeps PERMISSION_SHARE — verified against live PHP: the home root
// reports `oc:permissions` = RGDNVCK, `ocs:share-permissions` = 31,
// `ocm:share-permissions` = ["share","read","write"].  (An earlier revision
// also stripped SHARE unconditionally on the mount root to match a stale
// cold-start capture; that strip was removed — see filesystem.rs.)

const P_READ: i32 = 1;
const P_UPDATE: i32 = 2;
const P_CREATE: i32 = 4;
const P_DELETE: i32 = 8;
const P_SHARE: i32 = 16;
const P_ALL: i32 = 31;

#[test]
fn apply_sharing_mask_passthrough_when_sharing_enabled() {
    // Master environment: shareapi_exclude_groups unset → sharing enabled →
    // the SetupManager sharing_mask wrapper is inactive, permissions pass
    // through unchanged.
    assert_eq!(apply_sharing_mask(P_ALL, false), P_ALL);
}

#[test]
fn apply_sharing_mask_strips_share_when_disabled() {
    // When sharing is disabled for the user, PermissionsMask(mask=15) wraps
    // the cache layer and strips PERMISSION_SHARE from every read.
    assert_eq!(apply_sharing_mask(P_ALL, true), P_ALL - P_SHARE);
    assert_eq!(apply_sharing_mask(P_ALL, true), 15);
}

#[test]
fn compute_share_permissions_mount_root_dir() {
    // A mount root whose effective permissions are 15 (SHARE absent — the
    // sharing-disabled case).  The mount-root DELETE|UPDATE OR-in is a no-op
    // (both bits already set).
    assert_eq!(compute_share_permissions(15, true, true), 15);
}

#[test]
fn compute_share_permissions_non_root_dir() {
    // Ordinary directory keeps its full permissions (SHARE included).
    assert_eq!(compute_share_permissions(P_ALL, true, false), P_ALL);
}

#[test]
fn compute_share_permissions_mount_root_gains_delete_update() {
    // A mount root that somehow lacked DELETE|UPDATE gains them (PHP
    // Node::getSharePermissions lines 261-275).
    let read_only = P_READ | P_CREATE; // 5
    assert_eq!(
        compute_share_permissions(read_only, true, true),
        P_READ | P_CREATE | P_DELETE | P_UPDATE
    );
}

#[test]
fn compute_share_permissions_file_strips_create_delete() {
    // Files can never carry CREATE or DELETE (PHP lines 280-282).
    assert_eq!(
        compute_share_permissions(P_ALL, false, false),
        P_ALL & !(P_CREATE | P_DELETE)
    );
    assert_eq!(compute_share_permissions(P_ALL, false, false), 19);
}

#[test]
fn permissions_to_ocm_json_without_share() {
    // 15 has no SHARE bit → "share" is dropped from the OCM array (PHP
    // FilesPlugin::ncPermissions2ocmPermissions).
    assert_eq!(permissions_to_ocm_json(15), r#"["read","write"]"#);
}

#[test]
fn permissions_to_ocm_json_with_share() {
    assert_eq!(
        permissions_to_ocm_json(P_ALL),
        r#"["share","read","write"]"#
    );
}

#[test]
fn home_root_permission_pipeline_matches_php() {
    // End-to-end composition for the home storage root, mirroring
    // `filesystem.rs::get_props`.  DB stores 31 and — sharing enabled — the
    // value passes through unchanged.  Verified against live PHP: the home
    // root returns RGDNVCK / ocs=31 / ocm=["share","read","write"].
    let db_permissions = P_ALL;
    let sharing_disabled = false; // master environment
    let is_mount_root = true;

    let effective = apply_sharing_mask(db_permissions, sharing_disabled);

    assert_eq!(effective, P_ALL, "home root keeps SHARE (→ RGDNVCK)");
    assert_eq!(
        compute_share_permissions(effective, true, is_mount_root),
        P_ALL
    );
    assert_eq!(
        permissions_to_ocm_json(effective),
        r#"["share","read","write"]"#
    );
}

#[test]
fn home_root_permission_pipeline_strips_share_when_sharing_disabled() {
    // When sharing is genuinely disabled (shareapi config), the mask strips
    // SHARE even on the home root → GDNVCK / ocs=15 / ocm=["read","write"].
    let effective = apply_sharing_mask(P_ALL, true);
    assert_eq!(effective, P_ALL - P_SHARE);
    assert_eq!(permissions_to_ocm_json(effective), r#"["read","write"]"#);
}

#[test]
fn non_root_dir_permission_pipeline_matches_php() {
    // Ordinary directory (e.g. "files/Photos"): SHARE is retained.  PHP
    // returns RGDNVCK / ocs=31 / ocm=["share","read","write"].
    let db_permissions = P_ALL;
    let sharing_disabled = false;
    let is_mount_root = false;

    let effective = apply_sharing_mask(db_permissions, sharing_disabled);

    assert_eq!(effective, P_ALL, "non-root keeps SHARE (→ RGDNVCK)");
    assert_eq!(
        compute_share_permissions(effective, true, is_mount_root),
        P_ALL
    );
    assert_eq!(
        permissions_to_ocm_json(effective),
        r#"["share","read","write"]"#
    );
}

// ── get_workspace_file (text app workspace, 2026-08-14) ────────────────

async fn workspace_db() -> DbPool {
    let pool = DbPool::Sqlite(
        sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory SQLite"),
    );
    sqlx::query::<Sqlite>(
        "CREATE TABLE oc_filecache (
            fileid BIGINT NOT NULL PRIMARY KEY, storage BIGINT NOT NULL,
            path VARCHAR(4000) NOT NULL DEFAULT '', parent BIGINT NOT NULL DEFAULT 0,
            name VARCHAR(250), mimetype BIGINT NOT NULL DEFAULT 0
        )",
    )
    .execute(test_pool(&pool))
    .await
    .expect("create oc_filecache");
    sqlx::query::<Sqlite>(
        "INSERT INTO oc_filecache (fileid, storage, path, parent, name, mimetype) VALUES              (1, 7, 'files/Media', 0, 'Media', 2),              (2, 7, 'files/Media/README.md', 1, 'README.md', 3),              (3, 7, 'files/Media/Readme.md', 1, 'Readme.md', 3),              (4, 7, 'files/Media/.Readme.md', 1, '.Readme.md', 3),              (5, 7, 'files/Media/Readme.md', 1, 'Readme.md', 2)",
    )
    .execute(test_pool(&pool))
    .await
    .expect("seed oc_filecache");
    pool
}

#[tokio::test]
async fn workspace_file_priority_and_dir_filter() {
    let pool = workspace_db().await;
    // Priority order: Readme.md beats README.md; the directory-named
    // row (mimetype 2 = dir) is skipped even though it matches first by
    // name; .Readme.md is the last resort.
    let got = get_workspace_file(&pool, "oc_", 1, 7, 2).await;
    assert_eq!(got, Some((3, "files/Media/Readme.md".to_string())));

    // No dir named Readme.md and no other match -> the .Readme.md fallback.
    sqlx::query::<Sqlite>("UPDATE oc_filecache SET name = 'X' WHERE fileid = 3")
        .execute(test_pool(&pool))
        .await
        .unwrap();
    let got = get_workspace_file(&pool, "oc_", 1, 7, 2).await;
    assert_eq!(got, Some((2, "files/Media/README.md".to_string())));
}

#[tokio::test]
async fn workspace_file_absent() {
    let pool = workspace_db().await;
    sqlx::query::<Sqlite>("DELETE FROM oc_filecache WHERE fileid IN (2, 3, 4, 5)")
        .execute(test_pool(&pool))
        .await
        .unwrap();
    assert_eq!(get_workspace_file(&pool, "oc_", 1, 7, 2).await, None);
}

// ── Subtree rekey / property subtree ops (phase 24) ──────────────────
//
// Unit tests only ever see the SQLite arm; it is the parity pin for the
// set-based Postgres arm, so these assert the exact `path`/`path_hash`/
// `propertypath` values both arms must produce.

/// The Postgres arms strip the old prefix with `SUBSTRING(x FROM $n)`,
/// which counts characters — Rust slices by bytes.  For a multi-byte
/// prefix the two only agree because the offset is a *character* count
/// (PHP's `mb_strlen`), which is what this pins.
#[test]
fn subtree_suffix_offset_counts_characters() {
    let prefix = "files/Ordner-Ü/日本";
    let path = format!("{prefix}/child.txt");
    let by_bytes = &path[prefix.len()..];
    let by_chars: String = path
        .chars()
        .skip(subtree_suffix_offset(prefix) as usize - 1)
        .collect();
    assert_eq!(by_chars, by_bytes);
    assert_ne!(subtree_suffix_offset(prefix) as usize - 1, prefix.len());
}

async fn seed_paths(pool: &DbPool, rows: &[(i64, &str)]) {
    for (fid, path) in rows {
        sqlx::query::<Sqlite>(
            "INSERT INTO oc_filecache (fileid, storage, path, path_hash, parent, name, mimetype) \
             VALUES ($1, 1, $2, $3, 0, '', 0)",
        )
        .bind(fid)
        .bind(path)
        .bind(path_hash(path))
        .execute(test_pool(pool))
        .await
        .unwrap();
    }
}

async fn path_of(pool: &DbPool, fileid: i64) -> (String, String) {
    sqlx::query_as::<Sqlite, (String, String)>(
        "SELECT path, path_hash FROM oc_filecache WHERE fileid = $1",
    )
    .bind(fileid)
    .fetch_one(test_pool(pool))
    .await
    .unwrap()
}

#[tokio::test]
async fn rekey_subtree_paths_rewrites_nested_descendants() {
    let pool = fresh_batch_db().await;
    seed_paths(
        &pool,
        &[
            (1, "files/dir"),
            (2, "files/dir/a.txt"),
            (3, "files/dir/sub"),
            (4, "files/dir/sub/deep"),
            (5, "files/dir/sub/deep/b.txt"),
            (6, "files/dirt.txt"),
        ],
    )
    .await;

    rekey_subtree_paths(&pool, "oc_", 1, "files/dir", "files/moved")
        .await
        .unwrap();

    for (fid, expected) in [
        (2i64, "files/moved/a.txt"),
        (3, "files/moved/sub"),
        (4, "files/moved/sub/deep"),
        (5, "files/moved/sub/deep/b.txt"),
    ] {
        let (path, hash) = path_of(&pool, fid).await;
        assert_eq!(path, expected);
        assert_eq!(hash, path_hash(expected));
    }
    // The subtree root is the caller's job, and a sibling sharing the
    // name prefix is not a descendant.
    assert_eq!(path_of(&pool, 1).await.0, "files/dir");
    assert_eq!(path_of(&pool, 6).await.0, "files/dirt.txt");
}

/// A path over `format_property_path`'s 250-byte threshold, so its
/// `propertypath` is a SHA-1 digest instead of the raw path.
fn long_child(parent: &str) -> String {
    format!("{parent}/{}", "x".repeat(260))
}

async fn prop_paths(pool: &DbPool) -> Vec<String> {
    sqlx::query_scalar::<Sqlite, String>("SELECT propertypath FROM oc_properties ORDER BY id")
        .fetch_all(test_pool(pool))
        .await
        .unwrap()
}

#[tokio::test]
async fn custom_properties_subtree_move_rekeys_hashed_and_raw_paths() {
    let pool = fresh_batch_db().await;
    let long_old = long_child("files/dir/sub");
    let long_new = long_child("files/moved/sub");
    seed_paths(
        &pool,
        &[
            (1, "files/dir"),
            (2, "files/dir/a.txt"),
            (3, "files/dir/sub"),
            (4, long_old.as_str()),
            (5, "files/other.txt"),
        ],
    )
    .await;
    for p in [
        "files/dir/a.txt",
        "files/dir/sub",
        &long_old,
        "files/other.txt",
    ] {
        upsert_custom_property(&pool, "oc_", "alice", p, "{urn:x}p", b"<p/>", 2)
            .await
            .unwrap();
    }

    update_custom_properties_path_subtree(&pool, "oc_", "alice", 1, "files/dir", "files/moved")
        .await;

    assert_eq!(
        prop_paths(&pool).await,
        vec![
            "files/moved/a.txt".to_string(),
            "files/moved/sub".to_string(),
            format_property_path(&long_new),
            "files/other.txt".to_string(),
        ]
    );
}

#[tokio::test]
async fn delete_custom_properties_for_dir_clears_hashed_and_raw_paths() {
    let pool = fresh_batch_db().await;
    let long_path = long_child("files/dir/sub");
    seed_paths(
        &pool,
        &[
            (1, "files/dir"),
            (2, "files/dir/a.txt"),
            (3, "files/dir/sub"),
            (4, long_path.as_str()),
            (5, "files/other.txt"),
        ],
    )
    .await;
    for p in [
        "files/dir",
        "files/dir/a.txt",
        "files/dir/sub",
        &long_path,
        "files/other.txt",
    ] {
        upsert_custom_property(&pool, "oc_", "alice", p, "{urn:x}p", b"<p/>", 2)
            .await
            .unwrap();
    }

    delete_custom_properties_for_dir(&pool, "oc_", "alice", 1, "files/dir").await;

    assert_eq!(prop_paths(&pool).await, vec!["files/other.txt".to_string()]);
}