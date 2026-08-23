use nc_db::db_dispatch;
use nc_db::pool::DbPool;
use sqlx::{Postgres, Sqlite};


/// Return the most recent non-empty share note for a file.
///
/// Query: `SELECT note FROM oc_share WHERE file_source = ? AND note != ''
/// ORDER BY stime DESC LIMIT 1`.
///
/// The file's `oc_files_metadata.json` (parsed) — `None` when the file has
/// no metadata row (directories never do).  PHP's FilesPlugin handles
/// `{nc:}metadata-{key}` per key of this row (2026-08-14).
pub async fn get_metadata_json(
    pool: &DbPool,
    prefix: &str,
    fileid: i64,
) -> Option<serde_json::Value> {
    let table = format!("{prefix}files_metadata");
    let sql = format!("SELECT json FROM {table} WHERE file_id = $1");
    let fetched: Option<String> = db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, String>(&sql)
            .bind(fileid)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
    });
    fetched.and_then(|j| serde_json::from_str(&j).ok())
}


/// The folder's workspace file — the first Readme* child, excluding
/// directories (text app `WorkspaceService::getSupportedFilenames`,
/// 2026-08-14: the localized "Readme".md first — en: "Readme.md" — then the
/// static list).  Returns `(fileid, fc-path)` of the first match.
pub async fn get_workspace_file(
    pool: &DbPool,
    prefix: &str,
    dir_fileid: i64,
    storage_id: i64,
    dir_mime_id: i64,
) -> Option<(i64, String)> {
    let names: [&str; 4] = ["Readme.md", "README.md", "readme.md", ".Readme.md"];
    let table = format!("{prefix}filecache");
    let sql = format!(
        "SELECT fileid, path, mimetype, name FROM {table} \
         WHERE parent = $1 AND storage = $2 AND name = ANY($3::text[])",
    );
    let rows: Vec<(i64, String, i64, String)> = match pool {
        DbPool::Pg(p) => sqlx::query_as::<Postgres, (i64, String, i64, String)>(&sql)
            .bind(dir_fileid)
            .bind(storage_id)
            .bind(&names)
            .fetch_all(p)
            .await
            .unwrap_or_default(),
        DbPool::Sqlite(p) => {
            let placeholders = (1..=names.len())
                .map(|i| format!("${i}"))
                .collect::<Vec<_>>()
                .join(",");
            let sql = format!(
                "SELECT fileid, path, mimetype, name FROM {table} \
                 WHERE parent = $1 AND storage = $2 AND name IN ({placeholders})",
            );
            let mut q = sqlx::query_as::<Sqlite, (i64, String, i64, String)>(&sql)
                .bind(dir_fileid)
                .bind(storage_id);
            for n in names {
                q = q.bind(n);
            }
            q.fetch_all(p).await.unwrap_or_default()
        }
    };
    // Priority order: the localized "Readme".md first (en: "Readme.md"),
    // then the static list — first non-directory match wins.
    for n in names {
        if let Some((fileid, path, _mimetype, _name)) = rows
            .iter()
            .find(|(_, _, m, nm)| m != &dir_mime_id && nm == n)
        {
            return Some((*fileid, path.clone()));
        }
    }
    None
}


/// One `oc_preferences` value (the text app's `workspace_enabled` gate;
/// default handled by the caller).
pub async fn get_user_preference(
    pool: &DbPool,
    prefix: &str,
    uid: &str,
    app: &str,
    key: &str,
) -> Option<String> {
    let table = format!("{prefix}preferences");
    let sql = format!(
        "SELECT configvalue FROM {table} WHERE userid = $1 AND appid = $2 AND configkey = $3"
    );
    db_dispatch!(pool, |Db, c| {
        sqlx::query_scalar::<Db, String>(&sql)
            .bind(uid)
            .bind(app)
            .bind(key)
            .fetch_optional(c)
            .await
            .ok()
            .flatten()
    })
}
